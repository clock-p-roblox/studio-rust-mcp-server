from __future__ import annotations

import time

from .errors import BridgeError
from .http import get_json, post_json, task_url
from .session import Session


def status(session: Session) -> dict:
    return get_json(task_url(session, "/status"), session=session)


def mode(session: Session) -> dict:
    return get_json(task_url(session, "/studio/mode"), session=session)


def play(session: Session) -> dict:
    return post_json(task_url(session, "/studio/play"), session=session)


def stop(session: Session) -> dict:
    return post_json(task_url(session, "/studio/stop"), session=session)


def screenshot(session: Session) -> dict:
    return get_json(task_url(session, "/studio/screenshot"), timeout=30.0, session=session)


def play_mode_logs(session: Session, cursor: str | None = None, limit: int | None = None) -> dict:
    return get_json(task_url(session, "/runtime-log", {"cursor": cursor, "limit": limit}), session=session)


def run_code_direct(session: Session, code: str) -> dict:
    return post_json(task_url(session, "/studio/run-code-direct"), {"code": code}, timeout=30.0, session=session)


def official_ping(session: Session) -> dict:
    return post_json(task_url(session, "/official/ping"), {}, timeout=30.0, session=session)


def official_store_image(session: Session, file_path: str) -> dict:
    return post_json(task_url(session, "/official/store-image"), {"file_path": file_path}, timeout=120.0, session=session)


def official_generate_mesh(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/official/generate-mesh"), payload, timeout=None, session=session)


def official_generate_procedural_model(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/official/generate-procedural-model"), payload, timeout=None, session=session)


def official_wait_job(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/official/wait-job"), payload, timeout=None, session=session)


def official_search_creator_store(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/official/search-creator-store"), payload, timeout=120.0, session=session)


def official_insert_from_creator_store(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/official/insert-from-creator-store"), payload, timeout=None, session=session)


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
