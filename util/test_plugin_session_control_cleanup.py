#!/usr/bin/env python3
from __future__ import annotations

import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]


class PluginSessionControlCleanupTests(unittest.TestCase):
    def test_run_script_in_play_mode_tool_is_removed(self) -> None:
        dispatcher = (REPO / "plugin/src/Utils/ToolDispatcher.luau").read_text(encoding="utf-8")
        types = (REPO / "plugin/src/Types.luau").read_text(encoding="utf-8")
        server = (REPO / "src/rbx_studio_server.rs").read_text(encoding="utf-8")

        self.assertFalse((REPO / "plugin/src/Tools/RunScriptInPlayMode.luau").exists())
        self.assertNotIn("RunScriptInPlayMode", dispatcher)
        self.assertNotIn("RunScriptInPlayMode", types)
        self.assertNotIn("run_script_in_play_mode", server)

    def test_start_play_does_not_hide_stop_retry_for_previous_test(self) -> None:
        session_control = (REPO / "plugin/src/Utils/StudioSessionControl.luau").read_text(encoding="utf-8")

        self.assertNotIn("single_retry_stop_before_start", session_control)
        self.assertNotIn("one stop/retry", session_control)
        self.assertNotIn("Call start_stop_play(stop) once", session_control)
        self.assertIn("previous_test_in_progress", session_control)
        self.assertIn("start_play or run_server was not retried", session_control)
        self.assertIn("Restart Studio before launching again", session_control)

    def test_run_server_uses_previous_test_in_progress_error_code(self) -> None:
        session_control = (REPO / "plugin/src/Utils/StudioSessionControl.luau").read_text(encoding="utf-8")

        run_server_start = session_control.index("local function startRunServerMode")
        run_server_end = session_control.index("local function handleStartStopPlay", run_server_start)
        run_server_function = session_control[run_server_start:run_server_end]

        self.assertIn("isPreviousTestStillInProgress(errorMessage)", run_server_function)
        self.assertIn("previousTestInProgressError(errorMessage)", run_server_function)

    def test_start_play_waits_for_helper_control_ready(self) -> None:
        session_control = (REPO / "plugin/src/Utils/StudioSessionControl.luau").read_text(encoding="utf-8")

        start_play_start = session_control.index("local function startPlayMode")
        start_play_end = session_control.index("local function startRunServerMode", start_play_start)
        start_play_function = session_control[start_play_start:start_play_end]

        self.assertIn("waitForStartPlayLog", start_play_function)
        self.assertIn("waitForHelperControlReady", start_play_function)
        self.assertLess(
            start_play_function.index("waitForStartPlayLog"),
            start_play_function.index("waitForHelperControlReady"),
            "start_play must first observe Studio entering play, then wait for helper-owned control heartbeat",
        )
        self.assertIn('lastModeSource == "play_control"', session_control)
        self.assertIn('lastControlState == "ready"', session_control)
        self.assertIn('lastTransitionPhase == "running"', session_control)


if __name__ == "__main__":
    unittest.main()
