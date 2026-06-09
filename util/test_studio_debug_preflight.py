import unittest
from pathlib import Path
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parent))
import studio_debug_preflight as preflight


class StudioDebugPreflightTests(unittest.TestCase):
    def test_collects_multiple_entity_errors_before_studio_testing(self):
        errors: list[str] = []

        preflight.check_hub_task(
            {
                "task_id": "task-a",
                "place_id": "134795435066737",
                "service_state": "expired",
                "accepting_launches": False,
                "released": False,
                "claimed_by_helper_id": None,
            },
            task_id="task-a",
            helper_id="h-test",
            place_id="134795435066737",
            errors=errors,
        )
        preflight.check_hub_helper(
            {
                "helper_id": "h-test",
                "blocked": False,
                "active_tasks": [],
            },
            task_id="task-a",
            helper_id="h-test",
            require_active_task=True,
            errors=errors,
        )
        preflight.check_helper_task(
            {
                "ok": True,
                "task_id": "task-a",
                "claimed_task": {
                    "task_id": "task-a",
                    "place_id": "134795435066737",
                },
                "instances": [],
            },
            task_id="task-a",
            place_id="134795435066737",
            require_plugin=True,
            errors=errors,
        )
        preflight.check_server_state(
            {
                "ok": True,
                "task_id": "task-a",
                "active_helper": None,
            },
            task_id="task-a",
            require_helper=True,
            errors=errors,
        )

        joined = "\n".join(errors)
        self.assertIn("hub service_state mismatch", joined)
        self.assertIn("hub accepting_launches mismatch", joined)
        self.assertIn("hub helper active_tasks does not include task task-a", joined)
        self.assertIn("Studio plugin is not registered", joined)
        self.assertIn("server active_helper is missing", joined)

    def test_readonly_snapshots_are_collected_in_parallel(self):
        def reader(label: str):
            def run():
                time.sleep(0.2)
                return {"label": label}

            return run

        started = time.perf_counter()
        results, errors, elapsed_ms = preflight.collect_readonly_snapshots(
            {
                "hub status": reader("hub"),
                "task-server status": reader("task-server"),
                "helper status": reader("helper"),
                "helper task": reader("helper-task"),
            },
            timeout=1.0,
        )
        elapsed = time.perf_counter() - started

        self.assertEqual(errors, [])
        self.assertEqual(set(results), {"hub status", "task-server status", "helper status", "helper task"})
        self.assertLess(elapsed, 0.5)
        self.assertLess(elapsed_ms, 500)

    def test_hub_status_rejects_unexpected_same_place_claim(self):
        errors: list[str] = []

        preflight.check_hub_status(
            {
                "ok": True,
                "tasks": [{"task_id": "task-a"}],
                "helpers": [
                    {
                        "helper_id": "h-test",
                        "active_tasks": [{"task_id": "task-a", "place_id": "134795435066737"}],
                    },
                    {
                        "helper_id": "h-other",
                        "active_tasks": [{"task_id": "task-b", "place_id": "134795435066737"}],
                    },
                ],
            },
            task_id="task-a",
            helper_id="h-test",
            place_id="134795435066737",
            errors=errors,
        )

        self.assertIn("unexpected claimed tasks for place 134795435066737", "\n".join(errors))


if __name__ == "__main__":
    unittest.main()
