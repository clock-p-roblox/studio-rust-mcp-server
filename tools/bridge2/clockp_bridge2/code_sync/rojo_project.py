from __future__ import annotations

import json
from pathlib import Path
from typing import Any

from ..errors import BridgeError


def load_project_targets(project_path: Path) -> list[list[str]]:
    try:
        payload = json.loads(project_path.read_text(encoding="utf-8-sig"))
    except OSError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(project_path)}) from exc
    except json.JSONDecodeError as exc:
        raise BridgeError("code_sync_invalid_config", str(exc), {"path": str(project_path)}) from exc
    tree = payload.get("tree")
    if not isinstance(tree, dict):
        raise BridgeError("code_sync_invalid_config", "Rojo project missing tree object", {"path": str(project_path)})
    targets: list[list[str]] = []
    for name, child in tree.items():
        if name.startswith("$") or not isinstance(child, dict):
            continue
        _collect_targets(child, [name], targets, is_service=True)
    return _dedupe_paths(targets)


def _collect_targets(node: dict[str, Any], path: list[str], targets: list[list[str]], *, is_service: bool) -> None:
    has_path = "$path" in node
    explicit_children = [
        (name, child)
        for name, child in node.items()
        if not name.startswith("$") and isinstance(child, dict)
    ]
    if has_path or not explicit_children:
        targets.append(path)
    elif not is_service:
        targets.append(path)
    for name, child in explicit_children:
        _collect_targets(child, [*path, name], targets, is_service=False)


def _dedupe_paths(paths: list[list[str]]) -> list[list[str]]:
    seen: set[tuple[str, ...]] = set()
    result: list[list[str]] = []
    for path in paths:
        key = tuple(path)
        if key not in seen:
            seen.add(key)
            result.append(path)
    return sorted(result, key=lambda item: [segment.encode("utf-8") for segment in item])


def validate_studio_path_allowed(studio_path: list[str], targets: list[list[str]]) -> None:
    if not studio_path or any(not segment for segment in studio_path):
        raise BridgeError("code_sync_invalid_config", "studio_path must contain non-empty path segments", {"studio_path": studio_path})
    for target in targets:
        if studio_path[: len(target)] == target:
            return
    raise BridgeError(
        "code_sync_invalid_config",
        "studio_path is not declared by the Rojo project target allowlist",
        {"studio_path": studio_path, "studio_target_allowlist": targets},
    )
