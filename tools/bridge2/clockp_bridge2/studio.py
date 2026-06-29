from __future__ import annotations

import time

from .errors import BridgeError
from .http import get_json, post_json, task_url
from .session import Session


def status(session: Session) -> dict:
    return get_json(task_url(session, "/status"))


def mode(session: Session) -> dict:
    return get_json(task_url(session, "/studio/mode"))


def play(session: Session) -> dict:
    return post_json(task_url(session, "/studio/play"))


def stop(session: Session) -> dict:
    return post_json(task_url(session, "/studio/stop"))


def screenshot(session: Session) -> dict:
    return get_json(task_url(session, "/studio/screenshot"), timeout=30.0)


def play_mode_logs(session: Session, cursor: str | None = None, limit: int | None = None) -> dict:
    return get_json(task_url(session, "/runtime-log", {"cursor": cursor, "limit": limit}))


def run_code_direct(session: Session, code: str) -> dict:
    return post_json(task_url(session, "/studio/run-code-direct"), {"code": code}, timeout=30.0)


def ensure_edit_mode(session: Session, timeout_seconds: float = 20.0, poll_seconds: float = 0.5) -> dict:
    task_status = status(session)
    state = task_status.get("state")
    if task_status.get("ok") is not True or state != "live":
        raise BridgeError("task_not_live", "task session is not live", {"status": task_status})

    last_mode = mode(session)
    if _mode_value(last_mode) == "edit":
        return {"reason": "already_edit", "last_mode": last_mode}

    if _mode_value(last_mode) != "play_server":
        raise BridgeError("studio_mode_not_editable", "Studio is not in edit or play_server mode", {"last_mode": last_mode})

    stop(session)
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        time.sleep(poll_seconds)
        last_mode = mode(session)
        if _mode_value(last_mode) == "edit":
            return {"reason": "stopped_play", "last_mode": last_mode}

    raise BridgeError("ensure_edit_timeout", "timed out waiting for Studio edit mode", {"last_mode": last_mode})


def _mode_value(payload: dict) -> str:
    return str(payload.get("mode") or "unknown")
