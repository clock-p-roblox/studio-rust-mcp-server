#!/usr/bin/env python3
from __future__ import annotations

import argparse
import json
import os
import re
import socket
import subprocess
import sys
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any


HELPER_LOCAL_URL = "http://127.0.0.1:44750"
DEFAULT_PLACE_ID = "105986423068266"
ROBLOX_SPACE = Path(__file__).resolve().parents[2]
DEFAULT_ROJO = ROBLOX_SPACE / "rojo" / "target" / "release" / "rojo.exe"
ROJO_REPO = ROBLOX_SPACE / "rojo"
PLUGIN_PATH = Path(os.environ["LOCALAPPDATA"]) / "Roblox" / "Plugins" / "MCP2Plugin.rbxm"
ROJO_PLUGIN_PATH = Path(os.environ["LOCALAPPDATA"]) / "Roblox" / "Plugins" / "Rojo.rbxm"
PUBLIC_HOST_PATTERN = re.compile(r"^[a-z0-9.-]+\.dev\.clock-p\.com$")


class PublicMatrixError(RuntimeError):
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
        raise PublicMatrixError(f"command failed ({result.returncode}): {' '.join(args)}\n{result.stdout}")
    if result.stdout.strip():
        print(result.stdout.strip(), flush=True)
    return result


def go_bin() -> str:
    explicit = os.environ.get("GO_BIN", "").strip()
    if explicit:
        return explicit
    common = Path(r"K:\Program Files\Go\bin\go.exe")
    if common.exists():
        return str(common)
    return "go"


def read_text_trim(path: Path) -> str:
    if not path.exists():
        return ""
    return path.read_text(encoding="utf-8", errors="replace").strip()


def token_file_candidates(workspace: Path) -> list[Path]:
    paths = [workspace / ".dev.clock-p.com" / "feishu-token"]
    if appdata := os.environ.get("APPDATA"):
        paths.append(Path(appdata) / "dev.clock-p.com" / "feishu-token")
    if home := os.environ.get("HOME"):
        paths.append(Path(home) / ".dev.clock-p.com" / "feishu-token")
    return paths


def user_name_candidates(workspace: Path) -> list[Path]:
    paths = [workspace / ".dev.clock-p.com" / "feishu-user_name"]
    if appdata := os.environ.get("APPDATA"):
        paths.append(Path(appdata) / "dev.clock-p.com" / "feishu-user_name")
    if home := os.environ.get("HOME"):
        paths.append(Path(home) / ".dev.clock-p.com" / "feishu-user_name")
    return paths


def machine_name_candidates() -> list[Path]:
    paths: list[Path] = []
    if appdata := os.environ.get("APPDATA"):
        paths.append(Path(appdata) / "dev.clock-p.com" / "machine_name")
    if home := os.environ.get("HOME"):
        paths.append(Path(home) / ".dev.clock-p.com" / "machine_name")
    return paths


def resolve_token_file(workspace: Path) -> Path:
    for path in token_file_candidates(workspace):
        if path.is_file():
            return path.resolve()
    raise PublicMatrixError("cannot resolve feishu-token for public helper2 test")


def resolve_bearer_token(workspace: Path) -> str:
    if token := os.environ.get("ROBLOX_HELPER2_BEARER_TOKEN", "").strip():
        return token
    return read_text_trim(resolve_token_file(workspace))


def resolve_user_name(workspace: Path, explicit: str) -> str:
    if explicit.strip():
        return explicit.strip().lower()
    for path in user_name_candidates(workspace):
        value = read_text_trim(path).lower()
        if value:
            return value
    raise PublicMatrixError("cannot resolve feishu-user_name for public helper2 test")


def default_machine_name() -> str:
    for path in machine_name_candidates():
        value = read_text_trim(path).lower()
        if value:
            return value
    value = socket.gethostname().lower()
    value = re.sub(r"[^a-z0-9-]+", "-", value).strip("-")
    raise PublicMatrixError(f"cannot resolve machine_name from system identity files; hostname fallback would be {value!r}")


def helper_public_url(machine_name: str, user_name: str) -> str:
    host = f"roblox-helper-{machine_name}-{user_name}-user.dev.clock-p.com"
    if not PUBLIC_HOST_PATTERN.fullmatch(host):
        raise PublicMatrixError(f"invalid public helper host: {host}")
    return "https://" + host


def http_json(
    method: str,
    url: str,
    payload: dict[str, Any] | None = None,
    *,
    token: str,
    timeout: float = 30,
) -> Any:
    data = None
    headers = {"Accept": "application/json", "Authorization": f"Bearer {token}"}
    if payload is not None:
        data = json.dumps(payload, separators=(",", ":")).encode("utf-8")
        headers["Content-Type"] = "application/json"
    request = urllib.request.Request(url, data=data, headers=headers, method=method)
    with urllib.request.urlopen(request, timeout=timeout) as response:
        body = response.read()
    if not body:
        return None
    return json.loads(body.decode("utf-8"))


def expect_public_auth_failure(url: str, token: str | None) -> int:
    headers = {"Accept": "application/json"}
    if token is not None:
        headers["Authorization"] = f"Bearer {token}"
    request = urllib.request.Request(url, headers=headers, method="GET")
    try:
        with urllib.request.urlopen(request, timeout=15) as response:
            body = response.read().decode("utf-8", errors="replace")
        raise PublicMatrixError(f"expected public auth failure from {url}, got success: {body[:500]}")
    except urllib.error.HTTPError as exc:
        if 400 <= exc.code < 500:
            return exc.code
        body = exc.read().decode("utf-8", errors="replace")
        raise PublicMatrixError(f"expected public auth failure from {url}, got HTTP {exc.code}: {body[:500]}") from exc


def mcp_raw(base_url: str, token: str, method: str, params: dict[str, Any] | None = None, *, timeout: float = 120) -> dict[str, Any]:
    body: dict[str, Any] = {
        "jsonrpc": "2.0",
        "id": int(time.time_ns() % 2_000_000_000),
        "method": method,
    }
    if params is not None:
        body["params"] = params
    return http_json("POST", f"{base_url}/mcp", body, token=token, timeout=timeout)


def mcp_tool(base_url: str, token: str, name: str, arguments: dict[str, Any], *, timeout: float = 150) -> dict[str, Any]:
    response = mcp_raw(base_url, token, "tools/call", {"name": name, "arguments": arguments}, timeout=timeout)
    if "error" in response:
        raise PublicMatrixError(f"MCP RPC error for {name}: {response['error']}")
    result = response["result"]
    text = result["content"][0]["text"]
    if result.get("isError"):
        raise PublicMatrixError(f"MCP tool error for {name}: {text}")
    return json.loads(text)


def wait_until(label: str, timeout: float, fn) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception as exc:  # noqa: BLE001 - surface last diagnostic.
            last_error = exc
        time.sleep(1)
    if last_error:
        raise PublicMatrixError(f"timed out waiting for {label}: {last_error}") from last_error
    raise PublicMatrixError(f"timed out waiting for {label}")


def wait_public_ready(public_url: str, token: str, *, timeout: float) -> dict[str, Any]:
    def read_status() -> dict[str, Any] | None:
        payload = http_json("GET", f"{public_url}/status", token=token, timeout=10)
        exposure = payload.get("public_exposure")
        if not isinstance(exposure, dict):
            return None
        if exposure.get("state") == "running" and exposure.get("public_url") == public_url:
            return payload
        return None

    return wait_until(f"public helper ready {public_url}", timeout, read_status)


def wait_descriptor(workspace: Path) -> dict[str, Any]:
    descriptor_path = workspace / ".clock-p" / "session.json"

    def read_descriptor() -> dict[str, Any] | None:
        if not descriptor_path.exists():
            return None
        return json.loads(descriptor_path.read_text(encoding="utf-8"))

    return wait_until(f"descriptor {descriptor_path}", 30, read_descriptor)


def wait_session_live(base_url: str, token: str, task_id: str) -> dict[str, Any]:
    def read_status() -> dict[str, Any] | None:
        payload = http_json("GET", f"{base_url}/session/{task_id}/status", token=token, timeout=10)
        if payload.get("state") == "live":
            return payload
        return None

    return wait_until(f"public task live {task_id}", 90, read_status)


def wait_mode(base_url: str, token: str, task_id: str, expected: str, *, timeout: float) -> dict[str, Any]:
    last_payload: dict[str, Any] | None = None

    def read_mode() -> dict[str, Any] | None:
        nonlocal last_payload
        payload = http_json("GET", f"{base_url}/session/{task_id}/studio/mode", token=token, timeout=15)
        last_payload = payload
        if payload.get("available") and payload.get("mode") == expected:
            return payload
        return None

    try:
        return wait_until(f"public task {task_id} mode {expected}", timeout, read_mode)
    except PublicMatrixError as exc:
        raise PublicMatrixError(f"{exc}; last mode payload={last_payload}") from exc


def require_screenshot_file(payload: dict[str, Any]) -> None:
    screenshot = payload.get("screenshot")
    if not isinstance(screenshot, dict):
        raise PublicMatrixError(f"screenshot payload missing: {payload}")
    path = Path(str(screenshot.get("path", "")))
    if not path.exists() or path.stat().st_size <= 0:
        raise PublicMatrixError(f"screenshot file missing or empty: {screenshot}")
    if int(screenshot.get("bytes") or 0) <= 0 or int(screenshot.get("studio_pid") or 0) <= 0:
        raise PublicMatrixError(f"screenshot metadata is not real Studio evidence: {screenshot}")


def start_process(args: list[str], log_path: Path, *, env: dict[str, str] | None = None) -> subprocess.Popen[str]:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log = log_path.open("w", encoding="utf-8")
    return subprocess.Popen(
        args,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
        env=env,
        creationflags=subprocess.CREATE_NO_WINDOW if sys.platform == "win32" else 0,
    )


def terminate_process(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    process.terminate()
    try:
        process.wait(timeout=8)
        return
    except subprocess.TimeoutExpired:
        pass
    subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def matching_test_processes() -> list[dict[str, Any]]:
    ps = (
        "Get-Process | Where-Object { $_.ProcessName -match "
        "'studio-helper|task-agent|RobloxStudioBeta|clockbridge-cli|rojo' } | "
        "Select-Object Id,ProcessName,Path | ConvertTo-Json -Depth 3"
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


def assert_no_matching_processes() -> None:
    processes = matching_test_processes()
    if processes:
        formatted = json.dumps(processes, ensure_ascii=False, indent=2)
        raise PublicMatrixError("public matrix left helper2/Studio/Rojo/clockbridge processes running:\n" + formatted)


def ensure_no_existing_test_processes(*, kill_existing: bool) -> None:
    processes = matching_test_processes()
    if not processes:
        return
    if kill_existing:
        cleanup_matching_processes()
        return
    formatted = json.dumps(processes, ensure_ascii=False, indent=2)
    raise PublicMatrixError("existing helper2/Studio/Rojo/clockbridge processes are running; pass --kill-existing on a dedicated test machine:\n" + formatted)


def prepare_binaries(root: Path, bin_dir: Path, rojo: Path) -> None:
    bin_dir.mkdir(parents=True, exist_ok=True)
    go_helper = root / "go-helper"
    go = go_bin()
    run_command([go, "build", "-o", str(bin_dir / "studio-helper.exe"), r".\cmd\studio-helper"], cwd=go_helper, timeout=120)
    run_command([go, "build", "-o", str(bin_dir / "task-agent.exe"), r".\cmd\task-agent"], cwd=go_helper, timeout=120)
    if not rojo.exists():
        raise PublicMatrixError(f"rojo executable does not exist: {rojo}")
    run_command([str(rojo), "build", r"plugin-mcp2\default.project.json", "--plugin", "MCP2Plugin.rbxm"], cwd=root, timeout=60)
    if not PLUGIN_PATH.exists():
        raise PublicMatrixError(f"MCP2 plugin was not installed at {PLUGIN_PATH}")
    run_command([str(rojo), "build", "plugin.project.json", "--plugin", "Rojo.rbxm"], cwd=ROJO_REPO, timeout=60)
    if not ROJO_PLUGIN_PATH.exists():
        raise PublicMatrixError(f"Rojo plugin was not installed at {ROJO_PLUGIN_PATH}")


def create_project(parent: Path) -> Path:
    project = parent / "public-matrix-project"
    src = project / "src"
    src.mkdir(parents=True, exist_ok=True)
    (project / "default.project.json").write_text(
        json.dumps(
            {
                "name": "helper2-public-matrix",
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
    (src / "PublicMatrix.server.lua").write_text('print("CLOCKP_HELPER2_PUBLIC_MATRIX_READY")\n', encoding="utf-8")
    return project / "default.project.json"


def stop_agent(bin_dir: Path, workspace: Path) -> None:
    if not (workspace / ".clock-p" / "session.json").exists():
        return
    subprocess.run(
        [str(bin_dir / "task-agent.exe"), "stop", "--workspace", str(workspace)],
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
        timeout=10,
    )


def validate_descriptor(descriptor: dict[str, Any], public_url: str) -> str:
    helper = descriptor.get("helper")
    if not isinstance(helper, dict):
        raise PublicMatrixError(f"descriptor missing helper route: {descriptor}")
    if helper.get("base_url") != public_url:
        raise PublicMatrixError(f"descriptor helper.base_url is not public URL: {helper}")
    if helper.get("public_url") != public_url:
        raise PublicMatrixError(f"descriptor helper.public_url is not public URL: {helper}")
    task_id = descriptor.get("task_id")
    if not isinstance(task_id, str) or not task_id:
        raise PublicMatrixError(f"descriptor missing task_id: {descriptor}")
    rojo = descriptor.get("rojo")
    if not isinstance(rojo, dict):
        raise PublicMatrixError(f"descriptor missing rojo route: {descriptor}")
    upstream = rojo.get("upstream_url")
    if not isinstance(upstream, str) or not upstream.startswith("https://") or "-rojo-" not in upstream:
        raise PublicMatrixError(f"descriptor rojo.upstream_url is not public URL: {rojo}")
    return task_id


def run_matrix(args: argparse.Namespace) -> dict[str, Any]:
    root = repo_root()
    run_dir = Path(tempfile.mkdtemp(prefix="helper2-public-matrix-"))
    bin_dir = run_dir / "bin"
    logs_dir = run_dir / "logs"
    workspace = run_dir / "workspace"
    workspace.mkdir(parents=True)
    machine_name = args.machine_name or default_machine_name()
    user_name = resolve_user_name(workspace, args.user)
    public_url = helper_public_url(machine_name, user_name)
    bearer_token = resolve_bearer_token(workspace)
    project = create_project(run_dir)
    helper: subprocess.Popen[str] | None = None
    agent: subprocess.Popen[str] | None = None

    print(f"run_dir={run_dir}", flush=True)
    print(f"public_url={public_url}", flush=True)
    prepare_binaries(root, bin_dir, args.rojo)
    ensure_no_existing_test_processes(kill_existing=args.kill_existing)

    try:
        helper_args = [
                str(bin_dir / "studio-helper.exe"),
                "--addr",
                "127.0.0.1:44750",
                "--mcp2-stale-after",
                "20s",
                "--mcp2-stale-check-interval",
                "2s",
                "--register-domain=true",
        ]
        helper = start_process(
            helper_args,
            logs_dir / "helper.log",
        )
        wait_public_ready(public_url, bearer_token, timeout=args.public_ready_timeout)
        missing_status = expect_public_auth_failure(f"{public_url}/status", None)
        invalid_status = expect_public_auth_failure(f"{public_url}/status", "definitely-invalid-token")
        print(f"public auth checks ok missing={missing_status} invalid={invalid_status}", flush=True)

        env = os.environ.copy()
        env["ROBLOX_HELPER2_BEARER_TOKEN"] = bearer_token
        agent = start_process(
            [
                str(bin_dir / "task-agent.exe"),
                "start",
                "--workspace",
                str(workspace),
                "--environment",
                "public",
                "--machine_name",
                machine_name,
                "--user",
                user_name,
                "--place_id",
                args.place_id,
                "--rojo-bin",
                str(args.rojo),
                "--project",
                str(project),
            ],
            logs_dir / "agent.log",
            env=env,
        )
        descriptor = wait_descriptor(workspace)
        task_id = validate_descriptor(descriptor, public_url)
        print(f"task={task_id} agentPid={agent.pid}", flush=True)
        wait_session_live(public_url, bearer_token, task_id)
        initial_mode = wait_mode(public_url, bearer_token, task_id, "edit", timeout=args.initial_mode_timeout)
        print(f"initial public mode={initial_mode.get('mode')}", flush=True)

        status_from_mcp_status = http_json("GET", f"{public_url}/status?task_id={task_id}", token=bearer_token, timeout=20)
        if status_from_mcp_status.get("task_id") != task_id:
            raise PublicMatrixError(f"public /status task route mismatch: {status_from_mcp_status}")
        status_from_direct = http_json("GET", f"{public_url}/session/{task_id}/status", token=bearer_token, timeout=20)
        if status_from_direct.get("state") != "live":
            raise PublicMatrixError(f"public direct status not live: {status_from_direct}")
        direct_runtime_log = http_json("GET", f"{public_url}/session/{task_id}/runtime-log?limit=10", token=bearer_token, timeout=20)
        if direct_runtime_log.get("task_id") != task_id or "entries" not in direct_runtime_log:
            raise PublicMatrixError(f"public runtime-log read failed: {direct_runtime_log}")
        direct_screenshot = http_json("GET", f"{public_url}/session/{task_id}/studio/screenshot", token=bearer_token, timeout=60)
        require_screenshot_file(direct_screenshot)
        direct_play = http_json("POST", f"{public_url}/session/{task_id}/studio/play", token=bearer_token, timeout=150)
        wait_mode(public_url, bearer_token, task_id, "play_server", timeout=45)
        direct_stop = http_json("POST", f"{public_url}/session/{task_id}/studio/stop", token=bearer_token, timeout=150)
        wait_mode(public_url, bearer_token, task_id, "edit", timeout=45)
        print("public direct route matrix ok", flush=True)

        init = mcp_raw(public_url, bearer_token, "initialize", {"protocolVersion": "2025-11-25"}, timeout=20)
        if init.get("result", {}).get("serverInfo", {}).get("name") != "studio-helper2":
            raise PublicMatrixError(f"public MCP initialize failed: {init}")
        tools_response = mcp_raw(public_url, bearer_token, "tools/list", timeout=20)
        tool_names = [tool["name"] for tool in tools_response["result"]["tools"]]
        for required in (
            "helper2_status",
            "helper2_studio_play",
            "helper2_studio_stop",
            "helper2_studio_mode",
            "helper2_studio_screenshot",
            "helper2_runtime_log",
        ):
            if required not in tool_names:
                raise PublicMatrixError(f"public MCP tools/list missing {required}: {tool_names}")
        for forbidden in ("launch_studio_session", "start_stop_play", "take_screenshot", "runtime_log_read"):
            if forbidden in tool_names:
                raise PublicMatrixError(f"public MCP tools/list exposes old tool {forbidden}: {tool_names}")
        mcp_status = mcp_tool(public_url, bearer_token, "helper2_status", {"task_id": task_id}, timeout=20)
        if mcp_status.get("task_id") != task_id:
            raise PublicMatrixError(f"public MCP helper2_status mismatch: {mcp_status}")
        mcp_mode = mcp_tool(public_url, bearer_token, "helper2_studio_mode", {"task_id": task_id}, timeout=20)
        if not mcp_mode.get("available"):
            raise PublicMatrixError(f"public MCP mode unavailable: {mcp_mode}")
        mcp_screenshot = mcp_tool(public_url, bearer_token, "helper2_studio_screenshot", {"task_id": task_id}, timeout=60)
        require_screenshot_file(mcp_screenshot)
        mcp_tool(public_url, bearer_token, "helper2_studio_play", {"task_id": task_id}, timeout=150)
        wait_mode(public_url, bearer_token, task_id, "play_server", timeout=45)
        mcp_tool(public_url, bearer_token, "helper2_studio_stop", {"task_id": task_id}, timeout=150)
        wait_mode(public_url, bearer_token, task_id, "edit", timeout=45)
        mcp_runtime_log = mcp_tool(public_url, bearer_token, "helper2_runtime_log", {"task_id": task_id, "limit": 10}, timeout=20)
        if mcp_runtime_log.get("task_id") != task_id or "entries" not in mcp_runtime_log:
            raise PublicMatrixError(f"public MCP runtime-log read failed: {mcp_runtime_log}")
        print("public MCP route matrix ok", flush=True)

        return {
            "ok": True,
            "run_dir": str(run_dir),
            "public_url": public_url,
            "task_id": task_id,
            "direct_play_mode": direct_play.get("mode"),
            "direct_stop_mode": direct_stop.get("mode"),
            "direct_screenshot": direct_screenshot.get("screenshot"),
            "mcp_screenshot": mcp_screenshot.get("screenshot"),
        }
    finally:
        stop_agent(bin_dir, workspace)
        terminate_process(agent)
        terminate_process(helper)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Run real helper2 public exposure route matrix through dev.clock-p.com.")
    parser.add_argument("--place-id", default=DEFAULT_PLACE_ID)
    parser.add_argument("--rojo", type=Path, default=DEFAULT_ROJO)
    parser.add_argument("--machine-name", default="", help="public helper machine name; defaults to system machine_name file")
    parser.add_argument("--user", default="", help="clock-p user name; defaults to feishu-user_name")
    parser.add_argument("--public-ready-timeout", type=float, default=60)
    parser.add_argument("--initial-mode-timeout", type=float, default=180)
    parser.add_argument("--kill-existing", action="store_true", help="Kill existing helper2/Studio/Rojo/clockbridge processes before the test.")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    try:
        result = run_matrix(args)
        assert_no_matching_processes()
    except Exception as exc:  # noqa: BLE001 - CLI should report concise failure.
        print(f"PUBLIC_MATRIX_FAILED: {exc}", file=sys.stderr, flush=True)
        return 1
    print("PUBLIC_MATRIX_OK " + json.dumps(result, ensure_ascii=False, default=str), flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
