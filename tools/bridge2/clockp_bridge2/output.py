from __future__ import annotations

import json
import sys


def success(command: str, details: dict | None = None) -> int:
    _write_json({"ok": True, "command": command, "details": details or {}})
    return 0


def failure(command: str, code: str, message: str, details: dict | None = None, exit_code: int = 1) -> int:
    _write_json(
        {
            "ok": False,
            "command": command,
            "code": code,
            "message": message,
            "details": details or {},
        }
    )
    return exit_code


def _write_json(payload: dict) -> None:
    text = json.dumps(payload, ensure_ascii=False, separators=(",", ":")) + "\n"
    buffer = getattr(sys.stdout, "buffer", None)
    if buffer is not None:
        buffer.write(text.encode("utf-8"))
    else:
        sys.stdout.write(text)
    sys.stdout.flush()
