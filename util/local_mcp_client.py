#!/usr/bin/env python3
from __future__ import annotations

import json
import time
import urllib.error
import urllib.request
from typing import Any


class McpClient:
    def __init__(
        self,
        *,
        base_url: str,
        token: str | None = None,
        request_timeout_sec: float = 60.0,
        tool_retry_attempts: int = 1,
    ) -> None:
        self.url = base_url.rstrip("/") + "/mcp"
        self.token = token
        self.request_timeout_sec = request_timeout_sec
        self.tool_retry_attempts = max(1, tool_retry_attempts)
        self.session_id: str | None = None
        self.next_id = 1
        self._initialize()

    def call_tool_with_diagnostics(self, name: str, arguments: dict[str, Any]) -> tuple[dict[str, Any], dict[str, Any]]:
        started = time.perf_counter()
        last_error: Exception | None = None
        for attempt in range(1, self.tool_retry_attempts + 1):
            try:
                result = self._request(
                    "tools/call",
                    {
                        "name": name,
                        "arguments": arguments,
                    },
                )
                diagnostics = {
                    "attempts": attempt,
                    "elapsed_ms": int((time.perf_counter() - started) * 1000),
                }
                return result, diagnostics
            except Exception as exc:  # noqa: BLE001 - probe reports the final observed error.
                last_error = exc
        raise RuntimeError(f"MCP tool {name} failed after {self.tool_retry_attempts} attempts: {last_error}") from last_error

    def _initialize(self) -> None:
        result, session_id = self._post(
            {
                "jsonrpc": "2.0",
                "id": self._next_id(),
                "method": "initialize",
                "params": {
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": {
                        "name": "studio-rust-mcp-local-probe",
                        "version": "0",
                    },
                },
            },
            require_session=False,
        )
        if "error" in result:
            raise RuntimeError(f"MCP initialize failed: {result['error']}")
        if not session_id:
            raise RuntimeError("MCP initialize response did not include mcp-session-id")
        self.session_id = session_id
        self._post(
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {},
            },
            expect_response=False,
        )

    def _request(self, method: str, params: dict[str, Any]) -> dict[str, Any]:
        payload = {
            "jsonrpc": "2.0",
            "id": self._next_id(),
            "method": method,
            "params": params,
        }
        result, _session_id = self._post(payload)
        if "error" in result:
            raise RuntimeError(f"MCP {method} failed: {result['error']}")
        return result.get("result") or {}

    def _post(
        self,
        payload: dict[str, Any],
        *,
        require_session: bool = True,
        expect_response: bool = True,
    ) -> tuple[dict[str, Any], str | None]:
        headers = {
            "Content-Type": "application/json",
            "Accept": "application/json, text/event-stream",
        }
        if self.token:
            headers["Authorization"] = f"Bearer {self.token}"
        if require_session:
            if not self.session_id:
                raise RuntimeError("MCP session is not initialized")
            headers["mcp-session-id"] = self.session_id

        request = urllib.request.Request(
            self.url,
            data=json.dumps(payload).encode("utf-8"),
            headers=headers,
            method="POST",
        )
        try:
            with urllib.request.urlopen(request, timeout=self.request_timeout_sec) as response:
                body = response.read().decode("utf-8", errors="replace")
                if response.status == 202 and not expect_response:
                    return {}, response.headers.get("mcp-session-id")
                parsed = _parse_streamable_response(body)
                return parsed, response.headers.get("mcp-session-id")
        except urllib.error.HTTPError as exc:
            body = exc.read().decode("utf-8", errors="replace")
            raise RuntimeError(f"MCP HTTP {exc.code}: {body}") from exc
        except urllib.error.URLError as exc:
            raise RuntimeError(f"MCP request failed: {exc}") from exc

    def _next_id(self) -> int:
        value = self.next_id
        self.next_id += 1
        return value


def _parse_streamable_response(body: str) -> dict[str, Any]:
    body = body.strip()
    if not body:
        return {}
    if body.startswith("{"):
        return json.loads(body)

    latest: dict[str, Any] | None = None
    for line in body.splitlines():
        if not line.startswith("data: "):
            continue
        data = line[len("data: ") :].strip()
        if not data or not data.startswith("{"):
            continue
        latest = json.loads(data)
    if latest is None:
        raise RuntimeError(f"MCP response did not contain JSON data: {body[:400]}")
    return latest


def require_non_error_result(name: str, result: dict[str, Any]) -> dict[str, Any]:
    if result.get("isError") is True:
        raise RuntimeError(f"MCP tool {name} returned error: {extract_text_content(result)}")
    return result


def extract_text_content(result: dict[str, Any]) -> str:
    parts: list[str] = []
    for item in result.get("content") or []:
        if isinstance(item, dict) and item.get("type") == "text":
            parts.append(str(item.get("text") or ""))
    return "\n".join(parts)
