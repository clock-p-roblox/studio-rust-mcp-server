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
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parent))
from clockp_bridge2 import commands
from clockp_bridge2 import http as bridge_http
from clockp_bridge2.commands import main
from clockp_bridge2.errors import BridgeError
from clockp_bridge2.session import load_session
from clockp_bridge2.studio import ensure_edit_mode


class FakeHelper(BaseHTTPRequestHandler):
    mode = "edit"
    mode_seq = 101
    launch_id: int | None = None
    state = "live"
    available = True
    play_error_payload: dict | None = None
    play_changes_mode = True
    play_launch_behavior = "match"
    stop_changes_mode = True
    mode_query_count = 0
    mode_unavailable_on_query_number: int | None = None
    mode_unavailable_reason: str | None = None
    run_code_ok = True
    official_ok = True
    requests_seen: list[tuple[str, str, dict | None]] = []

    def log_message(self, format: str, *args: object) -> None:
        return

    def do_GET(self) -> None:
        self._record(None)
        if self.path.endswith("/status"):
            self._json({"ok": True, "state": type(self).state, "task_id": "task-a"})
        elif self.path.endswith("/studio/mode"):
            type(self).mode_query_count += 1
            if (
                type(self).mode_unavailable_on_query_number is not None
                and type(self).mode_query_count == type(self).mode_unavailable_on_query_number
            ):
                reason = type(self).mode_unavailable_reason or "mode_seq_changed"
                self._json({"ok": True, "available": False, "mode": "unknown", "reason": reason, "task_id": "task-a"})
                return
            payload = {
                "ok": True,
                "available": type(self).available,
                "mode": type(self).mode,
                "mode_seq": type(self).mode_seq,
                "task_id": "task-a",
            }
            if type(self).launch_id is not None:
                payload["launch_id"] = type(self).launch_id
            self._json(payload)
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
            if type(self).play_error_payload is not None:
                self._json(type(self).play_error_payload, status=409)
                return
            requested_launch_id = int(payload["play_args"]["launch_id"])
            if type(self).play_changes_mode:
                type(self).mode = "play_server"
                type(self).mode_seq += 1
                if type(self).play_launch_behavior == "match":
                    type(self).launch_id = requested_launch_id
                elif type(self).play_launch_behavior == "mismatch":
                    type(self).launch_id = requested_launch_id + 1
                elif type(self).play_launch_behavior == "missing":
                    type(self).launch_id = None
            self._json(
                {
                    "ok": True,
                    "accepted": True,
                    "requested_launch_id": requested_launch_id,
                    "command_result": {"result": {"status": "play_requested"}},
                }
            )
        elif self.path.endswith("/studio/stop"):
            if type(self).mode == "edit":
                self._json({"ok": True, "accepted": True, "command_result": {"result": {"status": "already_stopped"}}})
                return
            if type(self).stop_changes_mode:
                type(self).mode = "edit"
                type(self).mode_seq += 1
                type(self).launch_id = None
            self._json({"ok": True, "accepted": True, "command_result": {"result": {"status": "stop_requested"}}})
        elif self.path.endswith("/studio/run-code-direct"):
            if type(self).run_code_ok:
                self._json({"ok": True, "result": {"prints": ["hello"], "code": payload.get("code")}})
            else:
                self._json({"ok": False, "command_result": {"error": "blocked token"}})
        elif "/official/" in self.path:
            if type(self).official_ok:
                self._json({"ok": True, "official": {"path": self.path, "payload": payload}})
            else:
                self._json({"ok": False, "code": "official_tool_error", "message": "official failed"})
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


class FakeURLResponse:
    def __init__(self, body: dict, status: int = 200) -> None:
        self.status = status
        self._body = json.dumps(body).encode("utf-8")

    def read(self) -> bytes:
        return self._body

    def __enter__(self) -> "FakeURLResponse":
        return self

    def __exit__(self, exc_type, exc, tb) -> None:
        return None


class Bridge2CLITest(unittest.TestCase):
    def setUp(self) -> None:
        FakeHelper.mode = "edit"
        FakeHelper.mode_seq = 101
        FakeHelper.launch_id = None
        FakeHelper.state = "live"
        FakeHelper.available = True
        FakeHelper.play_error_payload = None
        FakeHelper.play_changes_mode = True
        FakeHelper.play_launch_behavior = "match"
        FakeHelper.stop_changes_mode = True
        FakeHelper.mode_query_count = 0
        FakeHelper.mode_unavailable_on_query_number = None
        FakeHelper.mode_unavailable_reason = None
        FakeHelper.run_code_ok = True
        FakeHelper.official_ok = True
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

        code_path.write_text("\ufeffprint('from bom file')", encoding="utf-8")
        code, payload, _stderr = self.run_cli("run-code-direct", "--file", str(code_path))
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["result"]["code"], "print('from bom file')")

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
            ensure_edit_mode(load_session(self.workspace), stop_timeout_seconds=0.05, poll_seconds=0.01)
        self.assertEqual(raised.exception.code, "stop_transition_timeout")

    def test_play_with_data_json_verifies_launch_id(self) -> None:
        code, payload, _stderr = self.run_cli("play", "--data-json", '{"kind":"smoke"}')
        self.assertEqual(code, 0)
        details = payload["details"]
        self.assertEqual(details["accepted"], True)
        self.assertEqual(details["final_mode"]["mode"], "play_server")
        self.assertEqual(details["final_mode"]["launch_id"], details["requested_launch_id"])
        play_requests = [request for request in FakeHelper.requests_seen if request[1].endswith("/studio/play")]
        self.assertEqual(len(play_requests), 1)
        sent_payload = play_requests[0][2]
        self.assertEqual(sent_payload["play_args"]["data"], {"kind": "smoke"})

    def test_play_with_data_file_verifies_launch_id(self) -> None:
        data_path = Path(self.workspace) / "play-data.json"
        data_path.write_text('{"map":"arena"}', encoding="utf-8")
        code, payload, _stderr = self.run_cli("play", "--data-file", str(data_path))
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["final_mode"]["launch_id"], payload["details"]["requested_launch_id"])

    def test_play_missing_launch_id_fails(self) -> None:
        FakeHelper.play_launch_behavior = "missing"
        code, payload, _stderr = self.run_cli("play")
        self.assertNotEqual(code, 0)
        self.assertEqual(payload["code"], "launch_id_missing")

    def test_play_helper_superseded_error_is_json_failure(self) -> None:
        FakeHelper.play_error_payload = {
            "ok": False,
            "code": "play_request_superseded",
            "message": "Studio mode changed before this play request produced a response",
        }
        code, payload, _stderr = self.run_cli("play")
        self.assertNotEqual(code, 0)
        self.assertEqual(payload["code"], "play_request_superseded")
        self.assertEqual(payload["message"], "Studio mode changed before this play request produced a response")

    def test_play_tolerates_transient_mode_seq_changed_unavailable(self) -> None:
        FakeHelper.mode_unavailable_on_query_number = 2
        FakeHelper.mode_unavailable_reason = "mode_seq_changed"
        code, payload, _stderr = self.run_cli("play")
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["final_mode"]["launch_id"], payload["details"]["requested_launch_id"])

    def test_stop_verifies_mode_seq_change(self) -> None:
        FakeHelper.mode = "play_server"
        FakeHelper.mode_seq = 120
        FakeHelper.launch_id = 77
        code, payload, _stderr = self.run_cli("stop")
        self.assertEqual(code, 0)
        details = payload["details"]
        self.assertEqual(details["final_mode"]["mode"], "edit")
        self.assertGreater(details["final_mode"]["mode_seq"], details["before_mode"]["mode_seq"])

    def test_stop_already_stopped_is_success(self) -> None:
        code, payload, _stderr = self.run_cli("stop")
        self.assertEqual(code, 0)
        self.assertEqual(payload["details"]["reason"], "already_stopped")

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
        for command in ("status", "mode", "play", "stop", "screenshot", "play-mode-logs", "official-ping"):
            with self.subTest(command=command):
                FakeHelper.requests_seen = []
                FakeHelper.mode = "edit"
                code, payload, stderr = self.run_cli(command)
                self.assertEqual(code, 0)
                self.assertEqual(stderr, "")
                self.assertEqual(payload["ok"], True)
                paths = [path for _method, path, _payload in FakeHelper.requests_seen]
                if command not in ("mode", "play", "stop"):
                    self.assertNotIn("/session/task-a/studio/mode", paths)

    def test_official_generate_mesh_ensures_edit_by_default(self) -> None:
        FakeHelper.mode = "play_server"
        code, payload, stderr = self.run_cli("official-generate-mesh", "--text-prompt", "small tree", "--size-x", "1", "--size-y", "2", "--size-z", "3", "--max-triangles", "120")
        self.assertEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["details"]["ensure_edit"]["reason"], "stopped_play")
        official = payload["details"]["official"]["official"]
        self.assertEqual(official["path"], "/session/task-a/official/generate-mesh")
        self.assertEqual(official["payload"]["text_prompt"], "small tree")
        self.assertEqual(official["payload"]["size"], {"x": 1.0, "y": 2.0, "z": 3.0})
        self.assertEqual(official["payload"]["max_triangles"], 120)

    def test_official_direct_option_skips_ensure(self) -> None:
        FakeHelper.mode = "play_server"
        code, payload, stderr = self.run_cli("official-generate-procedural-model", "--prompt", "crate", "--no-ensure-edit")
        self.assertEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertNotIn("ensure_edit", payload["details"])
        paths = [path for _method, path, _payload in FakeHelper.requests_seen]
        self.assertNotIn("/session/task-a/studio/stop", paths)
        self.assertIn("/session/task-a/official/generate-procedural-model", paths)

    def test_official_search_and_insert_payloads(self) -> None:
        code, search_payload, _stderr = self.run_cli("official-search-creator-store", "--query", "tree", "--asset-type", "Model", "--max-results", "3", "--price-filter", "free", "--verified-creators-only")
        self.assertEqual(code, 0)
        search = search_payload["details"]["official"]["payload"]
        self.assertEqual(search["query"], "tree")
        self.assertEqual(search["asset_type"], "Model")
        self.assertEqual(search["max_results"], 3)
        self.assertEqual(search["price_filter"], "free")
        self.assertEqual(search["verified_creators_only"], True)

        FakeHelper.requests_seen = []
        code, insert_payload, _stderr = self.run_cli("official-insert-from-creator-store", "--asset-id", "123", "--asset-name", "Tree", "--asset-type", "Model", "--parent-path", "game.Workspace")
        self.assertEqual(code, 0)
        insert = insert_payload["details"]["official"]["official"]["payload"]
        self.assertEqual(insert["asset_id"], "123")
        self.assertEqual(insert["asset_name"], "Tree")
        paths = [path for _method, path, _payload in FakeHelper.requests_seen]
        self.assertIn("/session/task-a/studio/mode", paths)
        self.assertIn("/session/task-a/official/insert-from-creator-store", paths)

    def test_official_mesh_rejects_partial_size(self) -> None:
        code, payload, stderr = self.run_cli("official-generate-mesh", "--text-prompt", "tree", "--size-x", "1")
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["code"], "argument_error")

    def test_official_helper_failure_is_json_failure(self) -> None:
        FakeHelper.official_ok = False
        code, payload, stderr = self.run_cli("official-ping")
        self.assertNotEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], False)
        self.assertEqual(payload["code"], "official_tool_error")
        self.assertEqual(payload["message"], "official failed")

    def test_public_helper_request_injects_bearer_token(self) -> None:
        token_dir = Path(self.workspace) / ".dev.clock-p.com"
        token_dir.mkdir()
        (token_dir / "feishu-token").write_text("secret-token\n", encoding="utf-8")
        session_path = Path(self.workspace) / ".clock-p" / "session.json"
        session_path.write_text(
            json.dumps(
                {
                    "task_id": "task-a",
                    "helper": {"base_url": "https://roblox-helper-sunjun2-sunjun-user.dev.clock-p.com"},
                }
            ),
            encoding="utf-8",
        )

        seen_headers: dict[str, str] = {}

        def fake_urlopen(req, timeout=10.0):
            nonlocal seen_headers
            seen_headers = dict(req.header_items())
            return FakeURLResponse({"ok": True, "state": "live", "task_id": "task-a"})

        with mock.patch.object(bridge_http.request, "urlopen", side_effect=fake_urlopen):
            code, payload, stderr = self.run_cli("status")

        self.assertEqual(code, 0)
        self.assertEqual(stderr, "")
        self.assertEqual(payload["ok"], True)
        self.assertEqual(seen_headers.get("Authorization"), "Bearer secret-token")


if __name__ == "__main__":
    unittest.main()
