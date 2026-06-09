#!/usr/bin/env python3
from __future__ import annotations

import unittest
from pathlib import Path


REPO = Path(__file__).resolve().parents[1]


class PluginSessionControlCleanupTests(unittest.TestCase):
    def test_run_script_does_not_install_persistent_session_control_script(self) -> None:
        run_script = (REPO / "plugin/src/Tools/RunScriptInPlayMode.luau").read_text(encoding="utf-8")

        self.assertNotIn(
            "StudioSessionControl.installSessionControlScript(args.mode)",
            run_script,
            "RunScriptInPlayMode is one-shot StudioTestService execution; it must not install persistent session control",
        )
        self.assertIn("removeTestScript()", run_script)

    def test_start_play_does_not_hide_stop_retry_for_previous_test(self) -> None:
        session_control = (REPO / "plugin/src/Utils/StudioSessionControl.luau").read_text(encoding="utf-8")

        self.assertNotIn("single_retry_stop_before_start", session_control)
        self.assertNotIn("one stop/retry", session_control)
        self.assertIn("start_play was not retried", session_control)


if __name__ == "__main__":
    unittest.main()
