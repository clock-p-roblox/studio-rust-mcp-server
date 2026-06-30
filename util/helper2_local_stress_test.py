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
import uuid
from pathlib import Path
from typing import Any


HELPER_BASE_URL = "http://127.0.0.1:44750"
DEFAULT_PLACE_ID = "105986423068266"
DEFAULT_PROJECT = Path(r"D:\roblox_space\.codex-runtime-stop-local\workspace\default.project.json")
DEFAULT_ROJO = Path(r"D:\roblox_space\rojo\target\release\rojo.exe")
PLUGIN_PATH = Path(os.environ["LOCALAPPDATA"]) / "Roblox" / "Plugins" / "MCP2Plugin.rbxm"
ROBLOX_LOG_DIR = Path(os.environ["LOCALAPPDATA"]) / "Roblox" / "logs"
ROJO_STUDIO_FAILURE_MARKERS = (
    "Unknown HTTP error: 502",
    "Rojo serve session start failed",
    "WebSocket connection closed unexpectedly",
)


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


def http_json_expect_error(
    method: str,
    url: str,
    expected: int,
    payload: dict[str, Any] | None = None,
    *,
    timeout: float = 10,
) -> Any:
    data = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(request, timeout=timeout) as response:
            body = response.read().decode("utf-8")
        raise StressError(f"expected HTTP {expected} from {method} {url}, got success: {body}")
    except urllib.error.HTTPError as exc:
        body = exc.read()
        if exc.code != expected:
            raise StressError(f"expected HTTP {expected} from {method} {url}, got {exc.code}: {body.decode('utf-8', errors='replace')}") from exc
        if not body:
            return None
        return json.loads(body.decode("utf-8"))


def expect_http_status(url: str, expected: int, *, method: str = "GET") -> None:
    try:
        request = urllib.request.Request(url, method=method)
        urllib.request.urlopen(request, timeout=5).read()
    except urllib.error.HTTPError as exc:
        if exc.code != expected:
            raise StressError(f"expected HTTP {expected} from {method} {url}, got {exc.code}") from exc
        return
    raise StressError(f"expected HTTP {expected} from {method} {url}, got success")


def capture_task_screenshot(task_id: str) -> Any:
    return http_json("GET", f"{HELPER_BASE_URL}/session/{task_id}/studio/screenshot", timeout=40)


def direct_task_mode(task_id: str, *, timeout: float = 8) -> dict[str, Any]:
    return http_json("GET", f"{HELPER_BASE_URL}/session/{task_id}/studio/mode", timeout=timeout)


def direct_task_play(task_id: str, *, timeout: float = 130) -> dict[str, Any]:
    return http_json("POST", f"{HELPER_BASE_URL}/session/{task_id}/studio/play", timeout=timeout)


def direct_task_stop(task_id: str, *, timeout: float = 130) -> dict[str, Any]:
    return http_json("POST", f"{HELPER_BASE_URL}/session/{task_id}/studio/stop", timeout=timeout)


def expect_external_response_result_ignored() -> None:
    payload = http_json(
        "POST",
        f"{HELPER_BASE_URL}/plugin/mcp2/response_result",
        {
            "command_id": 999999,
            "mode": "edit",
            "mode_seq": 1,
            "ok": True,
        },
        timeout=5,
    )
    if not payload.get("ignored") or payload.get("recorded") or payload.get("reason") != "invalid_lifecycle_source":
        raise StressError(f"external response_result was not ignored as fake Studio traffic: {payload}")


def require_task_channel_idle(summary: dict[str, Any], task_id: str) -> None:
    channel = summary.get("mcp2_channels", {}).get(task_id)
    if not isinstance(channel, dict):
        raise StressError(f"missing mcp2 channel for {task_id}: {summary.get('mcp2_channels')}")
    if channel.get("queued_command_count") != 0 or channel.get("waiting_response_command") is not None:
        raise StressError(f"task {task_id} channel is not idle: {channel}")


def require_screenshot_file(payload: dict[str, Any]) -> None:
    screenshot = payload.get("screenshot")
    if not isinstance(screenshot, dict):
        raise StressError(f"screenshot payload missing: {payload}")
    path = Path(str(screenshot.get("path", "")))
    if not path.exists() or path.stat().st_size <= 0:
        raise StressError(f"screenshot file missing or empty: {screenshot}")
    if int(screenshot.get("bytes") or 0) <= 0 or int(screenshot.get("studio_pid") or 0) <= 0:
        raise StressError(f"screenshot metadata is not real Studio evidence: {screenshot}")


def require_helper_pid_evidence(log_path: Path, task_ids: list[str]) -> None:
    text = log_path.read_text(encoding="utf-8", errors="replace")
    lines = text.splitlines()
    for task_id in task_ids:
        pull_ok = any(
            'msg="mcp2 pull completed"' in line
            and f"owner_id={task_id}" in line
            and 'user_agent="RobloxStudio' in line
            and "studio_pid=0" not in line
            and ("kind=studio_play" in line or "kind=studio_stop" in line or "kind=studio_mode_query" in line)
            for line in lines
        )
        response_ok = any(
            'msg="mcp2 response acknowledged"' in line
            and f"owner_id={task_id}" in line
            and "studio_pid=0" not in line
            and "recorded=true" in line
            for line in lines
        )
        if not pull_ok or not response_ok:
            raise StressError(f"helper log lacks real Studio PID pull/result evidence for {task_id}: pull={pull_ok} response={response_ok}")


def create_sync_probe_project(parent: Path) -> tuple[Path, str]:
    marker = "CLOCKP_ROJO_SYNC_PROBE_" + uuid.uuid4().hex
    project = parent / "sync-probe-project"
    src = project / "src"
    src.mkdir(parents=True, exist_ok=True)
    (project / "default.project.json").write_text(
        json.dumps(
            {
                "name": "helper2-sync-probe",
                "tree": {
                    "$className": "DataModel",
                    "ServerScriptService": {
                        "$className": "ServerScriptService",
                        "$path": "src",
                    },
                },
            },
            indent=2,
        )
        + "\n",
        encoding="utf-8",
    )
    (src / "RojoSyncProbe.server.lua").write_text(
        "\n".join(
            [
                f'local marker = "{marker}"',
                'script:SetAttribute("ClockPSyncProbeMarker", marker)',
                "print(marker)",
                "",
            ]
        ),
        encoding="utf-8",
    )
    return project / "default.project.json", marker


def wait_studio_log_marker(marker: str, *, timeout: float) -> Path:
    deadline = time.time() + timeout
    recent_logs = sorted(ROBLOX_LOG_DIR.glob("*Studio*_last.log"), key=lambda p: p.stat().st_mtime, reverse=True)[:20]
    while time.time() < deadline:
        for log_path in recent_logs:
            if not log_path.exists():
                continue
            text = log_path.read_text(encoding="utf-8", errors="replace")
            if marker in text:
                return log_path
        recent_logs = sorted(ROBLOX_LOG_DIR.glob("*Studio*_last.log"), key=lambda p: p.stat().st_mtime, reverse=True)[:20]
        time.sleep(1)
    raise StressError(f"Studio logs did not contain sync probe marker: {marker}")


def recent_studio_logs(since: float) -> list[Path]:
    return [
        path
        for path in sorted(ROBLOX_LOG_DIR.glob("*Studio*_last.log"), key=lambda p: p.stat().st_mtime, reverse=True)
        if path.stat().st_mtime >= since - 5
    ][:20]


def studio_log_contains(path: Path, needles: list[str]) -> bool:
    text = path.read_text(encoding="utf-8", errors="replace")
    return all(needle in text for needle in needles)


def wait_studio_rojo_connected(task_id: str, project_name: str, *, since: float, timeout: float) -> Path:
    deadline = time.time() + timeout
    last_logs: list[Path] = []
    while time.time() < deadline:
        last_logs = recent_studio_logs(since)
        for log_path in last_logs:
            if studio_log_contains(
                log_path,
                [
                    f"/task/{task_id}",
                    "Rojo serve session initial sync completed",
                    f"Rojo serve session status changed to Connected (details={project_name}",
                ],
            ):
                return log_path
        time.sleep(1)
    raise StressError(f"Studio log does not prove Rojo connected for task {task_id}; checked={last_logs}")


def wait_studio_sync_probe(task_id: str, marker: str, *, since: float, timeout: float) -> Path:
    deadline = time.time() + timeout
    last_logs: list[Path] = []
    while time.time() < deadline:
        last_logs = recent_studio_logs(since)
        for log_path in last_logs:
            if studio_log_contains(log_path, [f"/task/{task_id}", marker]):
                return log_path
        time.sleep(1)
    raise StressError(f"Studio log does not prove synced code ran for task {task_id}; marker={marker}; checked={last_logs}")


def assert_no_studio_rojo_failures(task_ids: list[str], *, since: float) -> None:
    failures: list[str] = []
    for log_path in recent_studio_logs(since):
        text = log_path.read_text(encoding="utf-8", errors="replace")
        if not any(f"/task/{task_id}" in text or f'"task_id":"{task_id}"' in text or f"task_id={task_id}" in text for task_id in task_ids):
            continue
        for line in text.splitlines():
            if any(task_id in line for task_id in task_ids) or any(f"/task/{task_id}" in line for task_id in task_ids):
                if any(marker in line for marker in ROJO_STUDIO_FAILURE_MARKERS):
                    failures.append(f"{log_path}: {line[:1000]}")
            elif any(marker in line for marker in ROJO_STUDIO_FAILURE_MARKERS):
                # Rojo often logs the task id on neighboring lines only; keep this as a hard failure for current-run logs.
                failures.append(f"{log_path}: {line[:1000]}")
    if failures:
        raise StressError("Studio Rojo failure detected before intentional task-agent kill:\n" + "\n".join(failures[:12]))


def wait_rojo_helper_evidence(log_path: Path, task_id: str, *, timeout: float) -> None:
    deadline = time.time() + timeout
    last_state = ""
    while time.time() < deadline:
        text = log_path.read_text(encoding="utf-8", errors="replace") if log_path.exists() else ""
        lines = text.splitlines()
        config_ok = any('msg="resolved task-bound Rojo config"' in line and f"task_id={task_id}" in line for line in lines)
        http_ok = any('msg="proxied task-bound Rojo HTTP request"' in line and f"task_id={task_id}" in line for line in lines)
        ws_ok = any('msg="proxying task-bound Rojo websocket"' in line and f"task_id={task_id}" in line for line in lines)
        last_state = f"config={config_ok} http={http_ok} websocket={ws_ok}"
        if config_ok and http_ok and ws_ok:
            return
        time.sleep(1)
    raise StressError(f"helper log lacks Studio-side Rojo evidence for {task_id}: {last_state}")


def post_manual_heartbeat(task_id: str, place_id: str, pid: int, started_at: int, rojo_port: int) -> dict[str, Any]:
    return http_json(
        "POST",
        f"{HELPER_BASE_URL}/session/{task_id}/heartbeat",
        {
            "task_id": task_id,
            "machine_name": "manual-phase11",
            "place_id": place_id,
            "task_agent_pid": pid,
            "task_agent_started_at_ms": started_at,
            "rojo_upstream_url": f"http://127.0.0.1:{rojo_port}",
        },
        timeout=5,
    )


def release_manual_task(task_id: str, pid: int, started_at: int) -> dict[str, Any]:
    return http_json(
        "POST",
        f"{HELPER_BASE_URL}/session/{task_id}/release",
        {
            "task_agent_pid": pid,
            "task_agent_started_at_ms": started_at,
        },
        timeout=5,
    )


def check_direct_api_error_states(place_id: str) -> None:
    missing_play = http_json_expect_error("POST", f"{HELPER_BASE_URL}/session/not-a-real-task/studio/play", 404)
    if missing_play.get("state") != "not_registered":
        raise StressError(f"unexpected missing task play payload: {missing_play}")

    manual_task = "manual-phase11-no-studio"
    manual_pid = 49001
    manual_started = 1782658000000
    post_manual_heartbeat(manual_task, place_id, manual_pid, manual_started, 49091)
    no_studio = http_json_expect_error("GET", f"{HELPER_BASE_URL}/session/{manual_task}/studio/screenshot", 409)
    if no_studio.get("code") != "studio_not_available":
        raise StressError(f"expected studio_not_available for manual task screenshot, got {no_studio}")
    release_manual_task(manual_task, manual_pid, manual_started)
    ended_play = http_json_expect_error("POST", f"{HELPER_BASE_URL}/session/{manual_task}/studio/play", 409)
    if ended_play.get("code") != "task_not_live":
        raise StressError(f"expected task_not_live for ended task play, got {ended_play}")
    ended_mode = direct_task_mode(manual_task)
    if ended_mode.get("available") or ended_mode.get("state") != "ended":
        raise StressError(f"expected ended task mode unavailable, got {ended_mode}")


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
        or "[Rojo-" in line
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


def wait_mode_direct(task_id: str, expected: str, *, timeout: float) -> dict[str, Any]:
    last_payload: dict[str, Any] | None = None

    def read_mode() -> dict[str, Any] | None:
        nonlocal last_payload
        payload = direct_task_mode(task_id, timeout=8)
        last_payload = payload
        if payload.get("available") and payload.get("mode") == expected:
            return payload
        return None

    try:
        return wait_until(f"task {task_id} direct mode {expected}", timeout, read_mode)
    except StressError as exc:
        raise StressError(f"{exc}; last direct mode payload={last_payload}") from exc


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


def call_direct_for_executor(action: str, task_id: str) -> dict[str, Any]:
    if action == "play":
        return direct_task_play(task_id)
    if action == "stop":
        return direct_task_stop(task_id)
    raise StressError(f"unknown direct action: {action}")


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
    studio_log_since = time.time()
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
    project_path = args.project
    sync_probe_marker = ""

    prepare_binaries(root, bin_dir, args.rojo)
    print(f"run_dir={run_dir}", flush=True)
    if args.sync_probe:
        project_path, sync_probe_marker = create_sync_probe_project(run_dir)
        print(f"sync_probe_project={project_path} marker={sync_probe_marker}", flush=True)
    ensure_no_existing_test_processes(kill_existing=args.kill_existing)

    try:
        helper = start_helper_process(bin_dir, logs_dir, "helper.log")
        wait_health()
        print(f"helper pid={helper.pid}", flush=True)

        wait_mode_fn = wait_mode_direct if args.direct_api_only else wait_mode

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
                str(project_path),
            ],
            logs_dir / "agent-a.log",
        )
        desc_a = wait_descriptor(work_a)
        print(f"taskA={desc_a['task_id']} agentPid={agent_a.pid}", flush=True)
        try:
            mode_a = wait_mode_fn(desc_a["task_id"], "edit", timeout=args.initial_mode_timeout)
        except Exception as exc:
            try:
                shot = capture_task_screenshot(desc_a["task_id"])
                print(f"initial taskA screenshot before failure: {shot}", flush=True)
            except Exception as shot_exc:
                print(f"initial taskA screenshot failed: {shot_exc}", flush=True)
            print("initial taskA studio diagnostics:\n" + latest_studio_diagnostics(), flush=True)
            raise exc
        print(f"taskA initial mode={mode_a['mode']}", flush=True)
        if args.require_rojo_plugin:
            wait_rojo_helper_evidence(logs_dir / "helper.log", desc_a["task_id"], timeout=args.rojo_evidence_timeout)
            print(f"taskA Rojo helper evidence ok task={desc_a['task_id']}", flush=True)
            task_a_rojo_log = wait_studio_rojo_connected(
                desc_a["task_id"],
                "helper2-sync-probe" if sync_probe_marker else "runtime-stop-local",
                since=studio_log_since,
                timeout=args.rojo_evidence_timeout,
            )
            print(f"taskA Studio Rojo connected evidence ok log={task_a_rojo_log}", flush=True)

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
                str(project_path),
            ],
            logs_dir / "agent-b.log",
        )
        desc_b = wait_descriptor(work_b)
        print(f"taskB={desc_b['task_id']} agentPid={agent_b.pid}", flush=True)
        try:
            mode_b = wait_mode_fn(desc_b["task_id"], "edit", timeout=args.initial_mode_timeout)
        except Exception as exc:
            try:
                shot = capture_task_screenshot(desc_b["task_id"])
                print(f"initial taskB screenshot before failure: {shot}", flush=True)
            except Exception as shot_exc:
                print(f"initial taskB screenshot failed: {shot_exc}", flush=True)
            print("initial taskB studio diagnostics:\n" + latest_studio_diagnostics(), flush=True)
            raise exc
        print(f"taskB initial mode={mode_b['mode']}", flush=True)
        if args.require_rojo_plugin:
            wait_rojo_helper_evidence(logs_dir / "helper.log", desc_b["task_id"], timeout=args.rojo_evidence_timeout)
            print(f"taskB Rojo helper evidence ok task={desc_b['task_id']}", flush=True)
            task_b_rojo_log = wait_studio_rojo_connected(
                desc_b["task_id"],
                "helper2-sync-probe" if sync_probe_marker else "runtime-stop-local",
                since=studio_log_since,
                timeout=args.rojo_evidence_timeout,
            )
            print(f"taskB Studio Rojo connected evidence ok log={task_b_rojo_log}", flush=True)

        if args.direct_api_only:
            expect_http_status(f"{HELPER_BASE_URL}/session/not-a-real-task/studio/screenshot", 404)
            expect_http_status(f"{HELPER_BASE_URL}/session/not-a-real-task/studio/play", 404, method="POST")
            check_direct_api_error_states(args.place_id)
            print("direct task API negative checks ok", flush=True)
        else:
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
            print(
                "negative checks ok: "
                f"old={old_error}; bad_task={bad_task_error}; bad_mode={bad_mode_error}",
                flush=True,
            )
        expect_http_status(f"{HELPER_BASE_URL}/v1/rojo/config?place_id={args.place_id}", 403)
        expect_http_status(f"{HELPER_BASE_URL}/plugin/mcp2/pull_command?mode=edit&mode_seq=1", 403)
        expect_external_response_result_ignored()
        print("external plugin route negative checks ok", flush=True)

        shot_a = None
        shot_b = None
        for cycle in range(1, args.cycles + 1):
            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                if args.direct_api_only:
                    future_play_a = executor.submit(call_direct_for_executor, "play", desc_a["task_id"])
                    future_play_b = executor.submit(call_direct_for_executor, "play", desc_b["task_id"])
                else:
                    future_play_a = executor.submit(call_tool_for_executor, "helper2_studio_play", desc_a["task_id"])
                    future_play_b = executor.submit(call_tool_for_executor, "helper2_studio_play", desc_b["task_id"])
                mode_samples: list[str] = []
                for _ in range(12):
                    if args.direct_api_only:
                        mode_samples.append(json.dumps(direct_task_mode(desc_a["task_id"], timeout=8), ensure_ascii=False))
                        mode_samples.append(json.dumps(direct_task_mode(desc_b["task_id"], timeout=8), ensure_ascii=False))
                    else:
                        mode_samples.append(mcp_tool("helper2_studio_mode", {"task_id": desc_a["task_id"]}, timeout=8)[1])
                        mode_samples.append(mcp_tool("helper2_studio_mode", {"task_id": desc_b["task_id"]}, timeout=8)[1])
                    time.sleep(0.5)
                play_a = future_play_a.result(timeout=150)
                play_b = future_play_b.result(timeout=150)
            if sync_probe_marker:
                marker_log_a = wait_studio_sync_probe(
                    desc_a["task_id"],
                    sync_probe_marker,
                    since=studio_log_since,
                    timeout=args.sync_probe_timeout,
                )
                marker_log_b = wait_studio_sync_probe(
                    desc_b["task_id"],
                    sync_probe_marker,
                    since=studio_log_since,
                    timeout=args.sync_probe_timeout,
                )
                print(f"sync probe marker observed in Studio logs: A={marker_log_a} B={marker_log_b}", flush=True)
            assert_no_studio_rojo_failures([desc_a["task_id"], desc_b["task_id"]], since=studio_log_since)
            print(
                f"cycle {cycle}/{args.cycles} concurrent play ok "
                f"A={play_a.get('mode')} B={play_b.get('mode')} samples={len(mode_samples)}",
                flush=True,
            )
            wait_mode_fn(desc_a["task_id"], "play_server", timeout=30)
            wait_mode_fn(desc_b["task_id"], "play_server", timeout=30)

            if cycle == 1 or cycle == args.cycles:
                if args.direct_api_only:
                    shot_a = capture_task_screenshot(desc_a["task_id"])
                    shot_b = capture_task_screenshot(desc_b["task_id"])
                else:
                    shot_a = mcp_tool("helper2_studio_screenshot", {"task_id": desc_a["task_id"]}, timeout=40)[2]
                    shot_b = mcp_tool("helper2_studio_screenshot", {"task_id": desc_b["task_id"]}, timeout=40)[2]
                require_screenshot_file(shot_a)
                require_screenshot_file(shot_b)
                print(f"cycle {cycle} screenshots ok A={shot_a['screenshot']} B={shot_b['screenshot']}", flush=True)

            with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
                if args.direct_api_only:
                    future_stop_a1 = executor.submit(call_direct_for_executor, "stop", desc_a["task_id"])
                else:
                    future_stop_a1 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_a["task_id"])
                time.sleep(0.1)
                if args.direct_api_only:
                    future_stop_a2 = executor.submit(call_direct_for_executor, "stop", desc_a["task_id"])
                    future_stop_b1 = executor.submit(call_direct_for_executor, "stop", desc_b["task_id"])
                else:
                    future_stop_a2 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_a["task_id"])
                    future_stop_b1 = executor.submit(call_tool_for_executor, "helper2_studio_stop", desc_b["task_id"])
                time.sleep(0.1)
                if args.direct_api_only:
                    future_stop_b2 = executor.submit(call_direct_for_executor, "stop", desc_b["task_id"])
                else:
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
            wait_mode_fn(desc_a["task_id"], "edit", timeout=30)
            wait_mode_fn(desc_b["task_id"], "edit", timeout=30)

        if args.helper_restart:
            print("terminating helper to simulate local helper/network outage", flush=True)
            old_helper_pid = helper.pid
            terminate_process(helper)
            helper = None
            time.sleep(8)
            helper = start_helper_process(bin_dir, logs_dir, "helper-restarted.log")
            wait_health()
            print(f"helper restarted old_pid={old_helper_pid} new_pid={helper.pid}", flush=True)
            wait_mode_fn(desc_a["task_id"], "edit", timeout=150)
            wait_mode_fn(desc_b["task_id"], "edit", timeout=150)
            print("helper restart recovery ok: both task sessions returned to edit", flush=True)

        subprocess.run(["taskkill", "/PID", str(agent_b.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        agent_b = None
        print("killed task-agent B; waiting lease expiry", flush=True)
        time.sleep(38)
        status_b = http_json("GET", f"{HELPER_BASE_URL}/session/{desc_b['task_id']}/status", timeout=10)
        if status_b.get("state") != "expired":
            raise StressError(f"expected task B expired after killed agent, got {status_b}")
        if args.direct_api_only:
            expired_play = http_json_expect_error("POST", f"{HELPER_BASE_URL}/session/{desc_b['task_id']}/studio/play", 409)
            if expired_play.get("code") != "task_not_live":
                raise StressError(f"expected task_not_live for expired task play, got {expired_play}")
            expired_screenshot = http_json_expect_error("GET", f"{HELPER_BASE_URL}/session/{desc_b['task_id']}/studio/screenshot", 409)
            if expired_screenshot.get("code") != "task_not_live":
                raise StressError(f"expected task_not_live for expired task screenshot, got {expired_screenshot}")
            expired_mode = direct_task_mode(desc_b["task_id"])
            if expired_mode.get("available") or expired_mode.get("state") != "expired":
                raise StressError(f"expected expired task mode unavailable, got {expired_mode}")
        summary = http_json("GET", f"{HELPER_BASE_URL}/studio/summary", timeout=10)
        b_studios = [studio for studio in summary.get("studios", []) if studio.get("owner_id") == desc_b["task_id"]]
        if b_studios:
            raise StressError(f"expected no task B studios after expiry, got {b_studios}")
        mode_a_after = wait_mode_fn(desc_a["task_id"], "edit", timeout=30)
        if args.direct_api_only:
            shot_a_after_expiry = capture_task_screenshot(desc_a["task_id"])
            require_screenshot_file(shot_a_after_expiry)
            print(f"post-expiry taskA screenshot ok {shot_a_after_expiry['screenshot']}", flush=True)
            direct_task_play(desc_a["task_id"])
            wait_mode_direct(desc_a["task_id"], "play_server", timeout=30)
            direct_task_stop(desc_a["task_id"])
            wait_mode_direct(desc_a["task_id"], "edit", timeout=30)
            summary = http_json("GET", f"{HELPER_BASE_URL}/studio/summary", timeout=10)
            require_task_channel_idle(summary, desc_a["task_id"])
            require_helper_pid_evidence(logs_dir / "helper.log", [desc_a["task_id"], desc_b["task_id"]])
        print(f"lease expiry isolation ok B={status_b['state']} A={mode_a_after['mode']}", flush=True)

        return {
            "ok": True,
            "run_dir": str(run_dir),
            "taskA": desc_a["task_id"],
            "taskB": desc_b["task_id"],
            "syncProbeMarker": sync_probe_marker,
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
    parser.add_argument("--direct-api-only", action="store_true", help="Use helper2 HTTP task APIs directly instead of helper2 MCP tools.")
    parser.add_argument("--require-rojo-plugin", action="store_true", help="Require real Studio-side Rojo plugin traffic through helper2.")
    parser.add_argument("--rojo-evidence-timeout", type=float, default=90)
    parser.add_argument("--sync-probe", action="store_true", help="Use a generated Rojo project with a server script and require its Studio output marker.")
    parser.add_argument("--sync-probe-timeout", type=float, default=45)
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
