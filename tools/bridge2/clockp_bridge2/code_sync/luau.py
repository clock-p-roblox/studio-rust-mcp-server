from __future__ import annotations


def long_string_literal(value: str) -> str:
    for equals_count in range(1, 8):
        marker = "=" * equals_count
        close = "]" + marker + "]"
        if close not in value:
            return "[" + marker + "[" + value + close
    raise ValueError("unable to encode Luau long string literal")
