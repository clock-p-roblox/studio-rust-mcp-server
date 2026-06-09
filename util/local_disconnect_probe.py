#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import threading
import time
import urllib.request
from pathlib import Path
from typing import Any


REPO = Path(__file__).resolve().parents[1]
PLATFORM_BRIDGE = REPO.parent / "clock-p-platform" / "tools" / "bridge"
sys.path.insert(0, str(PLATFORM_BRIDGE))

from mcp_client import McpClient, extract_text_content, require_non_error_result  # type: ignore


HUB_URL = "http://127.0.0.1:44758"
HELPER_URL = "http://127.0.0.1:44750"
SERVER_URL = "http://127.0.0.1:44756"
SERVER_PROXY_URL = "http://127.0.0.1:44760"
ROJO_URL = "http://127.0.0.1:44757"
HELPER_ID = "h_local_disconnect"
PLACE_ID = "134795435066737"
UNIVERSE_ID = "9327304100"
TOKEN = "local-test-token"


def request_json(url: str, *, method: str = "GET", payload: dict[str, Any] | None = None, timeout: float = 10) -> Any:
    data = None
    headers = {"Content-Type": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as response:
        body = response.read()
    return json.loads(body.decode("utf-8")) if body else None


def wait_until(label: str, predicate, timeout_sec: float = 90.0, interval_sec: float = 0.25):
    deadline = time.monotonic() + timeout_sec
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except Exception as exc:  # noqa: BLE001
            last_error = exc
        time.sleep(interval_sec)
    raise RuntimeError(f"timed out waiting for {label}; last_error={last_error}")


def start_process(name: str, args: list[str], log_dir: Path) -> subprocess.Popen:
    log_file = (log_dir / f"{name}.log").open("w", encoding="utf-8")
    env = dict(**__import__("os").environ)
    env.setdefault("RUST_LOG", "info")
    return subprocess.Popen(args, cwd=REPO, stdout=log_file, stderr=subprocess.STDOUT, env=env, text=True)


def start_proxy(log_dir: Path) -> subprocess.Popen:
    return start_process(
        "server-proxy",
        [
            sys.executable,
            str(REPO / "util" / "tcp_proxy.py"),
            "--listen-port",
            "44760",
            "--target-port",
            "44756",
        ],
        log_dir,
    )


def wait_http(url: str):
    return wait_until(url, lambda: request_json(url), timeout_sec=30)


def wait_http_bytes(url: str):
    def probe():
        with urllib.request.urlopen(url, timeout=10) as response:
            return response.status < 500

    return wait_until(url, probe, timeout_sec=30)


def task_heartbeat_loop(task_id: str, task_token: str, stop_event: threading.Event):
    payload = {
        "task_id": task_id,
        "task_token": task_token,
        "service_state": "ready",
        "accepting_launches": True,
        "routes": {
            "rojo_base_url": ROJO_URL,
            "mcp_base_url": SERVER_PROXY_URL,
            "runtime_log_base_url": "http://127.0.0.1:44759",
        },
        "services": {
            "rojo": "healthy",
            "mcp": "healthy",
            "runtime_log": "healthy",
            "rojo_public": "healthy",
            "mcp_public": "healthy",
            "runtime_log_public": "healthy",
        },
    }
    while not stop_event.is_set():
        try:
            request_json(f"{HUB_URL}/v1/tasks/heartbeat", method="POST", payload=payload)
        except Exception as exc:  # noqa: BLE001
            print(f"[disconnect-probe] task heartbeat failed: {exc}", flush=True)
        stop_event.wait(2.0)


def helper_task(task_id: str) -> dict[str, Any]:
    return request_json(f"{HELPER_URL}/v1/debug/tasks/{task_id}")


def server_state() -> dict[str, Any]:
    return request_json(f"{SERVER_URL}/v1/debug/state")


def remote_state(task_id: str) -> str | None:
    payload = helper_task(task_id)
    connection = payload.get("remote_connection") or {}
    return connection.get("state")


def wait_triplet(task_id: str, mode: str, control: str, phase: str) -> dict[str, Any]:
    def sample():
        hub_status = request_json(f"{HUB_URL}/status")
        hub_task = None
        for helper in hub_status.get("helpers", []):
            if helper.get("helper_id") != HELPER_ID:
                continue
            for task in helper.get("active_tasks", []):
                if task.get("task_id") == task_id:
                    hub_task = task
                    break
        helper_snapshot = helper_task(task_id).get("task_status") or {}
        server_task_status = ((server_state().get("active_helper") or {}).get("task_status") or {})
        observed = {"hub": hub_task, "helper": helper_snapshot, "server": server_task_status}
        if (
            hub_task
            and hub_task.get("studio_mode") == mode
            and hub_task.get("studio_control_state") == control
            and hub_task.get("studio_transition_phase") == phase
            and helper_snapshot.get("studio_mode") == mode
            and helper_snapshot.get("studio_control_state") == control
            and helper_snapshot.get("studio_transition_phase") == phase
            and server_task_status.get("studio_mode") == mode
            and server_task_status.get("studio_control_state") == control
            and server_task_status.get("studio_transition_phase") == phase
        ):
            return observed
        return None

    return wait_until(f"hub/helper/task-server {mode}/{control}/{phase}", sample, timeout_sec=10)


def call_tool(client: McpClient, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    result, _diagnostics = client.call_tool_with_diagnostics(name, arguments)
    result = require_non_error_result(name, result)
    text = extract_text_content(result)
    try:
        return json.loads(text) if text else {}
    except json.JSONDecodeError:
        return {"text": text}


def run_preflight(task_id: str) -> None:
    preflight = subprocess.run(
        [
            sys.executable,
            str(REPO / "util" / "studio_debug_preflight.py"),
            "--hub-url",
            HUB_URL,
            "--helper-url",
            HELPER_URL,
            "--server-url",
            SERVER_URL,
            "--task-id",
            task_id,
            "--helper-id",
            HELPER_ID,
            "--place-id",
            PLACE_ID,
            "--require-helper-active-task",
            "--require-plugin",
            "--require-server-helper",
            "--check-studio-log",
            "--rojo-url",
            ROJO_URL,
            "--timeout-sec",
            "0.75",
            "--max-elapsed-ms",
            "1000",
        ],
        cwd=REPO,
        text=True,
        capture_output=True,
        timeout=3,
    )
    print(preflight.stdout.strip(), flush=True)
    if preflight.returncode != 0:
        print(preflight.stderr, flush=True)
        raise RuntimeError("preflight failed")


def stop_process(proc: subprocess.Popen | None) -> None:
    if proc is None or proc.poll() is not None:
        return
    proc.terminate()
    try:
        proc.wait(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--disconnect-seconds", type=float, default=120.0)
    parser.add_argument("--cycles-before", type=int, default=2)
    parser.add_argument("--cycles-after", type=int, default=2)
    args = parser.parse_args()

    log_dir = REPO / ".tmp" / "local-disconnect-probe"
    log_dir.mkdir(parents=True, exist_ok=True)
    state_file = log_dir / "hub-state.json"
    if state_file.exists():
        state_file.unlink()

    procs: list[subprocess.Popen] = []
    proxy_proc: subprocess.Popen | None = None
    heartbeat_stop = threading.Event()
    heartbeat_thread: threading.Thread | None = None
    task_id = ""

    try:
        hub_exe = str(REPO / "target" / "release" / "roblox_hub.exe")
        server_exe = str(REPO / "target" / "release" / "rbx-studio-mcp.exe")
        helper_exe = str(REPO / "target" / "release" / "studio_helper.exe")
        rojo_exe = REPO.parent / "rojo" / "target" / "release" / "rojo.exe"
        if not rojo_exe.exists():
            raise RuntimeError(f"missing sibling Rojo release binary: {rojo_exe}")

        procs.append(start_process("hub", [hub_exe, "--no-auth", "--state-file", str(state_file), "--heartbeat-interval-sec", "2"], log_dir))
        wait_http(f"{HUB_URL}/status")

        created = request_json(
            f"{HUB_URL}/v1/tasks/create",
            method="POST",
            payload={
                "cluster_key": "local-disconnect-probe",
                "owner_user": "local-probe",
                "repo": "game1",
                "worktree_name": "local-disconnect-probe",
                "place_id": PLACE_ID,
                "universe_id": UNIVERSE_ID,
            },
        )
        task_id = created["task_id"]

        rojo_project_dir = log_dir / "rojo-project"
        rojo_project_dir.mkdir(parents=True, exist_ok=True)
        rojo_project = rojo_project_dir / "default.project.json"
        rojo_project.write_text(
            json.dumps(
                {
                    "name": "local-disconnect-probe",
                    "tree": {
                        "$className": "DataModel",
                        "Workspace": {"$className": "Workspace"},
                    },
                },
                indent=2,
            ),
            encoding="utf-8",
        )
        procs.append(start_process("rojo", [str(rojo_exe), "serve", str(rojo_project), "--port", "44757", "--task-id", task_id, "--workspace-path", str(REPO), "--public-base-url", ROJO_URL], log_dir))
        wait_http_bytes(f"{ROJO_URL}/api/rojo")

        procs.append(start_process("server", [server_exe, "--http", "--no-auth", "--task-id", task_id, "--workspace-path", str(REPO), "--hub-base-url", HUB_URL], log_dir))
        wait_http(f"{SERVER_URL}/status")

        proxy_proc = start_proxy(log_dir)
        procs.append(proxy_proc)
        wait_until("server proxy", lambda: urllib.request.urlopen(f"{SERVER_PROXY_URL}/status", timeout=2).status < 500, timeout_sec=10)

        heartbeat_thread = threading.Thread(target=task_heartbeat_loop, args=(task_id, created["task_token"], heartbeat_stop), daemon=True)
        heartbeat_thread.start()
        wait_until("hub task ready", lambda: request_json(f"{HUB_URL}/v1/tasks/{task_id}").get("service_state") == "ready", timeout_sec=10)

        procs.append(
            start_process(
                "helper",
                [
                    helper_exe,
                    "--hub-base-url",
                    HUB_URL,
                    "--bearer-token",
                    TOKEN,
                    "--helper-id",
                    HELPER_ID,
                    "--user-name",
                    "local-probe",
                    "--capacity",
                    "1",
                ],
                log_dir,
            )
        )

        wait_until("helper claim", lambda: request_json(f"{HUB_URL}/v1/tasks/{task_id}").get("claimed_by_helper_id") == HELPER_ID, timeout_sec=45)
        wait_until("plugin registration", lambda: len(helper_task(task_id).get("instances") or []) > 0, timeout_sec=120, interval_sec=0.5)
        wait_until("task-server active helper", lambda: (server_state().get("active_helper") or {}).get("task_id") == task_id, timeout_sec=45)
        run_preflight(task_id)

        client = McpClient(base_url=SERVER_URL, token=TOKEN, request_timeout_sec=180, tool_retry_attempts=1)
        observations: dict[str, Any] = {"task_id": task_id, "cycles": []}

        for index in range(args.cycles_before):
            start = call_tool(client, "launch_studio_session", {"task_id": task_id, "mode": "start_play"})
            start_triplet = wait_triplet(task_id, "start_play", "ready", "running")
            stop = call_tool(client, "start_stop_play", {"task_id": task_id, "mode": "stop"})
            stop_triplet = wait_triplet(task_id, "stop", "none", "idle")
            observations["cycles"].append({"phase": "before_disconnect", "index": index, "start": start, "start_triplet": start_triplet, "stop": stop, "stop_triplet": stop_triplet})

        before_disconnect = {
            "helper_remote_state": remote_state(task_id),
            "task_server_ok": request_json(f"{SERVER_URL}/status"),
        }
        observations["before_disconnect"] = before_disconnect
        if before_disconnect["helper_remote_state"] != "connected":
            raise RuntimeError(f"expected helper remote connected before disconnect, got {before_disconnect}")

        print(f"[disconnect-probe] pausing proxy for {args.disconnect_seconds:.1f}s", flush=True)
        stop_process(proxy_proc)
        proxy_proc = None
        wait_until("helper remote leaves connected", lambda: remote_state(task_id) != "connected", timeout_sec=30)
        outage_samples = []
        deadline = time.monotonic() + args.disconnect_seconds
        while time.monotonic() < deadline:
            sample = {
                "remaining_sec": round(deadline - time.monotonic(), 1),
                "helper_remote_state": remote_state(task_id),
                "task_server_status_source": request_json(f"{SERVER_URL}/status").get("status_source"),
                "task_server_process_alive": True,
            }
            outage_samples.append(sample)
            print(f"[disconnect-probe] outage sample {sample}", flush=True)
            time.sleep(min(15.0, max(0.0, deadline - time.monotonic())))
        observations["outage_samples"] = outage_samples

        proxy_proc = start_proxy(log_dir)
        procs.append(proxy_proc)
        wait_until("proxy restored", lambda: urllib.request.urlopen(f"{SERVER_PROXY_URL}/status", timeout=2).status < 500, timeout_sec=10)
        wait_until("helper remote reconnected", lambda: remote_state(task_id) == "connected", timeout_sec=60)
        wait_until("task-server active helper restored", lambda: (server_state().get("active_helper") or {}).get("task_id") == task_id, timeout_sec=30)
        run_preflight(task_id)
        observations["after_reconnect"] = {
            "helper_remote_state": remote_state(task_id),
            "task_server_active_helper": server_state().get("active_helper"),
        }

        for index in range(args.cycles_after):
            start = call_tool(client, "launch_studio_session", {"task_id": task_id, "mode": "start_play"})
            start_triplet = wait_triplet(task_id, "start_play", "ready", "running")
            stop = call_tool(client, "start_stop_play", {"task_id": task_id, "mode": "stop"})
            stop_triplet = wait_triplet(task_id, "stop", "none", "idle")
            observations["cycles"].append({"phase": "after_disconnect", "index": index, "start": start, "start_triplet": start_triplet, "stop": stop, "stop_triplet": stop_triplet})

        print(json.dumps(observations, ensure_ascii=False, indent=2), flush=True)
        return 0
    finally:
        heartbeat_stop.set()
        if heartbeat_thread is not None:
            heartbeat_thread.join(timeout=2)
        for proc in reversed(procs):
            stop_process(proc)
        try:
            subprocess.run(
                [
                    "powershell",
                    "-NoProfile",
                    "-Command",
                    (
                        "Get-CimInstance Win32_Process -Filter \"name = 'RobloxStudioBeta.exe'\" "
                        f"| Where-Object {{ $_.CommandLine -match '{PLACE_ID}' }} "
                        "| ForEach-Object { Stop-Process -Id $_.ProcessId -Force }"
                    ),
                ],
                timeout=10,
                check=False,
            )
        except Exception:
            pass


if __name__ == "__main__":
    raise SystemExit(main())
