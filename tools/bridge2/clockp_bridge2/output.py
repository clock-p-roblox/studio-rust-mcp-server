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
    sys.stdout.write(json.dumps(payload, ensure_ascii=False, separators=(",", ":")))
    sys.stdout.write("\n")
    sys.stdout.flush()
