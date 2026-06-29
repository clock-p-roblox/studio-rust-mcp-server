from __future__ import annotations

import json
from urllib import error, parse, request

from .errors import BridgeError
from .session import Session


def task_url(session: Session, path: str, query: dict[str, str | int | None] | None = None) -> str:
    quoted_task = parse.quote(session.task_id, safe="")
    url = f"{session.helper_base_url}/session/{quoted_task}{path}"
    if query:
        filtered = {key: str(value) for key, value in query.items() if value is not None and str(value) != ""}
        if filtered:
            url = f"{url}?{parse.urlencode(filtered)}"
    return url


def get_json(url: str, timeout: float = 10.0) -> dict:
    return _request_json("GET", url, None, timeout)


def post_json(url: str, payload: dict | None = None, timeout: float = 10.0) -> dict:
    return _request_json("POST", url, payload or {}, timeout)


def _request_json(method: str, url: str, payload: dict | None, timeout: float) -> dict:
    body = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = request.Request(url, data=body, headers=headers, method=method)
    try:
        with request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8")
            status = resp.status
    except error.HTTPError as exc:
        raw = exc.read().decode("utf-8", errors="replace")
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"raw": raw}
        raise BridgeError(
            _error_code_from_body(parsed, "helper_http_error"),
            _error_message_from_body(parsed, f"helper returned HTTP {exc.code}"),
            {"url": url, "status": exc.code, "body": parsed},
        ) from exc
    except error.URLError as exc:
        raise BridgeError("helper_unreachable", str(exc.reason), {"url": url}) from exc

    try:
        parsed = json.loads(raw) if raw else {}
    except json.JSONDecodeError as exc:
        raise BridgeError("helper_invalid_json", str(exc), {"url": url, "status": status, "raw": raw}) from exc
    if not isinstance(parsed, dict):
        raise BridgeError("helper_invalid_json", "helper response is not a JSON object", {"url": url, "status": status})
    return parsed


def _error_code_from_body(body: dict, default: str) -> str:
    for key in ("code", "reason"):
        value = body.get(key)
        if isinstance(value, str) and value:
            return value
    return default


def _error_message_from_body(body: dict, default: str) -> str:
    for key in ("message", "error"):
        value = body.get(key)
        if isinstance(value, str) and value:
            return value
    return default
