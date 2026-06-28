#!/usr/bin/env python3
from __future__ import annotations

import argparse
import concurrent.futures
import json
import os
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


HELPER_BASE_URL = "http://127.0.0.1:44750"
DEFAULT_PLACE_ID = "105986423068266"
DEFAULT_PROJECT = Path(r"D:\roblox_space\.codex-runtime-stop-local\workspace\default.project.json")
DEFAULT_ROJO = Path(r"D:\roblox_space\rojo\target\release\rojo.exe")
PLUGIN_PATH = Path(os.environ["LOCALAPPDATA"]) / "Roblox" / "Plugins" / "MCP2Plugin.rbxm"
ROBLOX_LOG_DIR = Path(os.environ["LOCALAPPDATA"]) / "Roblox" / "logs"


class StressError(RuntimeError):
    pass


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def run_command(args: list[str], *, cwd: Path, timeout: float) -> subprocess.CompletedProcess[str]:
    print("+ " + " ".join(args), flush=True)
    result = subprocess.run(
        args,
        cwd=str(cwd),
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
        timeout=timeout,
    )
    if result.returncode != 0:
        raise StressError(f"command failed ({result.returncode}): {' '.join(args)}\n{result.stdout}")
    if result.stdout.strip():
        print(result.stdout.strip(), flush=True)
    return result


def http_json(method: str, url: str, payload: dict[str, Any] | None = None, *, timeout: float = 10) -> Any:
    data = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        body = response.read()
    if not body:
        return None
    return json.loads(body.decode("utf-8"))


def expect_http_status(url: str, expected: int) -> None:
    try:
        urllib.request.urlopen(url, timeout=5).read()
    except urllib.error.HTTPError as exc:
        if exc.code != expected:
            raise StressError(f"expected HTTP {expected} from {url}, got {exc.code}") from exc
        return
    raise StressError(f"expected HTTP {expected} from {url}, got success")


def capture_task_screenshot(task_id: str) -> Any:
    return http_json("GET", f"{HELPER_BASE_URL}/session/{task_id}/studio/screenshot", timeout=40)


def latest_studio_diagnostics() -> str:
    candidates = sorted(ROBLOX_LOG_DIR.glob("*Studio*_last.log"), key=lambda p: p.stat().st_mtime, reverse=True)
    if not candidates:
        return "no Studio log files found"
    latest = candidates[0]
    text = latest.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    user_plugin_loaded = "user_MCP2Plugin.rbxm" in text
    login_seen = "www.roblox.com/login" in text or "LoginDialog" in text or "show login dialog" in text
    requested_lines = [
        line
        for line in lines
        if "plugins requested to load" in line
        or "user_MCP2Plugin.rbxm" in line
        or "www.roblox.com/login" in line
        or "LoginDialog" in line
        or "show login dialog" in line
    ]
    tail = "\n".join(requested_lines[-20:])
    return (
        f"latest_studio_log={latest}; "
        f"user_MCP2Plugin_loaded={user_plugin_loaded}; "
        f"login_seen={login_seen}\n{tail}"
    )


def mcp_raw(method: str, params: dict[str, Any] | None = None, *, timeout: float = 10) -> dict[str, Any]:
    body: dict[str, Any] = {
        "jsonrpc": "2.0",
        "id": int(time.time_ns() % 2_000_000_000),
        "method": method,
    }
    if params is not None:
        body["params"] = params
    return http_json("POST", f"{HELPER_BASE_URL}/mcp", body, timeout=timeout)


def mcp_tool(name: str, arguments: dict[str, Any], *, timeout: float = 120) -> tuple[bool, str, Any]:
    response = mcp_raw(
        "tools/call",
        {"name": name, "arguments": arguments},
        timeout=timeout,
    )
    if "error" in response:
        raise StressError(f"MCP RPC error for {name}: {response['error']}")
    result = response["result"]
    text = result["content"][0]["text"]
    if result.get("isError"):
        return True, text, None
    return False, text, json.loads(text)


def expect_mcp_error(name: str, arguments: dict[str, Any], contains: str) -> str:
    is_error, text, _payload = mcp_tool(name, arguments, timeout=12)
    if not is_error:
        raise StressError(f"expected MCP error from {name}, got {text}")
    if contains not in text:
        raise StressError(f"expected MCP error containing {contains!r}, got {text!r}")
    return text


def wait_until(label: str, timeout: float, fn) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception as exc:  # noqa: BLE001 - diagnostics are printed by caller.
            last_error = exc
        time.sleep(0.5)
    if last_error:
        raise StressError(f"timed out waiting for {label}: {last_error}") from last_error
    raise StressError(f"timed out waiting for {label}")


def wait_health() -> None:
    wait_until("helper health", 20, lambda: http_json("GET", f"{HELPER_BASE_URL}/healthz", timeout=2))


def wait_descriptor(workspace: Path) -> dict[str, Any]:
    descriptor_path = workspace / ".clock-p" / "session.json"

    def read_descriptor() -> dict[str, Any] | None:
        if not descriptor_path.exists():
            return None
        return json.loads(descriptor_path.read_text(encoding="utf-8"))

    return wait_until(f"descriptor {descriptor_path}", 20, read_descriptor)


def wait_mode(task_id: str, expected: str, *, timeout: float) -> dict[str, Any]:
    last_text = ""

    def read_mode() -> dict[str, Any] | None:
        nonlocal last_text
        is_error, text, payload = mcp_tool("helper2_studio_mode", {"task_id": task_id}, timeout=8)
        last_text = text
        if is_error:
            return None
        if payload.get("available") and payload.get("mode") == expected:
            return payload
        return None

    try:
        return wait_until(f"task {task_id} mode {expected}", timeout, read_mode)
    except StressError as exc:
        raise StressError(f"{exc}; last mode payload={last_text}") from exc


def parse_tool_payload(response: dict[str, Any]) -> dict[str, Any]:
    result = response["result"]
    if result.get("isError"):
        raise StressError(result["content"][0]["text"])
    return json.loads(result["content"][0]["text"])


def call_tool_for_executor(name: str, task_id: str) -> dict[str, Any]:
    response = mcp_raw(
        "tools/call",
        {"name": name, "arguments": {"task_id": task_id}},
        timeout=130,
    )
    return parse_tool_payload(response)


def start_helper_process(bin_dir: Path, logs_dir: Path, log_name: str) -> subprocess.Popen[str]:
    return start_process(
        [
            str(bin_dir / "studio-helper.exe"),
            "--addr",
            "127.0.0.1:44750",
            "--mcp2-stale-after",
            "20s",
            "--mcp2-stale-check-interval",
            "2s",
        ],
        logs_dir / log_name,
    )


def terminate_process(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=5)
        return
    except subprocess.TimeoutExpired:
        pass
    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def matching_test_processes() -> list[dict[str, Any]]:
    ps = (
        "Get-Process | Where-Object { $_.ProcessName -match "
        "'studio-helper|task-agent|RobloxStudioBeta' } | Select-Object Id,ProcessName,Path | ConvertTo-Json -Depth 3"
    )
    result = subprocess.run(
        ["powershell", "-NoProfile", "-Command", ps],
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    body = result.stdout.strip()
    if not body:
        return []
    payload = json.loads(body)
    if isinstance(payload, dict):
        return [payload]
    return payload


def cleanup_matching_processes() -> None:
    for process in matching_test_processes():
        pid = process.get("Id")
        if isinstance(pid, int):
            subprocess.run(["taskkill", "/PID", str(pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def ensure_no_existing_test_processes(*, kill_existing: bool) -> None:
    processes = matching_test_processes()
    if not processes:
        return
    if kill_existing:
        cleanup_matching_processes()
        return
    formatted = json.dumps(processes, ensure_ascii=False, indent=2)
    raise StressError(
        "existing Roblox Studio/helper/task-agent processes are running; "
        "close them first or pass --kill-existing for a dedicated test machine:\n"
        + formatted
    )


def start_process(args: list[str], log_path: Path) -> subprocess.Popen[str]:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log = log_path.open("w", encoding="utf-8")
    return subprocess.Popen(
        args,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
        creationflags=subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0,
    )


def prepare_binaries(root: Path, bin_dir: Path, rojo: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    go_helper = root / "go-helper"
    run_command(["go", "build", "-o", str(bin_dir / "studio-helper.exe"), r".\cmd\studio-helper"], cwd=go_helper, timeout=120)
    run_command(["go", "build", "-o", str(bin_dir / "task-agent.exe"), r".\cmd\task-agent"], cwd=go_helper, timeout=120)
    if not rojo.exists():
        raise StressError(f"rojo executable does not exist: {rojo}")
    run_command([str(rojo), "build", r"plugin-mcp2\default.project.json", "--plugin", "MCP2Plugin.rbxm"], cwd=root, timeout=60)
    if not PLUGIN_PATH.exists():
        raise StressError(f"MCP2 plugin was not installed at {PLUGIN_PATH}")


def stop_agent(bin_dir: Path, workspace: Path) -> None:
    if not (workspace / ".clock-p" / "session.json").exists():
        return
    subprocess.run(
        [str(bin_dir / "task-agent.exe"), "stop", "--workspace", str(workspace)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=10,
    )


def run_stress(args: argparse.Namespace) -> dict[str, Any]:
    root = repo_root()
    run_dir = Path(tempfile.mkdtemp(prefix="helper2-local-stress-"))
    bin_dir = run_dir / "bin"
    logs_dir = run_dir / "logs"
    work_a = run_dir / "work-a"
    work_b = run_dir / "work-b"
    work_a.mkdir(parents=True)
    work_b.mkdir(parents=True)
    helper: subprocess.Popen[str] | None = None
    agent_a: subprocess.Popen[str] | None = None
    agent_b: subprocess.Popen[str] | None = None

    prepare_binaries(root, bin_dir, args.rojo)
    print(f"run_dir={run_dir}", flush=True)
    ensure_no_existing_test_processes(kill_existing=args.kill_existing)

    try:
        helper = start_helper_process(bin_dir, logs_dir, "helper.log")
        wait_health()
        print(f"helper pid={helper.pid}", flush=True)

        agent_a = start_process(
            [
                str(bin_dir / "task-agent.exe"),
                "start",
                "--workspace",
                str(work_a),
                "--environment",
                "local",
                "--machine_name",
                "local-test-a",
                "--place_id",
                args.place_id,
                "--helper-base-url",
                HELPER_BASE_URL,
                "--rojo-bin",
                str(args.rojo),
                "--project",
                str(args.project),
            ],
            logs_dir / "agent-a.log",
        )
        desc_a = wait_descriptor(work_a)
        print(f"taskA={desc_a['task_id']} agentPid={agent_a.pid}", flush=True)
        try:
            mode_a = wait_mode(desc_a["task_id"], "edit", timeout=args.initial_mode_timeout)
        except Exception as exc:
            try:
                shot = capture_task_screenshot(desc_a["task_id"])
                print(f"initial taskA screenshot before failure: {shot}", flush=True)
            except Exception as shot_exc:
                print(f"initial taskA screenshot failed: {shot_exc}", flush=True)
            print("initial taskA studio diagnostics:\n" + latest_studio_diagnostics(), flush=True)
            raise exc
        print(f"taskA initial mode={mode_a['mode']}", flush=True)

        agent_b = start_process(
            [
                str(bin_dir / "task-agent.exe"),
                "start",
                "--workspace",
                str(work_b),
                "--environment",
                "local",
                "--machine_name",
                "local-test-b",
                "--place_id",
                args.place_id,
                "--helper-base-url",
                HELPER_BASE_URL,
                "--rojo-bin",
                str(args.rojo),
                "--project",
                str(args.project),
            ],
            logs_dir / "agent-b.log",
        )
        desc_b = wait_descriptor(work_b)
        print(f"taskB={desc_b['task_id']} agentPid={agent_b.pid}", flush=True)
        try:
            mode_b = wait_mode(desc_b["task_id"], "edit", timeout=args.initial_mode_timeout)
        except Exception as exc:
            try:
                shot = capture_task_screenshot(desc_b["task_id"])
                print(f"initial taskB screenshot before failure: {shot}", flush=True)
            except Exception as shot_exc:
                print(f"initial taskB screenshot failed: {shot_exc}", flush=True)
            print("initial taskB studio diagnostics:\n" + latest_studio_diagnostics(), flush=True)
            raise exc
        print(f"taskB initial mode={mode_b['mode']}", flush=True)

        tools = [tool["name"] for tool in mcp_raw("tools/list")["result"]["tools"]]
        for required in ("helper2_studio_play", "helper2_studio_stop", "helper2_studio_mode", "helper2_studio_screenshot"):
            if required not in tools:
                raise StressError(f"tools/list missing {required}")
        for forbidden in ("launch_studio_session", "start_stop_play", "take_screenshot", "runtime_log_read"):
            if forbidden in tools:
                raise StressError(f"tools/list still exposes legacy tool {forbidden}")
        print("tools/list new-name check ok", flush=True)

        old_error = expect_mcp_error("launch_studio_session", {"task_id": desc_a["task_id"]}, "unknown helper2 MCP tool")
        bad_task_error = expect_mcp_error("helper2_studio_screenshot", {"task_id": "not-a-real-task"}, "no registered session")
        bad_mode_error = expect_mcp_error(
            "helper2_studio_stop",
            {"task_id": desc_a["task_id"], "mode": "start_play"},
            "supports stop only",
        )
        expect_http_status(f"{HELPER_BASE_URL}/v1/rojo/config?place_id={args.place_id}", 403)
        expect_http_status(f"{HELPER_BASE_URL}/plugin/mcp2/pull_command?mode=edit&mode_seq=1", 403)
        print(
            "negative checks ok: "
            f"old={old_error}; bad_task={bad_task_error}; bad_mode={bad_mode_error}",
            flush=True,
        )

        shot_a = None
        shot_b = None
        for cycle in range(1, args.cycles + 1):
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                future_play_a = executor.submit(call_tool_for_executor, "helper2_studio_play", desc_a["task_id"])
                future_play_b = executor.submit(call_tool_for_executor, "helper2_studio_play", desc_b["task_id"])
                mode_samples: list[str] = []
                for _ in range(12):
                    mode_samples.append(mcp_tool("helper2_studio_mode", {"task_id": desc_a["task_id"]}, timeout=8)[1])
                    mode_samples.append(mcp_tool("helper2_studio_mode", {"task_id": desc_b["task_id"]}, timeout=8)[1])
                    time.sleep(0.5)
                play_a = future_play_a.result(timeout=150)
                play_b = future_play_b.result(timeout=150)
            print(
                f"cycle {cycle}/{args.cycles} concurrent play ok "
                f"A={play_a.get('mode')} B={play_b.get('mode')} samples={len(mode_samples)}",
                flush=True,
            )
            wait_mode(desc_a["task_id"], "play_server", timeout=30)
            wait_mode(desc_b["task_id"], "play_server", timeout=30)

            if cycle == 1 or cycle == args.cycles:
                shot_a = mcp_tool("helper2_studio_screenshot", {"task_id": desc_a["task_id"]}, timeout=40)[2]
                shot_b = mcp_tool("helper2_studio_screenshot", {"task_id": desc_b["task_id"]}, timeout=40)[2]
                print(f"cycle {cycle} screenshots ok A={shot_a['screenshot']} B={shot_b['screenshot']}", flush=True)

            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                future_stop_a1 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_a["task_id"])
                time.sleep(0.1)
                future_stop_a2 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_a["task_id"])
                future_stop_b1 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_b["task_id"])
                time.sleep(0.1)
                future_stop_b2 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_b["task_id"])
                stop_a1 = future_stop_a1.result(timeout=150)
                stop_a2 = future_stop_a2.result(timeout=150)
                stop_b1 = future_stop_b1.result(timeout=150)
                stop_b2 = future_stop_b2.result(timeout=150)
            print(
                f"cycle {cycle}/{args.cycles} duplicate/concurrent stop ok "
                f"A1={stop_a1.get('mode')} A2={stop_a2.get('mode')} "
                f"B1={stop_b1.get('mode')} B2={stop_b2.get('mode')}",
                flush=True,
            )
            wait_mode(desc_a["task_id"], "edit", timeout=30)
            wait_mode(desc_b["task_id"], "edit", timeout=30)

        if args.helper_restart:
            print("terminating helper to simulate local helper/network outage", flush=True)
            old_helper_pid = helper.pid
            terminate_process(helper)
            helper = None
            time.sleep(8)
            helper = start_helper_process(bin_dir, logs_dir, "helper-restarted.log")
            wait_health()
            print(f"helper restarted old_pid={old_helper_pid} new_pid={helper.pid}", flush=True)
            wait_mode(desc_a["task_id"], "edit", timeout=150)
            wait_mode(desc_b["task_id"], "edit", timeout=150)
            print("helper restart recovery ok: both task sessions returned to edit", flush=True)

        subprocess.run(["taskkill", "/PID", str(agent_b.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        agent_b = None
        print("killed task-agent B; waiting lease expiry", flush=True)
        time.sleep(38)
        status_b = http_json("GET", f"{HELPER_BASE_URL}/session/{desc_b['task_id']}/status", timeout=10)
        if status_b.get("state") != "expired":
            raise StressError(f"expected task B expired after killed agent, got {status_b}")
        summary = http_json("GET", f"{HELPER_BASE_URL}/studio/summary", timeout=10)
        b_studios = [studio for studio in summary.get("studios", []) if studio.get("owner_id") == desc_b["task_id"]]
        if b_studios:
            raise StressError(f"expected no task B studios after expiry, got {b_studios}")
        mode_a_after = wait_mode(desc_a["task_id"], "edit", timeout=30)
        print(f"lease expiry isolation ok B={status_b['state']} A={mode_a_after['mode']}", flush=True)

        return {
            "ok": True,
            "run_dir": str(run_dir),
            "taskA": desc_a["task_id"],
            "taskB": desc_b["task_id"],
            "screenshotA": shot_a["screenshot"],
            "screenshotB": shot_b["screenshot"],
            "summary": summary,
        }
    finally:
        stop_agent(bin_dir, work_a)
        stop_agent(bin_dir, work_b)
        terminate_process(agent_a)
        terminate_process(agent_b)
        terminate_process(helper)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run local helper2/task-agent/plugin-mcp2 Studio stress test.")
    parser.add_argument("--place-id", default=DEFAULT_PLACE_ID)
    parser.add_argument("--project", type=Path, default=DEFAULT_PROJECT)
    parser.add_argument("--rojo", type=Path, default=DEFAULT_ROJO)
    parser.add_argument("--initial-mode-timeout", type=float, default=150)
    parser.add_argument("--kill-existing", action="store_true", help="Kill existing Studio/helper/task-agent processes before the stress test.")
    parser.add_argument("--cycles", type=int, default=1, help="Number of concurrent play/stop cycles to run before lease-expiry checks.")
    parser.add_argument("--helper-restart", action="store_true", help="Restart helper while task-agents stay alive, then wait for heartbeat recovery.")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        result = run_stress(args)
    except Exception as exc:  # noqa: BLE001 - CLI should report concise failure.
        print(f"STRESS_FAILED: {exc}", file=sys.stderr, flush=True)
        return 1
    print("STRESS_OK " + json.dumps(result, ensure_ascii=False, default=str), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
