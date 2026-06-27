#!/usr/bin/env python3
from __future__ import annotations

import json
import subprocess
import sys
import threading
import time
import urllib.error
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


REPO = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(REPO / "util"))

from local_mcp_client import McpClient, extract_text_content, require_non_error_result


HUB_URL = "http://127.0.0.1:44758"
HELPER_URL = "http://127.0.0.1:44750"
SERVER_URL = "http://127.0.0.1:44756"
ROJO_URL = "http://127.0.0.1:44757"
HELPER_ID = "h_local_state_sync"
PLACE_ID = "134795435066737"
UNIVERSE_ID = "9327304100"
TOKEN = "local-test-token"
RUNTIME_LOG_SINK_URL = "http://127.0.0.1:44759"


class RuntimeLogSinkState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.delay_sec = 0.0
        self.fail_next = False
        self.received: list[dict[str, Any]] = []


class RuntimeLogSinkHandler(BaseHTTPRequestHandler):
    sink_state: RuntimeLogSinkState

    def do_POST(self) -> None:  # noqa: N802 - stdlib handler name
        length = int(self.headers.get("Content-Length") or "0")
        body = self.rfile.read(length)
        with self.sink_state.lock:
            delay_sec = self.sink_state.delay_sec
            fail = self.sink_state.fail_next
            self.sink_state.fail_next = False
            self.sink_state.received.append(
                {
                    "path": self.path,
                    "bytes": len(body),
                    "failed": fail,
                    "received_at": time.time(),
                }
            )
        if delay_sec > 0:
            time.sleep(delay_sec)
        if fail:
            payload = b"runtime log sink forced failure"
            self.send_response(503)
            self.send_header("Content-Length", str(len(payload)))
            self.end_headers()
            self.wfile.write(payload)
        else:
            self.send_response(204)
            self.send_header("Content-Length", "0")
            self.end_headers()

    def log_message(self, _format: str, *_args: Any) -> None:
        return


def start_runtime_log_sink() -> tuple[ThreadingHTTPServer, RuntimeLogSinkState]:
    state = RuntimeLogSinkState()

    class Handler(RuntimeLogSinkHandler):
        sink_state = state

    server = ThreadingHTTPServer(("127.0.0.1", 44759), Handler)
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    return server, state


def request_json(url: str, *, method: str = "GET", payload: dict[str, Any] | None = None) -> Any:
    data = None
    headers = {"Content-Type": "application/json"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    with urllib.request.urlopen(req, timeout=10) as response:
        body = response.read()
    if not body:
        return None
    return json.loads(body.decode("utf-8"))


def wait_until(label: str, predicate, timeout_sec: float = 90.0, interval_sec: float = 0.25):
    deadline = time.monotonic() + timeout_sec
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = predicate()
            if value:
                return value
        except Exception as exc:  # noqa: BLE001 - probe reports last observed failure.
            last_error = exc
        time.sleep(interval_sec)
    raise RuntimeError(f"timed out waiting for {label}; last_error={last_error}")


def start_process(name: str, args: list[str], log_dir: Path) -> subprocess.Popen:
    log_path = log_dir / f"{name}.log"
    log_file = log_path.open("w", encoding="utf-8")
    env = dict(**__import__("os").environ)
    env.setdefault("RUST_LOG", "info")
    return subprocess.Popen(
        args,
        cwd=REPO,
        stdout=log_file,
        stderr=subprocess.STDOUT,
        env=env,
        text=True,
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
            "mcp_base_url": SERVER_URL,
            "runtime_log_base_url": RUNTIME_LOG_SINK_URL,
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
            print(f"[probe] task heartbeat failed: {exc}", flush=True)
        stop_event.wait(2.0)


def hub_active_task(task_id: str) -> dict[str, Any] | None:
    status = request_json(f"{HUB_URL}/status")
    for helper in status.get("helpers", []):
        if helper.get("helper_id") != HELPER_ID:
            continue
        for task in helper.get("active_tasks", []):
            if task.get("task_id") == task_id:
                return task
    return None


def helper_task(task_id: str) -> dict[str, Any]:
    return request_json(f"{HELPER_URL}/v1/debug/tasks/{task_id}")


def post_helper_runtime_log(upload_url: str, timeout_sec: float = 1.0) -> tuple[int, float]:
    payload = json.dumps(
        {
            "session_id": "probe-runtime-log-forward",
            "runtime_id": "server",
            "lines": [{"message": "[probe] runtime-log-forward"}],
        }
    ).encode("utf-8")
    started = time.perf_counter()
    request = urllib.request.Request(
        upload_url,
        data=payload,
        method="POST",
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request, timeout=timeout_sec) as response:
        response.read()
        status = response.status
    elapsed_ms = (time.perf_counter() - started) * 1000
    return status, elapsed_ms


def wait_runtime_log_forward_status(task_id: str, predicate, label: str) -> dict[str, Any]:
    def sample():
        status = (helper_task(task_id).get("runtime_log_forward") or {})
        if predicate(status):
            return status
        return None

    return wait_until(label, sample, timeout_sec=8, interval_sec=0.1)


def wait_runtime_log_forward_everywhere(task_id: str, predicate, label: str) -> dict[str, Any]:
    def sample():
        helper_status = helper_task(task_id).get("runtime_log_forward") or {}
        hub_status = hub_active_task(task_id) or {}
        hub_runtime = hub_status.get("runtime_log_forward") or {}
        server = server_state()
        server_runtime = (
            ((server.get("active_helper") or {}).get("task_status") or {}).get("runtime_log_forward")
            or {}
        )
        server_top = request_json(f"{SERVER_URL}/status").get("runtime_log_forward") or {}
        observed = {
            "helper": helper_status,
            "hub": hub_runtime,
            "server_task_status": server_runtime,
            "server_status": server_top,
        }
        if all(predicate(value) for value in observed.values()):
            return observed
        return None

    return wait_until(label, sample, timeout_sec=8, interval_sec=0.1)


def server_state() -> dict[str, Any]:
    return request_json(f"{SERVER_URL}/v1/debug/state")


def wait_triplet(
    client: McpClient,
    task_id: str,
    session_state: str,
    control: str,
    phase: str,
) -> dict[str, Any]:
    def sample():
        hub_task = hub_active_task(task_id)
        helper = helper_task(task_id)
        server = server_state()
        helper_snapshot = helper.get("task_status") or {}
        server_task_status = (
            server.get("active_helper")
            or {}
        ).get("task_status") or {}
        live_state = call_tool(client, "get_studio_session_state", {"task_id": task_id})
        observed = {
            "hub": hub_task,
            "helper": helper_snapshot,
            "server": server_task_status,
            "live_state": live_state,
        }
        if (
            hub_task
            and live_state.get("studio_session_state") == session_state
            and hub_task.get("studio_control_state") == control
            and hub_task.get("studio_transition_phase") == phase
            and helper_snapshot.get("studio_control_state") == control
            and helper_snapshot.get("studio_transition_phase") == phase
            and server_task_status.get("studio_control_state") == control
            and server_task_status.get("studio_transition_phase") == phase
        ):
            return observed
        return None

    return wait_until(
        f"hub/helper/server {session_state}/{control}/{phase}",
        sample,
        timeout_sec=8,
    )


def call_tool(client: McpClient, name: str, arguments: dict[str, Any]) -> dict[str, Any]:
    result, _diagnostics = client.call_tool_with_diagnostics(name, arguments)
    result = require_non_error_result(name, result)
    text = extract_text_content(result)
    try:
        return json.loads(text) if text else {}
    except json.JSONDecodeError:
        return {"text": text}


def print_debug_snapshot(task_id: str, label: str):
    snapshot: dict[str, Any] = {"label": label}
    for name, getter in {
        "hub_status": lambda: request_json(f"{HUB_URL}/status"),
        "hub_task": lambda: request_json(f"{HUB_URL}/v1/tasks/{task_id}"),
        "hub_helper": lambda: request_json(f"{HUB_URL}/v1/debug/helpers/{HELPER_ID}"),
        "helper_task": lambda: helper_task(task_id),
        "server_state": server_state,
        "server_status": lambda: request_json(f"{SERVER_URL}/status"),
    }.items():
        try:
            snapshot[name] = getter()
        except Exception as exc:  # noqa: BLE001
            snapshot[name] = {"error": str(exc)}
    print(json.dumps(snapshot, ensure_ascii=False, indent=2), flush=True)


def main() -> int:
    log_dir = REPO / ".tmp" / "local-state-sync-probe"
    log_dir.mkdir(parents=True, exist_ok=True)
    state_file = log_dir / "hub-state.json"
    if state_file.exists():
        state_file.unlink()

    procs: list[subprocess.Popen] = []
    heartbeat_stop = threading.Event()
    heartbeat_thread: threading.Thread | None = None
    runtime_log_sink: ThreadingHTTPServer | None = None
    runtime_log_sink_state: RuntimeLogSinkState | None = None
    task_id = ""
    try:
        runtime_log_sink, runtime_log_sink_state = start_runtime_log_sink()
        hub_exe = str(REPO / "target" / "release" / "roblox_hub.exe")
        server_exe = str(REPO / "target" / "release" / "rbx-studio-mcp.exe")
        helper_exe = str(REPO / "target" / "release" / "studio_helper.exe")
        rojo_exe = REPO.parent / "rojo" / "target" / "release" / "rojo.exe"
        if not rojo_exe.exists():
            raise RuntimeError(f"missing sibling Rojo release binary: {rojo_exe}")

        procs.append(
            start_process(
                "hub",
                [hub_exe, "--no-auth", "--state-file", str(state_file), "--heartbeat-interval-sec", "2"],
                log_dir,
            )
        )
        wait_http(f"{HUB_URL}/status")

        created = request_json(
            f"{HUB_URL}/v1/tasks/create",
            method="POST",
            payload={
                "cluster_key": "local-state-sync-probe",
                "owner_user": "local-probe",
                "repo": "game1",
                "worktree_name": "local-state-sync-probe",
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
                    "name": "local-state-sync-probe",
                    "tree": {
                        "$className": "DataModel",
                        "Workspace": {
                            "$className": "Workspace",
                        },
                    },
                },
                indent=2,
            ),
            encoding="utf-8",
        )
        procs.append(
            start_process(
                "rojo",
                [
                    str(rojo_exe),
                    "serve",
                    str(rojo_project),
                    "--port",
                    "44757",
                    "--task-id",
                    task_id,
                    "--workspace-path",
                    str(REPO),
                    "--public-base-url",
                    ROJO_URL,
                ],
                log_dir,
            )
        )
        wait_http_bytes(f"{ROJO_URL}/api/rojo")

        heartbeat_thread = threading.Thread(
            target=task_heartbeat_loop,
            args=(task_id, created["task_token"], heartbeat_stop),
            daemon=True,
        )
        heartbeat_thread.start()
        wait_until(
            "hub task ready",
            lambda: request_json(f"{HUB_URL}/v1/tasks/{task_id}").get("service_state") == "ready",
            timeout_sec=10,
        )

        procs.append(
            start_process(
                "server",
                [
                    server_exe,
                    "--http",
                    "--no-auth",
                    "--task-id",
                    task_id,
                    "--workspace-path",
                    str(REPO),
                    "--hub-base-url",
                    HUB_URL,
                ],
                log_dir,
            )
        )
        wait_http(f"{SERVER_URL}/status")

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

        wait_until(
            "helper claim",
            lambda: request_json(f"{HUB_URL}/v1/tasks/{task_id}").get("claimed_by_helper_id") == HELPER_ID,
            timeout_sec=45,
        )

        helper_claim = helper_task(task_id)
        upload_url = (helper_claim.get("claimed_task") or {}).get("runtime_log_upload_url")
        if not upload_url:
            raise RuntimeError(f"helper did not expose runtime_log_upload_url: {helper_claim}")
        assert runtime_log_sink_state is not None
        runtime_log_sink_state.delay_sec = 1.0
        status, slow_ack_elapsed_ms = post_helper_runtime_log(upload_url, timeout_sec=1.0)
        if status != 202 or slow_ack_elapsed_ms > 500:
            raise RuntimeError(
                f"runtime-log forward did not fast ack slow sink: status={status} elapsed_ms={slow_ack_elapsed_ms:.1f}"
            )
        first_forward = wait_runtime_log_forward_status(
            task_id,
            lambda status: status.get("forwarded_count") == 1 and status.get("state") == "ready",
            "runtime-log slow sink background forward",
        )
        first_forward_everywhere = wait_runtime_log_forward_everywhere(
            task_id,
            lambda status: status.get("forwarded_count") == 1 and status.get("state") == "ready",
            "runtime-log slow sink status propagated to hub/server",
        )
        runtime_log_sink_state.delay_sec = 0.0
        with runtime_log_sink_state.lock:
            runtime_log_sink_state.fail_next = True
        status, failing_ack_elapsed_ms = post_helper_runtime_log(upload_url, timeout_sec=1.0)
        if status != 202 or failing_ack_elapsed_ms > 500:
            raise RuntimeError(
                f"runtime-log forward did not fast ack failing sink: status={status} elapsed_ms={failing_ack_elapsed_ms:.1f}"
            )
        failed_forward = wait_runtime_log_forward_status(
            task_id,
            lambda status: status.get("failed_count") == 1
            and status.get("last_http_status") == 503
            and "HTTP 503" in str(status.get("last_error")),
            "runtime-log failing sink status",
        )
        failed_forward_everywhere = wait_runtime_log_forward_everywhere(
            task_id,
            lambda status: status.get("failed_count") == 1
            and status.get("last_http_status") == 503
            and "HTTP 503" in str(status.get("last_error")),
            "runtime-log failing sink status propagated to hub/server",
        )
        print(
            json.dumps(
                {
                    "runtime_log_forward_probe": {
                        "slow_ack_elapsed_ms": int(slow_ack_elapsed_ms),
                        "failing_ack_elapsed_ms": int(failing_ack_elapsed_ms),
                        "first_forward": first_forward,
                        "first_forward_everywhere": first_forward_everywhere,
                        "failed_forward": failed_forward,
                        "failed_forward_everywhere": failed_forward_everywhere,
                    }
                },
                ensure_ascii=False,
                indent=2,
            ),
            flush=True,
        )

        try:
            wait_until(
                "plugin registration",
                lambda: len(helper_task(task_id).get("instances") or []) > 0,
                timeout_sec=120,
                interval_sec=0.5,
            )
        except Exception:
            print_debug_snapshot(task_id, "plugin registration timeout")
            raise
        try:
            wait_until(
                "server active helper",
                lambda: (server_state().get("active_helper") or {}).get("task_id") == task_id,
                timeout_sec=45,
            )
        except Exception:
            print_debug_snapshot(task_id, "server active helper timeout")
            raise

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

        client = McpClient(
            base_url=SERVER_URL,
            token=TOKEN,
            request_timeout_sec=180,
            tool_retry_attempts=1,
        )

        initial = call_tool(client, "get_studio_session_state", {"task_id": task_id})
        print(f"[probe] initial get_studio_session_state={initial}", flush=True)

        started = time.perf_counter()
        start_result = call_tool(
            client,
            "launch_studio_session",
            {"task_id": task_id, "mode": "start_play"},
        )
        start_elapsed_ms = int((time.perf_counter() - started) * 1000)
        start_triplet = wait_triplet(client, task_id, "play", "ready", "running")

        try:
            relaunch_result, _relaunch_diagnostics = client.call_tool_with_diagnostics(
                "launch_studio_session",
                {"task_id": task_id, "mode": "start_play"},
            )
        except RuntimeError as exc:
            relaunch_text = str(exc)
        else:
            relaunch_text = extract_text_content(relaunch_result)
        if "studio_already_playing" not in relaunch_text:
            raise RuntimeError(f"low-level launch while play should fail without stopping: {relaunch_text}")
        relaunch_rejected_triplet = wait_triplet(client, task_id, "play", "ready", "running")

        started = time.perf_counter()
        stop_result = call_tool(
            client,
            "start_stop_play",
            {"task_id": task_id, "mode": "stop"},
        )
        stop_elapsed_ms = int((time.perf_counter() - started) * 1000)
        stop_triplet = wait_triplet(client, task_id, "stop", "none", "idle")

        repeat_stop = call_tool(
            client,
            "start_stop_play",
            {"task_id": task_id, "mode": "stop"},
        )
        repeat_triplet = wait_triplet(client, task_id, "stop", "none", "idle")

        cleanup_check = call_tool(
            client,
            "run_code",
            {
                "task_id": task_id,
                "diagnostic": True,
                "command": (
                    'return tostring(game:GetService("ServerScriptService"):FindFirstChild("MCPStudioSessionControl") == nil)'
                ),
            },
        )
        cleanup_text = str(cleanup_check.get("text", ""))
        if "true" not in cleanup_text:
            raise RuntimeError(f"one-shot session control script was not cleaned up: {cleanup_check}")

        print(
            json.dumps(
                {
                    "task_id": task_id,
                    "cleanup_check": cleanup_check,
                    "start_elapsed_ms": start_elapsed_ms,
                    "stop_elapsed_ms": stop_elapsed_ms,
                    "start_result": start_result,
                    "relaunch_rejected": relaunch_text,
                    "relaunch_rejected_triplet": relaunch_rejected_triplet,
                    "stop_result": stop_result,
                    "repeat_stop": repeat_stop,
                    "start_triplet": start_triplet,
                    "stop_triplet": stop_triplet,
                    "repeat_triplet": repeat_triplet,
                },
                ensure_ascii=False,
                indent=2,
            ),
            flush=True,
        )
        return 0
    finally:
        heartbeat_stop.set()
        if heartbeat_thread is not None:
            heartbeat_thread.join(timeout=2)
        if runtime_log_sink is not None:
            runtime_log_sink.shutdown()
            runtime_log_sink.server_close()
        for proc in reversed(procs):
            proc.terminate()
        for proc in reversed(procs):
            try:
                proc.wait(timeout=5)
            except subprocess.TimeoutExpired:
                proc.kill()
        try:
            import subprocess as _subprocess

            _subprocess.run(
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
