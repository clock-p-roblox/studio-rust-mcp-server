from __future__ import annotations

import json
import tempfile
import unittest
from pathlib import Path
import sys

sys.path.insert(0, str(Path(__file__).resolve().parent))

from clockp_bridge2.code_sync.mapping import build_logical_tree
from clockp_bridge2.code_sync.config import CodeSyncNode
from clockp_bridge2.code_sync.diff import diff_manifests
from clockp_bridge2.code_sync.hashing import LogicalNode, target_authority_hash
from clockp_bridge2.code_sync.scanner import SourceFile, scan_root
from clockp_bridge2.errors import BridgeError


class CodeSyncTests(unittest.TestCase):
    def test_load_config_allows_supported_service_nodes(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(
                json.dumps(
                    {
                        "tree": {
                            "Workspace": {"ClockPTest": {"$local_path": "src", "$include": [], "$exclude": []}},
                            "ReplicatedStorage": {"ClockPRealTest": {"$local_path": "lib", "$include": [], "$exclude": []}},
                        }
                    }
                ),
                encoding="utf-8",
            )
            config = load_config(path)
        self.assertEqual([node.studio_path for node in config.nodes], [["Workspace", "ClockPTest"], ["ReplicatedStorage", "ClockPRealTest"]])

    def test_load_config_allows_managed_service_node(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(json.dumps({"tree": {"ServerScriptService": {"$local_path": ".ts-out/server"}}}), encoding="utf-8")
            config = load_config(path)
        self.assertEqual([node.studio_path for node in config.nodes], [["ServerScriptService"]])

    def test_load_config_rejects_service_node_kind(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(json.dumps({"tree": {"ServerScriptService": {"$local_path": ".ts-out/server", "$kind": "Folder"}}}), encoding="utf-8")
            with self.assertRaises(BridgeError) as ctx:
                load_config(path)
        self.assertEqual(ctx.exception.code, "code_sync_invalid_config")

    def test_load_config_rejects_unsupported_managed_service_node(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(json.dumps({"tree": {"Workspace": {"$local_path": "src"}}}), encoding="utf-8")
            with self.assertRaises(BridgeError) as ctx:
                load_config(path)
        self.assertEqual(ctx.exception.code, "code_sync_invalid_config")

    def test_load_config_rejects_unknown_service_root(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(
                json.dumps(
                    {
                        "tree": {
                            "NotAService": {"ClockPTest": {"$local_path": "src"}},
                        }
                    }
                ),
                encoding="utf-8",
            )
            with self.assertRaises(BridgeError):
                load_config(path)

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
            root = CodeSyncNode("src", ["ReplicatedStorage", "ClockPRealTest"], None, ["**/*.lua"], [])
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
            root = CodeSyncNode("src", ["ReplicatedStorage", "ClockPRealTest"], None, ["*.lua"], [])
            files = scan_root(workspace, root)
        self.assertEqual([file.relative_path for file in files], ["Main.lua"])

    def test_local_path_rejects_windows_absolute_path(self) -> None:
        with self.assertRaises(BridgeError):
            from clockp_bridge2.code_sync.config import load_config

            with tempfile.TemporaryDirectory() as tmp:
                path = Path(tmp) / "code-sync.tree.json"
                path.write_text(json.dumps({"tree": {"Workspace": {"A": {"$local_path": "K:/outside"}}}}), encoding="utf-8")
                load_config(path)

    def test_nested_nodes_allowed(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(
                json.dumps(
                    {
                        "tree": {
                            "Workspace": {
                                "A": {
                                    "$local_path": "a",
                                    "B": {"$local_path": "b"},
                                }
                            }
                        },
                    }
                ),
                encoding="utf-8",
            )
            config = load_config(path)
        self.assertEqual([node.studio_path for node in config.nodes], [["Workspace", "A"], ["Workspace", "A", "B"]])

    def test_load_config_rejects_unknown_node_metadata(self) -> None:
        from clockp_bridge2.code_sync.config import load_config

        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "code-sync.tree.json"
            path.write_text(
                json.dumps({"tree": {"Workspace": {"A": {"$local_path": "a", "$incldue": ["**/*.lua"]}}}}),
                encoding="utf-8",
            )
            with self.assertRaises(BridgeError) as ctx:
                load_config(path)
        self.assertEqual(ctx.exception.code, "code_sync_invalid_config")
        self.assertIn("$incldue", ctx.exception.details["metadata"])

    def test_invalid_utf8_rejected(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            src = workspace / "src"
            src.mkdir()
            (src / "Bad.lua").write_bytes(b"\xff")
            root = CodeSyncNode("src", ["Workspace"], None, ["**/*.lua"], [])
            with self.assertRaises(BridgeError) as ctx:
                scan_root(workspace, root)
        self.assertEqual(ctx.exception.code, "code_sync_unsupported_encoding")

    def test_source_too_large_rejected_before_flush(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            workspace = Path(tmp)
            src = workspace / "src"
            src.mkdir()
            (src / "Huge.lua").write_text("a" * 200000, encoding="utf-8")
            root = CodeSyncNode("src", ["Workspace"], None, ["**/*.lua"], [])
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
        self.assertEqual(
            target_authority_hash([["Workspace", "B"], ["ReplicatedStorage", "A"]]),
            "1c92a941b27b5f7cecf24f16cb8b3212d47da2cfc50b91b9aa1064c4523c299f",
        )

    def test_diff_manifest_match_and_mismatch(self) -> None:
        local = {"combined_hash": "a", "targets": [{"target_id": "r", "target_hash": "x"}]}
        live = {"combined_hash": "a", "targets": [{"target_id": "r", "exists": True, "target_hash": "x"}]}
        self.assertTrue(diff_manifests(local, live)["matched"])
        live["targets"][0]["target_hash"] = "y"
        live["combined_hash"] = "b"
        diff = diff_manifests(local, live)
        self.assertFalse(diff["matched"])
        self.assertEqual(diff["mismatches"][0]["kind"], "target_hash_mismatch")

    def test_1000_file_logical_tree(self) -> None:
        files = [SourceFile(f"Dir/File{i}.lua", f"return {i}\n", len(f"return {i}\n")) for i in range(1000)]
        tree = build_logical_tree("Root", files)
        self.assertEqual(tree.kind, "Folder")
        self.assertEqual(len(tree.children), 1)
        self.assertEqual(tree.children[0].name, "Dir")
        self.assertEqual(len(tree.children[0].children), 1000)


if __name__ == "__main__":
    unittest.main()
