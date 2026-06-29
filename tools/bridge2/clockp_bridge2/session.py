from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from .errors import BridgeError


@dataclass(frozen=True)
class Session:
    workspace: Path
    task_id: str
    helper_base_url: str


def load_session(workspace: str | None = None) -> Session:
    root = Path(workspace or ".").resolve()
    path = root / ".clock-p" / "session.json"
    try:
        body = path.read_text(encoding="utf-8")
    except FileNotFoundError as exc:
        raise BridgeError(
            "session_missing",
            "missing .clock-p/session.json; start task-agent first",
            {"path": str(path)},
        ) from exc
    except OSError as exc:
        raise BridgeError("session_read_failed", str(exc), {"path": str(path)}) from exc

    try:
        data = json.loads(body)
    except json.JSONDecodeError as exc:
        raise BridgeError("session_invalid_json", str(exc), {"path": str(path)}) from exc

    task_id = str(data.get("task_id") or "").strip()
    helper = data.get("helper") if isinstance(data.get("helper"), dict) else {}
    helper_base_url = str(helper.get("base_url") or "").strip().rstrip("/")
    if not task_id:
        raise BridgeError("session_missing_task_id", "session.json is missing task_id", {"path": str(path)})
    if not helper_base_url:
        raise BridgeError("session_missing_helper_base_url", "session.json is missing helper.base_url", {"path": str(path)})

    return Session(workspace=root, task_id=task_id, helper_base_url=helper_base_url)
