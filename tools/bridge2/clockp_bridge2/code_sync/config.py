from __future__ import annotations

import json
import re
from dataclasses import dataclass
from pathlib import Path
from typing import Any

from ..errors import BridgeError
from .project import validate_studio_path_allowed


@dataclass(frozen=True)
class CodeSyncRoot:
    root_id: str
    local_path: str
    studio_path: list[str]
    include: list[str]
    exclude: list[str]

    def as_config_dict(self) -> dict:
        return {
            "root_id": self.root_id,
            "local_path": self.local_path,
            "studio_path": self.studio_path,
            "include": self.include,
            "exclude": self.exclude,
        }


@dataclass(frozen=True)
class CodeSyncConfig:
    project_id: str
    mapping_profile: str
    roots: list[CodeSyncRoot]


def load_config(config_path: Path, targets: list[list[str]]) -> CodeSyncConfig:
    try:
        payload = json.loads(config_path.read_text(encoding="utf-8-sig"))
    except OSError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(config_path)}) from exc
    except json.JSONDecodeError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(config_path)}) from exc
    if not isinstance(payload, dict):
        raise BridgeError("code_sync_invalid_config", "code-sync config must be a JSON object", {"path": str(config_path)})
    project_id = _required_string(payload, "project_id")
    mapping_profile = _required_string(payload, "mapping_profile")
    if mapping_profile != "sync_lua_v1":
        raise BridgeError("code_sync_unsupported_mapping", "only mapping_profile=sync_lua_v1 is supported", {"mapping_profile": mapping_profile})
    raw_roots = payload.get("roots")
    if not isinstance(raw_roots, list) or not raw_roots:
        raise BridgeError("code_sync_invalid_config", "roots must be a non-empty array", {"path": str(config_path)})
    roots = [_parse_root(item, targets) for item in raw_roots]
    _validate_unique_roots(roots)
    return CodeSyncConfig(project_id=project_id, mapping_profile=mapping_profile, roots=roots)


def _parse_root(value: Any, targets: list[list[str]]) -> CodeSyncRoot:
    if not isinstance(value, dict):
        raise BridgeError("code_sync_invalid_config", "root entry must be an object", {"root": value})
    root_id = _required_string(value, "root_id")
    local_path = _required_string(value, "local_path").replace("\\", "/")
    if (
        local_path.startswith("/")
        or re.match(r"^[A-Za-z]:(/|$)", local_path) is not None
        or local_path.startswith("../")
        or "/../" in f"/{local_path}/"
        or local_path == ".."
    ):
        raise BridgeError("code_sync_invalid_config", "local_path must be workspace-relative and cannot escape workspace", {"root_id": root_id, "local_path": local_path})
    studio_path = value.get("studio_path")
    if not isinstance(studio_path, list) or not all(isinstance(segment, str) and segment for segment in studio_path):
        raise BridgeError("code_sync_invalid_config", "studio_path must be a non-empty string array", {"root_id": root_id})
    validate_studio_path_allowed(studio_path, targets)
    include = _pattern_list(value.get("include", ["**/*.lua", "**/*.luau"]), "include", root_id)
    exclude = _pattern_list(value.get("exclude", []), "exclude", root_id)
    return CodeSyncRoot(root_id=root_id, local_path=local_path, studio_path=studio_path, include=include, exclude=exclude)


def _required_string(payload: dict, key: str) -> str:
    value = payload.get(key)
    if not isinstance(value, str) or not value:
        raise BridgeError("code_sync_invalid_config", f"{key} must be a non-empty string", {"field": key})
    return value


def _string_list(value: Any, field: str, root_id: str) -> list[str]:
    if not isinstance(value, list) or not all(isinstance(item, str) for item in value):
        raise BridgeError("code_sync_invalid_config", f"{field} must be a string array", {"root_id": root_id, "field": field})
    return list(value)


def _pattern_list(value: Any, field: str, root_id: str) -> list[str]:
    return [pattern.replace("\\", "/") for pattern in _string_list(value, field, root_id)]


def _validate_unique_roots(roots: list[CodeSyncRoot]) -> None:
    seen_ids: set[str] = set()
    seen_paths: set[tuple[str, ...]] = set()
    for root in roots:
        if root.root_id in seen_ids:
            raise BridgeError("code_sync_invalid_config", "duplicate root_id", {"root_id": root.root_id})
        seen_ids.add(root.root_id)
        path = tuple(root.studio_path)
        if path in seen_paths:
            raise BridgeError("code_sync_invalid_config", "duplicate studio_path", {"studio_path": root.studio_path})
        for existing in seen_paths:
            if _is_prefix(path, existing) or _is_prefix(existing, path):
                raise BridgeError("code_sync_invalid_config", "managed root studio_path cannot overlap", {"studio_path": root.studio_path, "other_studio_path": list(existing)})
        seen_paths.add(path)


def _is_prefix(left: tuple[str, ...], right: tuple[str, ...]) -> bool:
    return len(left) <= len(right) and right[: len(left)] == left
