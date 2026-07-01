from __future__ import annotations

import json
import os
from pathlib import Path
import http.client
import ssl
from urllib import parse

from .errors import BridgeError
from .session import Session


def task_url(session: Session, path: str, query: dict[str, str | int | None] | None = None) -> str:
    quoted_task = parse.quote(session.task_id, safe="")
    url = f"{session.helper_url}/session/{quoted_task}{path}"
    if query:
        filtered = {key: str(value) for key, value in query.items() if value is not None and str(value) != ""}
        if filtered:
            url = f"{url}?{parse.urlencode(filtered)}"
    return url


def get_json(url: str, timeout: float = 10.0, session: Session | None = None) -> dict:
    return _request_json("GET", url, None, timeout, session)


def post_json(url: str, payload: dict | None = None, timeout: float = 10.0, session: Session | None = None) -> dict:
    return _request_json("POST", url, payload or {}, timeout, session)


def _request_json(method: str, url: str, payload: dict | None, timeout: float, session: Session | None) -> dict:
    body = None
    headers = {"Accept": "application/json"}
    if payload is not None:
        body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
        headers["Content-Type"] = "application/json"
    auth_header = _public_helper_auth_header(url, session)
    if auth_header:
        headers["Authorization"] = auth_header
    if session is not None and session.task_session_token:
        headers["X-ClockP-Task-Token"] = session.task_session_token

    try:
        status, raw_body = _send_request(method, url, body, headers, timeout)
    except (OSError, TimeoutError, http.client.HTTPException, ssl.SSLError) as exc:
        raise BridgeError("helper_unreachable", str(exc), {"url": url}) from exc

    raw = raw_body.decode("utf-8", errors="replace")
    if status >= 400:
        try:
            parsed = json.loads(raw) if raw else {}
        except json.JSONDecodeError:
            parsed = {"raw": raw}
        raise BridgeError(
            _error_code_from_body(parsed, "helper_http_error"),
            _error_message_from_body(parsed, f"helper returned HTTP {status}"),
            {"url": url, "status": status, "body": parsed},
        )

    try:
        parsed = json.loads(raw) if raw else {}
    except json.JSONDecodeError as exc:
        raise BridgeError("helper_invalid_json", str(exc), {"url": url, "status": status, "raw": raw}) from exc
    if not isinstance(parsed, dict):
        raise BridgeError("helper_invalid_json", "helper response is not a JSON object", {"url": url, "status": status})
    return parsed


def _send_request(method: str, url: str, body: bytes | None, headers: dict[str, str], timeout: float) -> tuple[int, bytes]:
    parsed = parse.urlparse(url)
    if parsed.scheme not in ("http", "https"):
        raise BridgeError("helper_invalid_url", f"unsupported helper URL scheme: {parsed.scheme or '<empty>'}", {"url": url})
    if not parsed.hostname:
        raise BridgeError("helper_invalid_url", "helper URL is missing a host", {"url": url})

    path = parsed.path or "/"
    if parsed.query:
        path = f"{path}?{parsed.query}"

    if parsed.scheme == "https":
        conn: http.client.HTTPConnection = http.client.HTTPSConnection(parsed.hostname, parsed.port, timeout=timeout)
    else:
        conn = http.client.HTTPConnection(parsed.hostname, parsed.port, timeout=timeout)

    try:
        conn.request(method, path, body=body, headers=headers)
        resp = conn.getresponse()
        raw = resp.read()
        return resp.status, raw
    finally:
        conn.close()


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


def _public_helper_auth_header(url: str, session: Session | None) -> str:
    parsed = parse.urlparse(url)
    if parsed.scheme != "https":
        return ""
    host = (parsed.hostname or "").strip().lower()
    if not host.endswith(".dev.clock-p.com"):
        return ""
    token = _resolve_public_helper_bearer(session)
    return f"Bearer {token}"


def _resolve_public_helper_bearer(session: Session | None) -> str:
    workspace = session.workspace if session is not None else None
    candidates = _token_candidates(workspace)
    for candidate in candidates:
        try:
            value = candidate.read_text(encoding="utf-8").strip()
        except OSError:
            continue
        if value:
            return value
    details = {"token_candidates": [str(path) for path in candidates]}
    raise BridgeError("helper_bearer_missing", "feishu-token is required for public helper requests", details)


def _token_candidates(workspace: Path | None) -> list[Path]:
    candidates: list[Path] = []
    if workspace is not None:
        candidates.append(workspace / ".dev.clock-p.com" / "feishu-token")
    for env_name, suffix in (
        ("APPDATA", ("dev.clock-p.com", "feishu-token")),
        ("USERPROFILE", (".dev.clock-p.com", "feishu-token")),
        ("HOME", (".dev.clock-p.com", "feishu-token")),
    ):
        base_text = os.environ.get(env_name, "").strip()
        if base_text:
            base = Path(base_text)
            candidates.append(base.joinpath(*suffix))
    return candidates
