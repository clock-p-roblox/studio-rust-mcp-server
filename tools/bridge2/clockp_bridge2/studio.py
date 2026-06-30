from __future__ import annotations

import secrets
import time

from .errors import BridgeError
from .http import get_json, post_json, task_url
from .session import Session

_SAFE_INT_MAX = (1 << 53) - 1


def status(session: Session) -> dict:
    return get_json(task_url(session, "/status"), session=session)


def mode(session: Session) -> dict:
    return get_json(task_url(session, "/studio/mode"), timeout=5.0, session=session)


def play(
    session: Session,
    data: dict | None = None,
    transition_timeout_seconds: float = 30.0,
    poll_seconds: float = 0.5,
) -> dict:
    _ensure_task_live(session)
    before_mode = _require_live_mode(mode(session), "play")
    before_mode_seq = _require_mode_seq(before_mode, "play")
    requested_launch_id = _generate_launch_id()
    request_payload = {"play_args": {"launch_id": requested_launch_id, "data": data or {}}}
    play_response = post_json(task_url(session, "/studio/play"), request_payload, timeout=12.0, session=session)
    _ensure_helper_ok("play", play_response)
    accepted_status = _command_status(play_response)
    if accepted_status != "play_requested":
        raise BridgeError(
            "unexpected_play_response",
            f"unexpected play response status: {accepted_status or 'unknown'}",
            {"play_response": play_response},
        )
    requested_launch_id = int(play_response.get("requested_launch_id") or requested_launch_id)
    started_at = time.monotonic()
    last_mode = before_mode
    deadline = started_at + transition_timeout_seconds
    while True:
        polled_mode = mode(session)
        if _is_transient_mode_unavailable(polled_mode):
            if time.monotonic() >= deadline:
                raise BridgeError(
                    "play_transition_timeout",
                    "timed out waiting for Studio play mode with matching launch_id",
                    {
                        "before_mode": before_mode,
                        "last_mode": polled_mode,
                        "play_response": play_response,
                        "requested_launch_id": requested_launch_id,
                    },
                )
            time.sleep(poll_seconds)
            continue
        last_mode = _require_live_mode(polled_mode, "play")
        current_mode_seq = _require_mode_seq(last_mode, "play")
        current_mode = _mode_value(last_mode)
        if current_mode == "play_server" and current_mode_seq != before_mode_seq:
            launch_id = _launch_id_value(last_mode)
            if launch_id is None:
                raise BridgeError(
                    "launch_id_missing",
                    "play mode changed but current runtime did not report launch_id",
                    {
                        "before_mode": before_mode,
                        "final_mode": last_mode,
                        "play_response": play_response,
                        "requested_launch_id": requested_launch_id,
                    },
                )
            if launch_id != requested_launch_id:
                raise BridgeError(
                    "launch_id_mismatch",
                    "play mode changed but current runtime launch_id does not match this request",
                    {
                        "before_mode": before_mode,
                        "final_mode": last_mode,
                        "play_response": play_response,
                        "requested_launch_id": requested_launch_id,
                        "observed_launch_id": launch_id,
                    },
                )
            return {
                "accepted": True,
                "requested_launch_id": requested_launch_id,
                "play_request": play_response,
                "before_mode": before_mode,
                "final_mode": last_mode,
                "wait_ms": int((time.monotonic() - started_at) * 1000),
            }
        if current_mode_seq != before_mode_seq and current_mode != "play_server":
            raise BridgeError(
                "unexpected_mode_transition",
                f"mode_seq changed but Studio entered unexpected mode: {current_mode}",
                {"before_mode": before_mode, "final_mode": last_mode, "play_response": play_response},
            )
        if time.monotonic() >= deadline:
            raise BridgeError(
                "play_transition_timeout",
                "timed out waiting for Studio play mode with matching launch_id",
                {
                    "before_mode": before_mode,
                    "last_mode": last_mode,
                    "play_response": play_response,
                    "requested_launch_id": requested_launch_id,
                },
            )
        time.sleep(poll_seconds)


def stop(session: Session, transition_timeout_seconds: float = 20.0, poll_seconds: float = 0.5) -> dict:
    _ensure_task_live(session)
    before_mode = _require_live_mode(mode(session), "stop")
    before_mode_seq = _require_mode_seq(before_mode, "stop")
    stop_response = post_json(task_url(session, "/studio/stop"), timeout=12.0, session=session)
    _ensure_helper_ok("stop", stop_response)
    accepted_status = _command_status(stop_response)
    if accepted_status == "already_stopped":
        return {
            "accepted": True,
            "stop_request": stop_response,
            "before_mode": before_mode,
            "final_mode": before_mode,
            "wait_ms": 0,
            "reason": "already_stopped",
        }
    if accepted_status != "stop_requested":
        raise BridgeError(
            "unexpected_stop_response",
            f"unexpected stop response status: {accepted_status or 'unknown'}",
            {"stop_response": stop_response},
        )
    started_at = time.monotonic()
    last_mode = before_mode
    deadline = started_at + transition_timeout_seconds
    while True:
        polled_mode = mode(session)
        if _is_transient_mode_unavailable(polled_mode):
            if time.monotonic() >= deadline:
                raise BridgeError(
                    "stop_transition_timeout",
                    "timed out waiting for Studio edit mode after stop request",
                    {"before_mode": before_mode, "last_mode": polled_mode, "stop_response": stop_response},
                )
            time.sleep(poll_seconds)
            continue
        last_mode = _require_live_mode(polled_mode, "stop")
        current_mode_seq = _require_mode_seq(last_mode, "stop")
        current_mode = _mode_value(last_mode)
        if current_mode == "edit" and current_mode_seq != before_mode_seq:
            return {
                "accepted": True,
                "stop_request": stop_response,
                "before_mode": before_mode,
                "final_mode": last_mode,
                "wait_ms": int((time.monotonic() - started_at) * 1000),
            }
        if current_mode_seq != before_mode_seq and current_mode != "edit":
            raise BridgeError(
                "unexpected_mode_transition",
                f"mode_seq changed but Studio entered unexpected mode during stop: {current_mode}",
                {"before_mode": before_mode, "final_mode": last_mode, "stop_response": stop_response},
            )
        if time.monotonic() >= deadline:
            raise BridgeError(
                "stop_transition_timeout",
                "timed out waiting for Studio edit mode after stop request",
                {"before_mode": before_mode, "last_mode": last_mode, "stop_response": stop_response},
            )
        time.sleep(poll_seconds)


def screenshot(session: Session) -> dict:
    return get_json(task_url(session, "/studio/screenshot"), timeout=30.0, session=session)


def play_mode_logs(session: Session, cursor: str | None = None, limit: int | None = None) -> dict:
    return get_json(task_url(session, "/runtime-log", {"cursor": cursor, "limit": limit}), session=session)


def run_code_direct(session: Session, code: str) -> dict:
    return post_json(task_url(session, "/studio/run-code-direct"), {"code": code}, timeout=30.0, session=session)


def code_sync_get_manifest(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/code-sync/get-manifest"), payload, timeout=30.0, session=session)


def code_sync_apply(session: Session, payload: dict) -> dict:
    return post_json(task_url(session, "/code-sync/apply"), payload, timeout=30.0, session=session)


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


def ensure_edit_mode(
    session: Session,
    stop_timeout_seconds: float = 20.0,
    poll_seconds: float = 0.5,
) -> dict:
    _ensure_task_live(session)
    last_mode = _require_live_mode(mode(session), "ensure_edit")
    if _mode_value(last_mode) == "edit":
        return {"reason": "already_edit", "last_mode": last_mode}
    if _mode_value(last_mode) != "play_server":
        raise BridgeError("studio_mode_not_editable", "Studio is not in edit or play_server mode", {"last_mode": last_mode})
    stop_result = stop(session, transition_timeout_seconds=stop_timeout_seconds, poll_seconds=poll_seconds)
    return {"reason": "stopped_play", "last_mode": stop_result["final_mode"], "stop": stop_result}


def _ensure_task_live(session: Session) -> None:
    task_status = status(session)
    state = task_status.get("state")
    if task_status.get("ok") is not True or state != "live":
        raise BridgeError("task_not_live", "task session is not live", {"status": task_status})


def _require_live_mode(payload: dict, command: str) -> dict:
    if payload.get("ok") is not True:
        raise BridgeError(f"{command}_mode_unavailable", f"{command} requires a successful Studio mode query", {"mode": payload})
    if payload.get("available") is False:
        raise BridgeError(f"{command}_mode_unavailable", f"{command} requires a live Studio mode query", {"mode": payload})
    return payload


def _is_transient_mode_unavailable(payload: dict) -> bool:
    if payload.get("ok") is not True or payload.get("available") is not False:
        return False
    reason = str(payload.get("reason") or "")
    return reason in {"mode_seq_changed", "mode_query_timeout"}


def _require_mode_seq(payload: dict, command: str) -> int:
    value = payload.get("mode_seq")
    if isinstance(value, int) and value > 0:
        return value
    if isinstance(value, float) and value > 0 and value.is_integer():
        return int(value)
    raise BridgeError(f"{command}_mode_seq_missing", f"{command} requires mode_seq in Studio mode payload", {"mode": payload})


def _ensure_helper_ok(command: str, response: dict) -> None:
    if response.get("ok") is not False:
        return
    command_result = response.get("command_result")
    command_error = ""
    if isinstance(command_result, dict):
        command_error = str(command_result.get("error") or "")
    code = str(response.get("code") or _error_code_from_message(command_error) or "helper_command_failed")
    message = str(response.get("message") or response.get("error") or command_error or f"{command} failed")
    raise BridgeError(code, message, response)


def _error_code_from_message(message: str) -> str:
    prefix, separator, _suffix = message.partition(":")
    candidate = prefix.strip()
    if separator and candidate and " " not in candidate:
        return candidate
    return ""


def _command_status(payload: dict) -> str:
    command_result = payload.get("command_result")
    if isinstance(command_result, dict):
        result = command_result.get("result")
        if isinstance(result, dict):
            value = result.get("status")
            if isinstance(value, str):
                return value
    return ""


def _mode_value(payload: dict) -> str:
    return str(payload.get("mode") or "unknown")


def _launch_id_value(payload: dict) -> int | None:
    value = payload.get("launch_id")
    if isinstance(value, int) and value > 0:
        return value
    if isinstance(value, float) and value > 0 and value.is_integer():
        return int(value)
    return None


def _generate_launch_id() -> int:
    return secrets.randbelow(_SAFE_INT_MAX) + 1
