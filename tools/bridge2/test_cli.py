from __future__ import annotations

import contextlib
import io
import json
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))
from clockp_bridge2 import commands
from clockp_bridge2.commands import main
from clockp_bridge2.errors import BridgeError
from clockp_bridge2.session import load_session
from clockp_bridge2.studio import ensure_edit_mode


class FakeHelper(BaseHTTPRequestHandler):
    mode = "edit"
    state = "live"
    available = True
    stop_changes_mode = True
    run_code_ok = True
    requests_seen: list[tuple[str, str, dict | None]] = []

    def log_message(self, format: str, *args: object) -> None:
        return

    def do_GET(self) -> None:
        self._record(None)
        if self.path.endswith("/status"):
            self._json({"ok": True, "state": type(self).state, "task_id": "task-a"})
        elif self.path.endswith("/studio/mode"):
            self._json({"ok": True, "available": type(self).available, "mode": type(self).mode, "task_id": "task-a"})
        elif self.path.endswith("/studio/screenshot"):
            self._json({"ok": True, "screenshot": {"path": "shot.png"}})
        elif self.path.startswith("/session/task-a/runtime-log"):
            self._json({"ok": True, "entries": [], "next_cursor": ""})
        else:
            self._json({"code": "not_found", "message": self.path}, status=404)

    def do_POST(self) -> None:
        length = int(self.headers.get("Content-Length", "0"))
        body = self.rfile.read(length).decode("utf-8") if length else ""
        payload = json.loads(body) if body else {}
        self._record(payload)
        if self.path.endswith("/studio/play"):
            type(self).mode = "play_server"
            self._json({"ok": True, "result": {"status": "play_requested"}})
        elif self.path.endswith("/studio/stop"):
            if type(self).stop_changes_mode:
                type(self).mode = "edit"
            self._json({"ok": True, "result": {"status": "stop_requested"}})
        elif self.path.endswith("/studio/run-code-direct"):
            if type(self).run_code_ok:
                self._json({"ok": True, "result": {"prints": ["hello"], "code": payload.get("code")}})
            else:
                self._json({"ok": False, "command_result": {"error": "blocked token"}})
        else:
            self._json({"code": "not_found", "message": self.path}, status=404)

    def _record(self, payload: dict | None) -> None:
        type(self).requests_seen.append((self.command, self.path, payload))

    def _json(self, payload: dict, status: int = 200) -> None:
        body = json.dumps(payload).encode("utf-8")
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


class Bridge2CLITest(unittest.TestCase):
    def setUp(self) -> None:
        FakeHelper.mode = "edit"
        FakeHelper.state = "live"
        FakeHelper.available = True
        FakeHelper.stop_changes_mode = True
        FakeHelper.run_code_ok = True
        FakeHelper.requests_seen = []
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), FakeHelper)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.tmp = tempfile.TemporaryDirectory()
        root = Path(self.tmp.name)
        session_dir = root / ".clock-p"
        session_dir.mkdir()
        (session_dir / "session.json").write_text(
            json.dumps(
                {
                    "task_id": "task-a",
                    "helper": {"base_url": f"http://127.0.0.1:{self.server.server_address[1]}"},
                }
            ),
            encoding="utf-8",
        )
        self.workspace = str(root)

    def tearDown(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        self.tmp.cleanup()

    def run_cli(self, *args: str) -> tuple[int, dict, str]:
        stdout = io.StringIO()
        stderr = io.StringIO()
        with contextlib.redirect_stdout(stdout), contextlib.redirect_stderr(stderr):
            code = main(["--workspace", self.workspace, *args])
        return code, json.loads(stdout.getvalue()), stderr.getvalue()

    def test_status_outputs_single_json_on_stdout(self) -> None:
        code, payload, stderr = self.run_cli("status")
        self.assertEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], True)
        self.assertEqual(payload["command"], "status")
        self.assertEqual(payload["details"]["state"], "live")

    def test_argument_error_is_json(self) -> None:
        code, payload, stderr = self.run_cli("run-code")
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], False)
        self.assertEqual(payload["command"], "run-code")
        self.assertEqual(payload["code"], "argument_error")

    def test_help_is_json(self) -> None:
        code, payload, stderr = self.run_cli("--help")
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], False)
        self.assertEqual(payload["code"], "help_requested")

    def test_unhandled_exception_is_json(self) -> None:
        original = commands.load_session
        commands.load_session = lambda _workspace=None: (_ for _ in ()).throw(RuntimeError("boom"))
        try:
            code, payload, stderr = self.run_cli("status")
        finally:
            commands.load_session = original
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], False)
        self.assertEqual(payload["code"], "unhandled_exception")

    def test_file_input_and_mutual_exclusion(self) -> None:
        code_path = Path(self.workspace) / "code.lua"
        code_path.write_text("print('from file')", encoding="utf-8")
        code, payload, _stderr = self.run_cli("run-code-direct", "--file", str(code_path))
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["result"]["code"], "print('from file')")

        code, payload, stderr = self.run_cli("run-code-direct", "--code", "print(1)", "--file", str(code_path))
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["code"], "argument_error")

    def test_missing_session_fields_are_json(self) -> None:
        session_path = Path(self.workspace) / ".clock-p" / "session.json"
        session_path.write_text(json.dumps({"machine_name": "should-not-be-used"}), encoding="utf-8")
        code, payload, stderr = self.run_cli("status")
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["code"], "session_missing_task_id")

    def test_ensure_edit_already_edit(self) -> None:
        code, payload, _stderr = self.run_cli("ensure-edit")
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["reason"], "already_edit")

    def test_ensure_edit_rejects_unknown_mode_and_stale_task(self) -> None:
        FakeHelper.mode = "unknown"
        code, payload, _stderr = self.run_cli("ensure-edit")
        self.assertNotEqual(code, 0)
        self.assertEqual(payload["code"], "studio_mode_not_editable")

        FakeHelper.mode = "edit"
        FakeHelper.state = "stale"
        code, payload, _stderr = self.run_cli("ensure-edit")
        self.assertNotEqual(code, 0)
        self.assertEqual(payload["code"], "task_not_live")

    def test_ensure_edit_timeout(self) -> None:
        FakeHelper.mode = "play_server"
        FakeHelper.stop_changes_mode = False
        with self.assertRaises(BridgeError) as raised:
            ensure_edit_mode(load_session(self.workspace), timeout_seconds=0.05, poll_seconds=0.01)
        self.assertEqual(raised.exception.code, "ensure_edit_timeout")

    def test_run_code_stops_play_before_direct_call(self) -> None:
        FakeHelper.mode = "play_server"
        code, payload, _stderr = self.run_cli("run-code", "--code", "print('hello')")
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["ensure_edit"]["reason"], "stopped_play")
        self.assertEqual(payload["details"]["run_code"]["result"]["code"], "print('hello')")
        paths = [path for _method, path, _payload in FakeHelper.requests_seen]
        self.assertIn("/session/task-a/studio/stop", paths)
        self.assertIn("/session/task-a/studio/run-code-direct", paths)

    def test_run_code_direct_does_not_stop_play(self) -> None:
        FakeHelper.mode = "play_server"
        code, payload, _stderr = self.run_cli("run-code-direct", "--code", "print('hello')")
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["result"]["code"], "print('hello')")
        paths = [path for _method, path, _payload in FakeHelper.requests_seen]
        self.assertNotIn("/session/task-a/studio/stop", paths)

    def test_helper_ok_false_becomes_cli_failure(self) -> None:
        FakeHelper.run_code_ok = False
        code, payload, stderr = self.run_cli("run-code-direct", "--code", "return StudioTestService")
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], False)
        self.assertEqual(payload["code"], "helper_command_failed")
        self.assertEqual(payload["message"], "blocked token")

    def test_thin_commands_do_not_ensure_edit(self) -> None:
        for command in ("status", "mode", "play", "stop", "screenshot", "play-mode-logs"):
            with self.subTest(command=command):
                FakeHelper.requests_seen = []
                FakeHelper.mode = "edit"
                code, payload, stderr = self.run_cli(command)
                self.assertEqual(code, 0)
                self.assertEqual(stderr, "")
                self.assertEqual(payload["ok"], True)
                paths = [path for _method, path, _payload in FakeHelper.requests_seen]
                if command != "mode":
                    self.assertNotIn("/session/task-a/studio/mode", paths)


if __name__ == "__main__":
    unittest.main()
