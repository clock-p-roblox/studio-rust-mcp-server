from __future__ import annotations

import json
import sys
import traceback


def _main(argv: list[str]) -> int:
    try:
        from clockp_bridge2.commands import main
    except Exception as exc:  # noqa: BLE001 - CLI import failures must stay JSON-only.
        print(
            json.dumps(
                {
                    "ok": False,
                    "command": "unknown",
                    "code": "cli_import_failed",
                    "message": str(exc),
                    "details": {"traceback": traceback.format_exc()},
                },
                ensure_ascii=False,
                separators=(",", ":"),
            )
        )
        return 1
    return main(argv)


if __name__ == "__main__":
    raise SystemExit(_main(sys.argv[1:]))
