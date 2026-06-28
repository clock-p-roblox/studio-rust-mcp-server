from __future__ import annotations

import argparse
import ctypes
import json
import socket
import subprocess
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
GO_HELPER = ROOT / "go-helper"
DEFAULT_ROJO = Path(r"D:\roblox_space\rojo\target\release\rojo.exe")

PROCESS_QUERY_LIMITED_INFORMATION = 0x1000
PROCESS_TERMINATE = 0x0001
STILL_ACTIVE = 259
TH32CS_SNAPPROCESS = 0x00000002
INVALID_HANDLE_VALUE = ctypes.c_void_p(-1).value


class PROCESSENTRY32W(ctypes.Structure):
    _fields_ = [
        ("dwSize", ctypes.c_ulong),
        ("cntUsage", ctypes.c_ulong),
        ("th32ProcessID", ctypes.c_ulong),
        ("th32DefaultHeapID", ctypes.c_void_p),
        ("th32ModuleID", ctypes.c_ulong),
        ("cntThreads", ctypes.c_ulong),
        ("th32ParentProcessID", ctypes.c_ulong),
        ("pcPriClassBase", ctypes.c_long),
        ("dwFlags", ctypes.c_ulong),
        ("szExeFile", ctypes.c_wchar * 260),
    ]


class Phase9Error(RuntimeError):
    pass


class HelperState:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.heartbeats: list[dict[str, Any]] = []
        self.releases: list[dict[str, Any]] = []

    def record_heartbeat(self, task_id: str, payload: dict[str, Any]) -> None:
        with self.lock:
            self.heartbeats.append({"task_id": task_id, "payload": payload, "at": time.time()})

    def record_release(self, task_id: str, payload: dict[str, Any]) -> None:
        with self.lock:
            self.releases.append({"task_id": task_id, "payload": payload, "at": time.time()})

    def has_heartbeat(self, task_id: str) -> bool:
        with self.lock:
            return any(item["task_id"] == task_id for item in self.heartbeats)

    def has_release(self, task_id: str) -> bool:
        with self.lock:
            return any(item["task_id"] == task_id for item in self.releases)


class HelperHandler(BaseHTTPRequestHandler):
    server: "HelperHTTPServer"

    def log_message(self, format: str, *args: object) -> None:
        return

    def do_POST(self) -> None:
        parsed = urllib.parse.urlparse(self.path)
        parts = [part for part in parsed.path.split("/") if part]
        if len(parts) == 3 and parts[0] == "session":
            task_id = parts[1]
            action = parts[2]
            payload = self._read_json()
            if action == "heartbeat":
                self.server.state.record_heartbeat(task_id, payload)
                self._write_json(200, {"ok": True, "task_id": task_id, "state": "live"})
                return
            if action == "release":
                self.server.state.record_release(task_id, payload)
                self._write_json(200, {"ok": True, "task_id": task_id, "state": "ended", "killed_pids": []})
                return
        self._write_json(404, {"ok": False, "error": "not found"})

    def _read_json(self) -> dict[str, Any]:
        length = int(self.headers.get("Content-Length") or "0")
        if length <= 0:
            return {}
        body = self.rfile.read(length)
        return json.loads(body.decode("utf-8"))

    def _write_json(self, status: int, payload: dict[str, Any]) -> None:
        body = json.dumps(payload).encode("utf-8") + b"\n"
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class HelperHTTPServer(ThreadingHTTPServer):
    def __init__(self, addr: tuple[str, int], state: HelperState) -> None:
        super().__init__(addr, HelperHandler)
        self.state = state


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


def process_snapshot() -> list[tuple[int, int, str]]:
    handle = ctypes.windll.kernel32.CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0)
    if handle == INVALID_HANDLE_VALUE:
        raise Phase9Error("CreateToolhelp32Snapshot failed")
    try:
        entry = PROCESSENTRY32W()
        entry.dwSize = ctypes.sizeof(PROCESSENTRY32W)
        rows: list[tuple[int, int, str]] = []
        ok = ctypes.windll.kernel32.Process32FirstW(handle, ctypes.byref(entry))
        while ok:
            rows.append((int(entry.th32ProcessID), int(entry.th32ParentProcessID), entry.szExeFile))
            ok = ctypes.windll.kernel32.Process32NextW(handle, ctypes.byref(entry))
        return rows
    finally:
        ctypes.windll.kernel32.CloseHandle(handle)


def descendant_pids(root_pid: int) -> list[int]:
    rows = process_snapshot()
    by_parent: dict[int, list[int]] = {}
    for pid, parent_pid, _name in rows:
        by_parent.setdefault(parent_pid, []).append(pid)
    pending = list(by_parent.get(root_pid, []))
    result: list[int] = []
    while pending:
        pid = pending.pop()
        result.append(pid)
        pending.extend(by_parent.get(pid, []))
    return result


def wait_until(label: str, timeout: float, fn) -> Any:
    deadline = time.monotonic() + timeout
    last_error: Exception | None = None
    while time.monotonic() < deadline:
        try:
            value = fn()
            if value:
                return value
        except Exception as exc:  # noqa: BLE001 - surfaced below with context.
            last_error = exc
        time.sleep(0.25)
    if last_error is not None:
        raise Phase9Error(f"timed out waiting for {label}: {last_error}") from last_error
    raise Phase9Error(f"timed out waiting for {label}")


def wait_process_exit(pid: int, timeout: float) -> None:
    wait_until(f"process {pid} exit", timeout, lambda: not process_is_running(pid))


def wait_all_processes_exit(pids: list[int], timeout: float) -> None:
    wait_until(
        f"processes exit {pids}",
        timeout,
        lambda: all(not process_is_running(pid) for pid in pids),
    )


def tcp_port_open(url: str) -> bool:
    parsed = urllib.parse.urlparse(url)
    host = parsed.hostname or "127.0.0.1"
    port = parsed.port
    if port is None:
        return False
    try:
        with socket.create_connection((host, port), timeout=0.5):
            return True
    except OSError:
        return False


def wait_port_closed(url: str, timeout: float) -> None:
    wait_until(f"{url} port closed", timeout, lambda: not tcp_port_open(url))


def wait_port_open(url: str, timeout: float) -> None:
    wait_until(f"{url} port open", timeout, lambda: tcp_port_open(url))


def http_json(url: str, *, timeout: float = 3.0) -> dict[str, Any]:
    with urllib.request.urlopen(url, timeout=timeout) as response:
        return json.loads(response.read().decode("utf-8"))


def wait_descriptor(workspace: Path, *, previous_task_id: str | None = None) -> dict[str, Any]:
    path = workspace / ".clock-p" / "session.json"

    def read() -> dict[str, Any] | None:
        if not path.is_file():
            return None
        payload = json.loads(path.read_text(encoding="utf-8"))
        if previous_task_id is not None and payload.get("task_id") == previous_task_id:
            return None
        return payload

    return wait_until(f"descriptor {path}", 15, read)


def log_tail(path: Path | None, limit: int = 6000) -> str:
    if path is None or not path.is_file():
        return ""
    text = path.read_text(encoding="utf-8", errors="replace")
    return text[-limit:]


def wait_status(
    status_url: str,
    predicate,
    *,
    timeout: float = 30.0,
    process: subprocess.Popen[str] | None = None,
    log_path: Path | None = None,
) -> dict[str, Any]:
    last: dict[str, Any] | None = None

    def read() -> dict[str, Any] | None:
        nonlocal last
        if process is not None and process.poll() is not None:
            raise Phase9Error(
                f"task-agent process exited early with code {process.returncode}; "
                f"log_tail={log_tail(log_path)!r}"
            )
        last = http_json(status_url)
        return last if predicate(last) else None

    try:
        return wait_until(f"status predicate for {status_url}", timeout, read)
    except Phase9Error as exc:
        raise Phase9Error(f"{exc}; last_status={last}") from exc


def run_command(args: list[str], *, cwd: Path, timeout: float) -> None:
    completed = subprocess.run(args, cwd=cwd, text=True, capture_output=True, timeout=timeout)
    if completed.returncode != 0:
        raise Phase9Error(
            f"command failed ({completed.returncode}): {' '.join(args)}\n"
            f"stdout={completed.stdout}\nstderr={completed.stderr}"
        )


def start_agent(
    task_agent: Path,
    workspace: Path,
    helper_url: str,
    rojo_bin: Path,
    project: Path,
    machine_name: str,
    place_id: str,
    log_path: Path,
) -> subprocess.Popen[str]:
    log_path.parent.mkdir(parents=True, exist_ok=True)
    log = log_path.open("w", encoding="utf-8")
    return subprocess.Popen(
        [
            str(task_agent),
            "start",
            "--workspace",
            str(workspace),
            "--environment",
            "local",
            "--machine_name",
            machine_name,
            "--place_id",
            place_id,
            "--helper-base-url",
            helper_url,
            "--rojo-bin",
            str(rojo_bin),
            "--project",
            str(project),
        ],
        cwd=ROOT,
        stdout=log,
        stderr=subprocess.STDOUT,
        text=True,
    )


def stop_agent_cli(task_agent: Path, workspace: Path) -> None:
    run_command([str(task_agent), "stop", "--workspace", str(workspace)], cwd=ROOT, timeout=10)


def require_running_agent(
    status_url: str,
    state: HelperState,
    task_id: str,
    process: subprocess.Popen[str],
    log_path: Path,
) -> dict[str, Any]:
    status = wait_status(
        status_url,
        lambda payload: payload.get("rojo", {}).get("state") == "running"
        and int(payload.get("rojo", {}).get("pid") or 0) > 0
        and payload.get("helper_registration_state") == "registered",
        timeout=40,
        process=process,
        log_path=log_path,
    )
    wait_until(f"helper heartbeat for {task_id}", 10, lambda: state.has_heartbeat(task_id))
    return status


def cleanup_process(process: subprocess.Popen[str] | None) -> None:
    if process is None or process.poll() is not None:
        return
    terminate_pid(process.pid)
    try:
        process.wait(timeout=5)
    except subprocess.TimeoutExpired:
        subprocess.run(["taskkill", "/PID", str(process.pid), "/T", "/F"], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)


def run_phase9(args: argparse.Namespace) -> dict[str, Any]:
    if not args.rojo.exists():
        raise Phase9Error(f"Rojo binary does not exist: {args.rojo}")
    if not args.project.exists():
        raise Phase9Error(f"Rojo project does not exist: {args.project}")

    run_root = Path(tempfile.mkdtemp(prefix="helper2-phase9-"))
    bin_dir = run_root / "bin"
    logs_dir = run_root / "logs"
    workspace = run_root / "workspace"
    bin_dir.mkdir(parents=True)
    workspace.mkdir(parents=True)
    task_agent = bin_dir / "task-agent.exe"
    agents: list[subprocess.Popen[str]] = []

    helper_state = HelperState()
    helper = HelperHTTPServer(("127.0.0.1", 0), helper_state)
    helper_thread = threading.Thread(target=helper.serve_forever, daemon=True)
    helper_thread.start()
    helper_url = f"http://127.0.0.1:{helper.server_port}"

    try:
        run_command(["go", "build", "-o", str(task_agent), r".\cmd\task-agent"], cwd=GO_HELPER, timeout=120)

        agent1_log = logs_dir / "agent1.log"
        agent1 = start_agent(task_agent, workspace, helper_url, args.rojo, args.project, "phase9-a", args.place_id, agent1_log)
        agents.append(agent1)
        desc1 = wait_descriptor(workspace)
        status1 = require_running_agent(desc1["task_agent_status_url"], helper_state, desc1["task_id"], agent1, agent1_log)
        rojo1_pid = int(status1["rojo"]["pid"])
        rojo_url = desc1["rojo"]["upstream_url"]
        wait_port_open(rojo_url, 10)

        terminate_pid(rojo1_pid)
        wait_process_exit(rojo1_pid, 10)
        status_after_rojo_kill = wait_status(
            desc1["task_agent_status_url"],
            lambda payload: payload.get("rojo", {}).get("state") == "running"
            and int(payload.get("rojo", {}).get("pid") or 0) > 0
            and int(payload.get("rojo", {}).get("pid") or 0) != rojo1_pid
            and int(payload.get("rojo", {}).get("restarts") or 0) >= 1,
            timeout=30,
        )
        if desc1["rojo"]["upstream_url"] != rojo_url:
            raise Phase9Error("Rojo upstream URL changed after restart")
        rojo_restart_pid = int(status_after_rojo_kill["rojo"]["pid"])
        wait_port_open(rojo_url, 10)

        agent2_log = logs_dir / "agent2.log"
        agent2 = start_agent(task_agent, workspace, helper_url, args.rojo, args.project, "phase9-b", args.place_id, agent2_log)
        agents.append(agent2)
        wait_process_exit(agent1.pid, 15)
        wait_until(f"helper release for {desc1['task_id']}", 10, lambda: helper_state.has_release(desc1["task_id"]))
        desc2 = wait_descriptor(workspace, previous_task_id=desc1["task_id"])
        status2 = require_running_agent(desc2["task_agent_status_url"], helper_state, desc2["task_id"], agent2, agent2_log)
        rojo2_pid = int(status2["rojo"]["pid"])
        rojo2_url = desc2["rojo"]["upstream_url"]

        stop_agent_cli(task_agent, workspace)
        wait_process_exit(agent2.pid, 15)
        wait_until(f"helper release for {desc2['task_id']}", 10, lambda: helper_state.has_release(desc2["task_id"]))
        wait_process_exit(rojo2_pid, 10)
        wait_port_closed(rojo2_url, 10)

        agent3_log = logs_dir / "agent3.log"
        agent3 = start_agent(task_agent, workspace, helper_url, args.rojo, args.project, "phase9-c", args.place_id, agent3_log)
        agents.append(agent3)
        desc3 = wait_descriptor(workspace, previous_task_id=desc2["task_id"])
        status3 = require_running_agent(desc3["task_agent_status_url"], helper_state, desc3["task_id"], agent3, agent3_log)
        rojo3_pid = int(status3["rojo"]["pid"])
        rojo3_url = desc3["rojo"]["upstream_url"]
        forced_child_pids = sorted(set(descendant_pids(agent3.pid) + [rojo3_pid]))
        terminate_pid(agent3.pid)
        wait_process_exit(agent3.pid, 10)
        wait_all_processes_exit(forced_child_pids, 10)
        wait_port_closed(rojo3_url, 10)
        wait_until(f"no descendants for forced task-agent {agent3.pid}", 10, lambda: len(descendant_pids(agent3.pid)) == 0)

        return {
            "ok": True,
            "run_dir": str(run_root),
            "helper_url": helper_url,
            "task_ids": [desc1["task_id"], desc2["task_id"], desc3["task_id"]],
            "rojo_restart": {"old_pid": rojo1_pid, "new_pid": rojo_restart_pid, "upstream_url": rojo_url},
            "graceful_stop": {"task_id": desc2["task_id"], "rojo_pid": rojo2_pid},
            "forced_kill": {"task_id": desc3["task_id"], "child_pids": forced_child_pids},
        }
    except Exception:
        print(f"phase9 run dir retained for diagnostics: {run_root}", file=sys.stderr)
        raise
    finally:
        for process in agents:
            cleanup_process(process)
        helper.shutdown()
        helper.server_close()


def main() -> int:
    parser = argparse.ArgumentParser(description="Run Phase 9 task-agent/Rojo lifecycle gate.")
    parser.add_argument("--rojo", type=Path, default=DEFAULT_ROJO)
    parser.add_argument("--project", type=Path, default=ROOT / "plugin-mcp2" / "default.project.json")
    parser.add_argument("--place-id", default="134795435066737")
    args = parser.parse_args()
    try:
        result = run_phase9(args)
    except Exception as exc:  # noqa: BLE001 - command-line gate reports concise failure.
        print(f"PHASE9_FAIL {exc}", file=sys.stderr)
        return 1
    print("PHASE9_OK " + json.dumps(result, ensure_ascii=False, default=str))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
