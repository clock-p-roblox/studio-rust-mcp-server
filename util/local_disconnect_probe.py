#!/usr/bin/env python3
from __future__ import annotations

import sys


DEPRECATED_MESSAGE = (
    "DEPRECATED: util/local_disconnect_probe.py targeted the obsolete Rust "
    "hub/task-server/helper1/mcp1/runtime-log stack. The current mainline is "
    "Go task-agent -> Go studio-helper/helper2 -> plugin-mcp2, validated from "
    ".clock-p/session.json and helper2 task-scoped APIs. Do not use this probe "
    "as a Phase 9+ validation gate."
)


def main() -> int:
    print(DEPRECATED_MESSAGE, file=sys.stderr)
    return 2


if __name__ == "__main__":
    raise SystemExit(main())
