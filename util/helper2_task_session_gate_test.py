from __future__ import annotations

import argparse
import ctypes
import json
import os
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
GO_HELPER = ROOT / "go-helper"
MACHINE_NAME = "task-session-win"

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
PROCESS_TERMINATE = 0x0001
STILL_ACTIVE = 259


class TaskSessionGateError(RuntimeError):
    pass


def process_is_running(pid: int) -> bool:
    if pid <= 0:
        return False
    handle = ctypes.windll.kernel32.OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, False, pid)
    if not handle:
        return False
    try:
        exit_code = ctypes.c_ulong()
        ok = ctypes.windll.kernel32.GetExitCodeProcess(handle, ctypes.byref(exit_code))
        return bool(ok) and exit_code.value == STILL_ACTIVE
    finally:
        ctypes.windll.kernel32.CloseHandle(handle)


def terminate_pid(pid: int) -> None:
    handle = ctypes.windll.kernel32.OpenProcess(PROCESS_TERMINATE, False, pid)
    if not handle:
        return
    try:
        ctypes.windll.kernel32.TerminateProcess(handle, 1)
    finally:
        ctypes.windll.kernel32.CloseHandle(handle)


def wait_until(label: str, timeout: float, fn) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception as exc:  # noqa: BLE001
            last_error = exc
        time.sleep(0.25)
    if last_error is not None:
        raise TaskSessionGateError(f"timed out waiting for {label}: {last_error}") from last_error
    raise TaskSessionGateError(f"timed out waiting for {label}")


def wait_process_exit(pid: int, timeout: float) -> None:
    wait_until(f"process {pid} exit", timeout, lambda: not process_is_running(pid))


def free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def run_command(args: list[str], *, cwd: Path, timeout: float) -> None:
    completed = subprocess.run(args, cwd=cwd, text=True, capture_output=True, timeout=timeout)
    if completed.returncode != 0:
        raise TaskSessionGateError(
            f"command failed ({completed.returncode}): {' '.join(args)}\n"
            f"stdout={completed.stdout}\nstderr={completed.stderr}"
        )


def http_json(
    method: str,
    url: str,
    payload: dict[str, Any] | None = None,
    *,
    expected: int = 200,
    task_token: str | None = None,
) -> dict[str, Any]:
    data = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    if task_token is not None:
        headers["X-ClockP-Task-Token"] = task_token
    request = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(request, timeout=5) as response:
            body = response.read().decode("utf-8")
            if response.status != expected:
                raise TaskSessionGateError(f"{method} {url} returned HTTP {response.status}, want {expected}: {body}")
            return json.loads(body)
    except urllib.error.HTTPError as exc:
        body = exc.read().decode("utf-8")
        if exc.code != expected:
            raise TaskSessionGateError(f"{method} {url} returned HTTP {exc.code}, want {expected}: {body}") from exc
        return json.loads(body)


def log_tail(path: Path, limit: int = 6000) -> str:
    if not path.is_file():
        return ""
    return path.read_text(encoding="utf-8", errors="replace")[-limit:]


def start_helper(helper_bin: Path, run_root: Path, name: str, check_interval: str = "5s") -> tuple[subprocess.Popen[str], str, Path]:
    port = free_port()
    log_path = run_root / "logs" / f"{name}.log"
    log_path.parent.mkdir(parents=True, exist_ok=True)
    fake_studio = run_root / "fake-RobloxStudioBeta.exe"
    fake_studio.write_text("not a real executable\n", encoding="utf-8")
    env = dict(os.environ)
    env["CLOCK_P_STUDIO_PATH"] = str(fake_studio)
    log = log_path.open("w", encoding="utf-8")
    process = subprocess.Popen(
        [
            str(helper_bin),
            "-addr",
            f"127.0.0.1:{port}",
            "-auto-start-place-id=",
            "-mcp2-stale-check-interval",
            check_interval,
        ],
        cwd=ROOT,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
        env=env,
    )
    base_url = f"http://127.0.0.1:{port}"

    def healthy() -> dict[str, Any] | None:
        if process.poll() is not None:
            raise TaskSessionGateError(f"helper exited early with code {process.returncode}; log_tail={log_tail(log_path)!r}")
        payload = http_json("GET", base_url + "/healthz")
        return payload if payload.get("ok") else None

    wait_until(f"{name} healthz", 10, healthy)
    return process, base_url, log_path


def stop_helper(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    terminate_pid(process.pid)
    wait_process_exit(process.pid, 10)


def task_token(task_id: str) -> str:
    return f"token-{task_id}"


def code_sync_binding(task_id: str, *, place_id: str, config_hash: str | None = None) -> dict[str, Any]:
    return {
        "protocol_version": 2,
        "workspace_id": f"workspace-{task_id}",
        "place_id": place_id,
        "machine_name": MACHINE_NAME,
        "mapping_profile": "sync_lua_v1",
        "code_sync_config_hash": config_hash or f"config-{task_id}",
        "roots_authority_hash": f"roots-{task_id}",
        "roots": [
            {
                "root_id": "app",
                "studio_path": ["ReplicatedStorage", "ClockPTaskSession"],
            }
        ],
    }


def heartbeat(
    base_url: str,
    task_id: str,
    *,
    pid: int,
    started_at: int,
    place_id: str,
    config_hash: str | None = None,
    expected: int = 200,
) -> dict[str, Any]:
    return http_json(
        "POST",
        f"{base_url}/session/{task_id}/heartbeat",
        {
            "task_id": task_id,
            "machine_name": MACHINE_NAME,
            "place_id": place_id,
            "task_agent_pid": pid,
            "task_agent_started_at_ms": started_at,
            "task_session_token": task_token(task_id),
            "code_sync": code_sync_binding(task_id, place_id=place_id, config_hash=config_hash),
        },
        expected=expected,
    )


def release(base_url: str, task_id: str, *, pid: int, started_at: int, expected: int = 200) -> dict[str, Any]:
    return http_json(
        "POST",
        f"{base_url}/session/{task_id}/release",
        {
            "task_agent_pid": pid,
            "task_agent_started_at_ms": started_at,
        },
        expected=expected,
        task_token=task_token(task_id),
    )


def status(base_url: str, task_id: str, *, expected: int = 200, token: str | None = None) -> dict[str, Any]:
    return http_json(
        "GET",
        f"{base_url}/session/{task_id}/status",
        expected=expected,
        task_token=task_token(task_id) if token is None else token,
    )


def desired_owner_ids(payload: dict[str, Any]) -> list[str]:
    return sorted(item.get("owner_id", "") for item in payload.get("desired_studio", []))


def require(condition: bool, message: str) -> None:
    if not condition:
        raise TaskSessionGateError(message)


def run_task_session_gate(args: argparse.Namespace) -> dict[str, Any]:
    run_root = Path(tempfile.mkdtemp(prefix="helper2-task-session-"))
    bin_dir = run_root / "bin"
    bin_dir.mkdir(parents=True)
    helper_bin = bin_dir / "studio-helper.exe"
    helpers: list[subprocess.Popen[str]] = []

    def beat(base_url: str, task_id: str, *, pid: int, started_at: int, config_hash: str | None = None, expected: int = 200) -> dict[str, Any]:
        return heartbeat(base_url, task_id, pid=pid, started_at=started_at, place_id=args.place_id, config_hash=config_hash, expected=expected)

    try:
        run_command(["go", "build", "-o", str(helper_bin), r".\cmd\studio-helper"], cwd=GO_HELPER, timeout=120)

        helper1, base1, log1 = start_helper(helper_bin, run_root, "helper1", check_interval=args.check_interval)
        helpers.append(helper1)

        hb_a = beat(base1, "task-a", pid=40101, started_at=1700000001001)
        require(hb_a.get("state") == "live", f"task A heartbeat failed: {hb_a}")
        hb_b = beat(base1, "task-b", pid=40102, started_at=1700000001002)
        require(hb_b.get("state") == "live", f"task B heartbeat failed: {hb_b}")
        hb_a_again = beat(base1, "task-a", pid=40101, started_at=1700000001001)
        require(hb_a_again.get("state") == "live", f"task A idempotent heartbeat failed: {hb_a_again}")

        status_missing_token = http_json("GET", f"{base1}/session/task-a/status", expected=401)
        require(status_missing_token.get("code") == "task_token_missing", f"missing token was not rejected: {status_missing_token}")
        status_wrong_token = status(base1, "task-a", expected=403, token="wrong-token")
        require(status_wrong_token.get("code") == "task_token_mismatch", f"wrong token was not rejected: {status_wrong_token}")

        status_a = status(base1, "task-a")
        status_b = status(base1, "task-b")
        require(status_a.get("state") == "live", f"task A is not live: {status_a}")
        require(status_b.get("state") == "live", f"task B is not live: {status_b}")
        require(desired_owner_ids(status_a) == ["task-a"], f"task A desired state is not task-owned: {status_a}")
        require(desired_owner_ids(status_b) == ["task-b"], f"task B desired state is not task-owned: {status_b}")
        contract_a = status_a.get("contract") or {}
        require("task_session_token" not in contract_a, f"status leaked task token: {status_a}")
        require((contract_a.get("code_sync") or {}).get("roots_authority_hash") == "roots-task-a", f"task A code_sync missing: {status_a}")

        mismatch = beat(base1, "task-a", pid=40101, started_at=1700000001001, config_hash="changed-config", expected=409)
        require(mismatch.get("code") == "immutable_mismatch", f"code_sync mutation was not rejected: {mismatch}")

        released_b = release(base1, "task-b", pid=40102, started_at=1700000001002)
        require(released_b.get("state") == "ended", f"task B release failed: {released_b}")
        ended_b = status(base1, "task-b")
        require(ended_b.get("state") == "ended", f"task B status is not ended: {ended_b}")
        require(desired_owner_ids(ended_b) == [], f"task B desired survived release: {ended_b}")
        post_release_hb = beat(base1, "task-b", pid=40102, started_at=1700000001002, expected=409)
        require(post_release_hb.get("code") == "task_ended", f"task B heartbeat after release was not rejected: {post_release_hb}")
        status_a_after_b = status(base1, "task-a")
        require(status_a_after_b.get("state") == "live", f"task A changed after task B release: {status_a_after_b}")
        require(desired_owner_ids(status_a_after_b) == ["task-a"], f"task A desired mutated by task B release: {status_a_after_b}")

        beat(base1, "task-c", pid=40103, started_at=1700000001003)
        wait_for_task_c_expiry_while_refreshing_a(base1, args.expiry_timeout, args.place_id)
        expired_c = status(base1, "task-c")
        require(desired_owner_ids(expired_c) == [], f"task C desired survived expiry: {expired_c}")
        recovered_c = beat(base1, "task-c", pid=40103, started_at=1700000001003)
        require(recovered_c.get("state") == "live", f"task C did not recover after expiry: {recovered_c}")
        status_c = status(base1, "task-c")
        require(desired_owner_ids(status_c) == ["task-c"], f"task C desired was not restored: {status_c}")
        status_a_after_c = status(base1, "task-a")
        require(status_a_after_c.get("state") == "live", f"task A changed after task C expiry/recovery: {status_a_after_c}")

        stop_helper(helper1)
        helpers.remove(helper1)

        helper2, base2, log2 = start_helper(helper_bin, run_root, "helper2", check_interval=args.check_interval)
        helpers.append(helper2)
        status(base2, "task-a", expected=404)
        beat(base2, "task-a", pid=40101, started_at=1700000001001)
        restored_a = status(base2, "task-a")
        require(restored_a.get("state") == "live", f"task A did not restore after helper restart: {restored_a}")
        require(desired_owner_ids(restored_a) == ["task-a"], f"helper restart restored unexpected desired state: {restored_a}")
        status(base2, "task-b", expected=404)

        return {
            "ok": True,
            "run_dir": str(run_root),
            "helper1": {"url": base1, "log": str(log1)},
            "helper2": {"url": base2, "log": str(log2)},
            "tasks": ["task-a", "task-b", "task-c"],
        }
    except Exception:
        print(f"task-session gate run dir retained for diagnostics: {run_root}", file=sys.stderr)
        raise
    finally:
        for helper in list(helpers):
            stop_helper(helper)


def wait_for_task_c_expiry_while_refreshing_a(base_url: str, timeout: float, place_id: str) -> None:
    deadline = time.monotonic() + timeout
    next_a_heartbeat = 0.0
    while time.monotonic() < deadline:
        now = time.monotonic()
        if now >= next_a_heartbeat:
            heartbeat(base_url, "task-a", pid=40101, started_at=1700000001001, place_id=place_id)
            next_a_heartbeat = now + 5.0
        payload = status(base_url, "task-c")
        if payload.get("state") == "expired":
            return
        time.sleep(0.5)
    raise TaskSessionGateError("timed out waiting for task C expiry while refreshing task A")


def main() -> int:
    parser = argparse.ArgumentParser(description="Run helper2 code-sync task-session gate.")
    parser.add_argument("--place-id", required=True, help="Roblox place id supplied by the tester")
    parser.add_argument("--check-interval", default="250ms")
    parser.add_argument("--expiry-timeout", type=float, default=40.0)
    args = parser.parse_args()
    try:
        result = run_task_session_gate(args)
    except Exception as exc:  # noqa: BLE001
        print(f"TASK_SESSION_GATE_FAIL {exc}", file=sys.stderr)
        return 1
    print("TASK_SESSION_GATE_OK " + json.dumps(result, ensure_ascii=False, default=str))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
