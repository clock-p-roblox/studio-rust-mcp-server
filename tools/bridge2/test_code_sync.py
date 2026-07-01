from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

from clockp_bridge2.code_sync.mapping import build_logical_tree
from clockp_bridge2.code_sync.project import load_project_targets, validate_studio_path_allowed
from clockp_bridge2.code_sync.config import CodeSyncRoot
from clockp_bridge2.code_sync.diff import diff_manifests
from clockp_bridge2.code_sync.hashing import LogicalNode
from clockp_bridge2.code_sync.scanner import SourceFile, scan_root
from clockp_bridge2.errors import BridgeError


class CodeSyncTests(unittest.TestCase):
    def test_game1_style_project_targets_allow_workspace_and_declared_container(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            project = Path(tmp) / "default.project.json"
            project.write_text(
                json.dumps(
                    {
                        "tree": {
                            "$className": "DataModel",
                            "ReplicatedStorage": {
                                "$className": "ReplicatedStorage",
                                "ClockPRealTest": {"$className": "Folder"},
                            },
                            "Workspace": {"$className": "Workspace"},
                        }
                    }
                ),
                encoding="utf-8",
            )
            targets = load_project_targets(project)
        self.assertIn(["ReplicatedStorage", "ClockPRealTest"], targets)
        self.assertIn(["Workspace"], targets)
        validate_studio_path_allowed(["Workspace", "Probe"], targets)
        validate_studio_path_allowed(["ReplicatedStorage", "ClockPRealTest", "App"], targets)
        with self.assertRaises(BridgeError):
            validate_studio_path_allowed(["ReplicatedStorage", "Shared"], targets)

    def test_init_usurp_script_keeps_children(self) -> None:
        tree = build_logical_tree(
            "Root",
            [
                SourceFile("init.lua", "return {}\n", 10),
                SourceFile("Child.lua", "return 1\n", 9),
            ],
        )
        self.assertEqual(tree.kind, "ModuleScript")
        self.assertEqual(tree.name, "Root")
        self.assertEqual([child.name for child in tree.children], ["Child"])
        self.assertEqual(tree.children[0].kind, "ModuleScript")

    def test_crlf_normalization_is_scanner_responsibility(self) -> None:
        source = "print(1)\r\nprint(2)\r"
        normalized = source.replace("\r\n", "\n").replace("\r", "\n")
        self.assertEqual(normalized, "print(1)\nprint(2)\n")

    def test_double_star_include_matches_root_file(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            src = workspace / "src"
            src.mkdir()
            (src / "Main.lua").write_text("return 1\n", encoding="utf-8")
            root = CodeSyncRoot("app", "src", ["ReplicatedStorage", "ClockPRealTest"], ["**/*.lua"], [])
            files = scan_root(workspace, root)
        self.assertEqual([file.relative_path for file in files], ["Main.lua"])

    def test_glob_star_does_not_cross_directories(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            src = workspace / "src"
            nested = src / "Dir"
            nested.mkdir(parents=True)
            (src / "Main.lua").write_text("return 1\n", encoding="utf-8")
            (nested / "Nested.lua").write_text("return 2\n", encoding="utf-8")
            root = CodeSyncRoot("app", "src", ["ReplicatedStorage", "ClockPRealTest"], ["*.lua"], [])
            files = scan_root(workspace, root)
        self.assertEqual([file.relative_path for file in files], ["Main.lua"])

    def test_local_path_rejects_windows_absolute_path(self) -> None:
        with self.assertRaises(BridgeError):
            from clockp_bridge2.code_sync.config import load_config

            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "code-sync.roots.json"
                path.write_text(
                    json.dumps(
                        {
                            "project_id": "x",
                            "mapping_profile": "sync_lua_v1",
                            "roots": [{"root_id": "x", "local_path": "K:/outside", "studio_path": ["Workspace"], "include": [], "exclude": []}],
                        }
                    ),
                    encoding="utf-8",
                )
                load_config(path, [["Workspace"]])

    def test_root_overlap_rejected(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.roots.json"
            path.write_text(
                json.dumps(
                    {
                        "project_id": "x",
                        "mapping_profile": "sync_lua_v1",
                        "roots": [
                            {"root_id": "a", "local_path": "a", "studio_path": ["Workspace", "A"], "include": [], "exclude": []},
                            {"root_id": "b", "local_path": "b", "studio_path": ["Workspace", "A", "B"], "include": [], "exclude": []},
                        ],
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(BridgeError):
                load_config(path, [["Workspace"]])

    def test_invalid_utf8_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            src = workspace / "src"
            src.mkdir()
            (src / "Bad.lua").write_bytes(b"\xff")
            root = CodeSyncRoot("app", "src", ["Workspace"], ["**/*.lua"], [])
            with self.assertRaises(BridgeError) as ctx:
                scan_root(workspace, root)
        self.assertEqual(ctx.exception.code, "code_sync_unsupported_encoding")

    def test_source_too_large_rejected_before_flush(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            src = workspace / "src"
            src.mkdir()
            (src / "Huge.lua").write_text("a" * 200000, encoding="utf-8")
            root = CodeSyncRoot("app", "src", ["Workspace"], ["**/*.lua"], [])
            with self.assertRaises(BridgeError) as ctx:
                scan_root(workspace, root)
        self.assertEqual(ctx.exception.code, "code_sync_source_too_large")
        self.assertEqual(ctx.exception.details["relative_path"], "Huge.lua")
        self.assertEqual(ctx.exception.details["source_chars"], 200000)

    def test_hash_fixture(self) -> None:
        script = LogicalNode("Main", "ModuleScript", "return 1\n")
        self.assertEqual(script.entry_hash(), "6520c8981971c534640f33cd4664b94b77ae9d2b9e6c3d3e7da744f13639a209")
        folder = LogicalNode("Root", "Folder", children=[script])
        self.assertEqual(folder.entry_hash(), "0fa8f8896db5050d170a1682aff52745b71d44c1dd0b2779e7842e3062873ad4")

    def test_diff_manifest_match_and_mismatch(self) -> None:
        local = {"combined_hash": "a", "roots": [{"root_id": "r", "root_hash": "x"}]}
        live = {"combined_hash": "a", "roots": [{"root_id": "r", "exists": True, "root_hash": "x"}]}
        self.assertTrue(diff_manifests(local, live)["matched"])
        live["roots"][0]["root_hash"] = "y"
        live["combined_hash"] = "b"
        diff = diff_manifests(local, live)
        self.assertFalse(diff["matched"])
        self.assertEqual(diff["mismatches"][0]["kind"], "root_hash_mismatch")

    def test_1000_file_logical_tree(self) -> None:
        files = [SourceFile(f"Dir/File{i}.lua", f"return {i}\n", len(f"return {i}\n")) for i in range(1000)]
        tree = build_logical_tree("Root", files)
        self.assertEqual(tree.kind, "Folder")
        self.assertEqual(len(tree.children), 1)
        self.assertEqual(tree.children[0].name, "Dir")
        self.assertEqual(len(tree.children[0].children), 1000)


if __name__ == "__main__":
    unittest.main()
