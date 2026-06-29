from __future__ import annotations


class BridgeError(Exception):
    def __init__(
        self,
        code: str,
        message: str,
        details: dict | None = None,
        exit_code: int = 1,
    ) -> None:
        super().__init__(message)
        self.code = code
        self.message = message
        self.details = details or {}
        self.exit_code = exit_code
