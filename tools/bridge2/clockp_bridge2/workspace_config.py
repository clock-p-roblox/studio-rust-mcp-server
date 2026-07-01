from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from .errors import BridgeError


@dataclass(frozen=True)
class WorkspaceConfig:
    place_id: str
    code_sync_config: str


def load_workspace_config(workspace: Path) -> WorkspaceConfig:
    path = workspace / "clock-p.workspace.json"
    try:
        body = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise BridgeError("workspace_config_missing", "missing clock-p.workspace.json", {"path": str(path)}) from exc
    except OSError as exc:
        raise BridgeError("workspace_config_read_failed", str(exc), {"path": str(path)}) from exc

    try:
        data = json.loads(body)
    except json.JSONDecodeError as exc:
        raise BridgeError("workspace_config_invalid_json", str(exc), {"path": str(path)}) from exc
    if not isinstance(data, dict):
        raise BridgeError("workspace_config_invalid_json", "clock-p.workspace.json must be a JSON object", {"path": str(path)})

    place_id = str(data.get("place_id") or "").strip()
    if not place_id:
        raise BridgeError("workspace_config_missing_place_id", "clock-p.workspace.json is missing place_id", {"path": str(path)})
    if not place_id.isdigit():
        raise BridgeError("workspace_config_invalid_place_id", "clock-p.workspace.json place_id must contain digits only", {"path": str(path), "place_id": place_id})

    code_sync_config = str(data.get("code_sync_config") or "code-sync.roots.json").strip() or "code-sync.roots.json"
    return WorkspaceConfig(place_id=place_id, code_sync_config=code_sync_config)
