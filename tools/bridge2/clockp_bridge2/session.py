from __future__ import annotations

import json
from dataclasses import dataclass
from pathlib import Path

from .errors import BridgeError


@dataclass(frozen=True)
class Session:
    workspace: Path
    task_id: str
    helper_url: str
    task_session_token: str
    code_sync: dict


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
    task_session_token = str(data.get("task_session_token") or "").strip()
    code_sync = data.get("code_sync") if isinstance(data.get("code_sync"), dict) else {}
    helper_url = str(data.get("helper_url") or "").strip().rstrip("/")
    if not task_id:
        raise BridgeError("session_missing_task_id", "session.json is missing task_id", {"path": str(path)})
    if not task_session_token:
        raise BridgeError("session_missing_task_session_token", "session.json is missing task_session_token", {"path": str(path)})
    if not helper_url:
        raise BridgeError("session_missing_helper_url", "session.json is missing helper_url", {"path": str(path)})
    if not code_sync:
        raise BridgeError("session_missing_code_sync", "session.json is missing code_sync binding", {"path": str(path)})

    return Session(workspace=root, task_id=task_id, helper_url=helper_url, task_session_token=task_session_token, code_sync=code_sync)
