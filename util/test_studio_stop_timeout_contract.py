#!/usr/bin/env python3
from __future__ import annotations

import re
import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]
PLUGIN_SESSION_CONTROL = REPO / "plugin/src/Utils/StudioSessionControl.luau"
SERVER = REPO / "src/rbx_studio_server.rs"
HELPER = REPO / "src/bin/studio_helper.rs"


def read(path: Path) -> str:
    return path.read_text(encoding="utf-8")


def luau_seconds_constant(source: str, name: str) -> int:
    match = re.search(rf"local\s+{re.escape(name)}\s*=\s*(\d+)", source)
    if match is None:
        raise AssertionError(f"missing Luau constant {name}")
    return int(match.group(1))


def rust_secs_constant(source: str, name: str) -> int:
    match = re.search(
        rf"const\s+{re.escape(name)}\s*:\s*Duration\s*=\s*Duration::from_secs\((\d+)\)",
        source,
    )
    if match is None:
        raise AssertionError(f"missing Rust Duration constant {name}")
    return int(match.group(1))


class StudioStopTimeoutContractTests(unittest.TestCase):
    def test_stop_timeout_order_allows_plugin_to_report_before_transport_timeout(self) -> None:
        plugin = read(PLUGIN_SESSION_CONTROL)
        helper = read(HELPER)
        server = read(SERVER)

        plugin_stop_timeout = luau_seconds_constant(plugin, "STOP_TIMEOUT_SECONDS")
        helper_plugin_timeout = rust_secs_constant(helper, "PLUGIN_STUDIO_CONTROL_REQUEST_TIMEOUT")
        server_helper_timeout = rust_secs_constant(server, "HELPER_STUDIO_CONTROL_REQUEST_TIMEOUT")

        self.assertEqual(plugin_stop_timeout, 60)
        self.assertGreater(helper_plugin_timeout, plugin_stop_timeout)
        self.assertGreater(server_helper_timeout, helper_plugin_timeout)

    def test_edit_runtime_stop_does_not_directly_stop_play_runtime(self) -> None:
        plugin = read(PLUGIN_SESSION_CONTROL)
        stop_function_start = plugin.index("local function stop(")
        stop_function_end = plugin.index("local function waitForStartPlayLog", stop_function_start)
        stop_function = plugin[stop_function_start:stop_function_end]

        self.assertNotIn("waitForStopLog(baseline, STOP_TIMEOUT_SECONDS)", stop_function)
        self.assertNotIn('/v1/mcp/plugin/stop-request', stop_function)
        self.assertNotIn("StudioTestService:EndTest", stop_function)
        self.assertNotIn("ExecutePlayModeAsync", stop_function)
        self.assertNotIn("ExecuteRunModeAsync", stop_function)

    def test_play_control_script_polls_stop_intent_and_stops_in_runtime(self) -> None:
        plugin = read(PLUGIN_SESSION_CONTROL)
        install_start = plugin.index("local function installSessionControlScript")
        install_end = plugin.index("local function requestStudioLog", install_start)
        install_function = plugin[install_start:install_end]

        self.assertIn("/v1/mcp/plugin/control-heartbeat", install_function)
        self.assertIn("/v1/mcp/plugin/stop-request", install_function)
        self.assertIn("/v1/mcp/plugin/stop-request/ack", install_function)
        self.assertIn("StudioTestService:EndTest", install_function)
        self.assertIn('stopped_by = "mcp_session_control"', install_function)
        self.assertLess(
            install_function.index("lastStopRequestId = stopRequestId"),
            install_function.index("StudioTestService:EndTest"),
            "a stop request must be consumed before EndTest so failures do not retry the same request forever",
        )
        self.assertLess(
            install_function.index('pcall(reportStopRequestAck, stopRequestId, "end_test_requested", nil)'),
            install_function.index("StudioTestService:EndTest"),
        )
        self.assertIn('"end_test_failed"', install_function)
        self.assertNotIn("/v1/mcp/plugin/stop-ack", install_function)
        self.assertNotIn("ExecutePlayModeAsync", install_function)
        self.assertNotIn("ExecuteRunModeAsync", install_function)

    def test_legacy_play_runtime_stop_paths_are_removed(self) -> None:
        helper = read(HELPER)
        self.assertFalse((REPO / "plugin/src/Utils/GameStopUtil.luau").exists())
        self.assertNotIn("/v1/mcp/plugin/stop-ack", helper)
        self.assertNotIn("mcp_plugin_stop_ack_handler", helper)
        self.assertNotIn("stopping_acknowledged", helper)


if __name__ == "__main__":
    unittest.main()
