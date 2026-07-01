from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ..errors import BridgeError

MAPPING_PROFILE = "sync_lua_v1"
ALLOWED_NODE_META_KEYS = {"$local_path", "$kind", "$include", "$exclude"}
ALLOWED_STUDIO_SERVICES = {
    "Lighting",
    "ReplicatedFirst",
    "ReplicatedStorage",
    "ServerScriptService",
    "ServerStorage",
    "SoundService",
    "StarterGui",
    "StarterPack",
    "StarterPlayer",
    "Workspace",
}
ALLOWED_MANAGED_SERVICE_NODES = {"ReplicatedStorage", "ServerScriptService"}


@dataclass(frozen=True)
class CodeSyncNode:
    local_path: str
    studio_path: list[str]
    kind: str | None
    include: list[str]
    exclude: list[str]

    def as_config_dict(self) -> dict:
        return {
            "local_path": self.local_path,
            "studio_path": self.studio_path,
            "kind": self.kind,
            "include": self.include,
            "exclude": self.exclude,
        }


@dataclass(frozen=True)
class CodeSyncConfig:
    nodes: list[CodeSyncNode]


def load_config(config_path: Path) -> CodeSyncConfig:
    try:
        payload = _loads_no_duplicate_keys(config_path.read_text(encoding="utf-8-sig"))
    except OSError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(config_path)}) from exc
    except json.JSONDecodeError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(config_path)}) from exc
    except ValueError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(config_path)}) from exc
    if not isinstance(payload, dict):
        raise BridgeError("code_sync_invalid_config", "code-sync config must be a JSON object", {"path": str(config_path)})
    if "roots" in payload:
        raise BridgeError("code_sync_invalid_config", "old roots format is not supported; use tree", {"path": str(config_path)})
    tree = payload.get("tree")
    if not isinstance(tree, dict) or not tree:
        raise BridgeError("code_sync_invalid_config", "tree must be a non-empty object", {"path": str(config_path)})
    nodes: list[CodeSyncNode] = []
    for service_name, service_tree in tree.items():
        if service_name.startswith("$"):
            raise BridgeError("code_sync_invalid_config", "top-level keys must be Studio services", {"key": service_name})
        if service_name not in ALLOWED_STUDIO_SERVICES:
            raise BridgeError("code_sync_invalid_config", "tree key must be a supported DataModel service", {"service": service_name})
        if not isinstance(service_tree, dict):
            raise BridgeError("code_sync_invalid_config", "Studio service entry must be an object", {"service": service_name})
        _parse_tree_node(service_tree, [service_name], nodes, is_service=True)
    _validate_unique_studio_paths(nodes)
    if not nodes:
        raise BridgeError("code_sync_invalid_config", "tree must contain at least one node with $local_path", {"path": str(config_path)})
    return CodeSyncConfig(nodes=nodes)


def _loads_no_duplicate_keys(raw: str) -> Any:
    def hook(pairs: list[tuple[str, Any]]) -> dict:
        result: dict[str, Any] = {}
        for key, value in pairs:
            if key in result:
                raise ValueError(f"duplicate JSON key: {key}")
            result[key] = value
        return result

    return json.loads(raw, object_pairs_hook=hook)


def _parse_tree_node(value: dict, studio_path: list[str], nodes: list[CodeSyncNode], *, is_service: bool = False) -> None:
    meta_keys = {key for key in value if key.startswith("$")}
    local_path_value = value.get("$local_path")
    is_node = local_path_value is not None
    if is_service and meta_keys and not is_node:
        raise BridgeError("code_sync_invalid_config", "Studio service entries cannot declare metadata unless they are managed nodes", {"studio_path": studio_path, "metadata": sorted(meta_keys)})
    if not is_node and meta_keys:
        raise BridgeError("code_sync_invalid_config", "only nodes with $local_path can declare metadata", {"studio_path": studio_path, "metadata": sorted(meta_keys)})
    unknown_meta_keys = meta_keys - ALLOWED_NODE_META_KEYS
    if unknown_meta_keys:
        raise BridgeError("code_sync_invalid_config", "unknown code-sync node metadata", {"studio_path": studio_path, "metadata": sorted(unknown_meta_keys)})
    if is_service and is_node and studio_path[0] not in ALLOWED_MANAGED_SERVICE_NODES:
        raise BridgeError("code_sync_invalid_config", "this Studio service cannot be a managed code-sync node", {"studio_path": studio_path, "service": studio_path[0]})
    if is_service and "$kind" in meta_keys:
        raise BridgeError("code_sync_invalid_config", "managed Studio service nodes cannot declare $kind", {"studio_path": studio_path})
    if is_node:
        nodes.append(_parse_node(value, studio_path))
    for child_name, child_value in value.items():
        if child_name.startswith("$"):
            continue
        if not isinstance(child_name, str) or not child_name:
            raise BridgeError("code_sync_invalid_config", "Studio child name must be a non-empty string", {"studio_path": studio_path})
        if child_name.startswith("$"):
            raise BridgeError("code_sync_invalid_config", "Studio child names cannot start with $", {"studio_path": studio_path, "child": child_name})
        if not isinstance(child_value, dict):
            raise BridgeError("code_sync_invalid_config", "Studio child entry must be an object", {"studio_path": studio_path + [child_name]})
        _parse_tree_node(child_value, studio_path + [child_name], nodes)


def _parse_node(value: dict, studio_path: list[str]) -> CodeSyncNode:
    local_path = _required_string(value, "$local_path").replace("\\", "/")
    if (
        local_path.startswith("/")
        or re.match(r"^[A-Za-z]:(/|$)", local_path) is not None
        or local_path.startswith("../")
        or "/../" in f"/{local_path}/"
        or local_path == ".."
    ):
        raise BridgeError("code_sync_invalid_config", "local_path must be workspace-relative and cannot escape workspace", {"studio_path": studio_path, "local_path": local_path})
    kind_value = value.get("$kind")
    kind = None
    if kind_value is not None:
        if kind_value not in {"Folder", "ModuleScript", "Script", "LocalScript"}:
            raise BridgeError("code_sync_invalid_config", "$kind must be Folder, ModuleScript, Script or LocalScript", {"studio_path": studio_path, "kind": kind_value})
        kind = str(kind_value)
    include = _pattern_list(value.get("$include", ["**/*.lua", "**/*.luau"]), "$include", studio_path)
    exclude = _pattern_list(value.get("$exclude", []), "$exclude", studio_path)
    return CodeSyncNode(local_path=local_path, studio_path=studio_path, kind=kind, include=include, exclude=exclude)


def _required_string(payload: dict, key: str) -> str:
    value = payload.get(key)
    if not isinstance(value, str) or not value:
        raise BridgeError("code_sync_invalid_config", f"{key} must be a non-empty string", {"field": key})
    return value


def _string_list(value: Any, field: str, studio_path: list[str]) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise BridgeError("code_sync_invalid_config", f"{field} must be a string array", {"studio_path": studio_path, "field": field})
    return list(value)


def _pattern_list(value: Any, field: str, studio_path: list[str]) -> list[str]:
    return [pattern.replace("\\", "/") for pattern in _string_list(value, field, studio_path)]


def _validate_unique_studio_paths(nodes: list[CodeSyncNode]) -> None:
    seen_paths: set[tuple[str, ...]] = set()
    for node in nodes:
        path = tuple(node.studio_path)
        if path in seen_paths:
            raise BridgeError("code_sync_invalid_config", "duplicate studio_path", {"studio_path": node.studio_path})
        seen_paths.add(path)
