from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path
import re

from ..errors import BridgeError
from .config import CodeSyncRoot


@dataclass(frozen=True)
class SourceFile:
    relative_path: str
    source: str
    source_bytes: int


def scan_root(workspace: Path, root: CodeSyncRoot) -> list[SourceFile]:
    base = (workspace / root.local_path).resolve()
    workspace_resolved = workspace.resolve()
    try:
        base.relative_to(workspace_resolved)
    except ValueError as exc:
        raise BridgeError("code_sync_invalid_config", "local_path escapes workspace", {"root_id": root.root_id, "local_path": root.local_path})
    if not base.exists() or not base.is_dir():
        raise BridgeError("code_sync_invalid_config", "local_path must exist and be a directory", {"root_id": root.root_id, "local_path": str(base)})
    result: list[SourceFile] = []
    for path in sorted(base.rglob("*"), key=lambda item: item.as_posix().encode("utf-8")):
        if not path.is_file():
            continue
        rel = path.relative_to(base).as_posix()
        if not _matches(rel, root.include) or _matches(rel, root.exclude):
            continue
        try:
            raw = path.read_bytes()
            text = raw.decode("utf-8")
        except UnicodeDecodeError as exc:
            raise BridgeError("code_sync_unsupported_encoding", "code-sync only supports UTF-8 text files", {"root_id": root.root_id, "path": str(path)}) from exc
        normalized = text.replace("\r\n", "\n").replace("\r", "\n")
        result.append(SourceFile(relative_path=rel, source=normalized, source_bytes=len(normalized.encode("utf-8"))))
    return result


def _matches(relative_path: str, patterns: list[str]) -> bool:
    for pattern in patterns:
        if _glob_match(relative_path, pattern):
            return True
    return False


def _glob_match(relative_path: str, pattern: str) -> bool:
    return re.fullmatch(_glob_to_regex(pattern), relative_path) is not None


def _glob_to_regex(pattern: str) -> str:
    parts: list[str] = []
    index = 0
    while index < len(pattern):
        char = pattern[index]
        if char == "*":
            if index + 1 < len(pattern) and pattern[index + 1] == "*":
                next_index = index + 2
                if next_index < len(pattern) and pattern[next_index] == "/":
                    parts.append("(?:.*/)?")
                    index = next_index + 1
                else:
                    parts.append(".*")
                    index = next_index
            else:
                parts.append("[^/]*")
                index += 1
        elif char == "?":
            parts.append("[^/]")
            index += 1
        else:
            parts.append(re.escape(char))
            index += 1
    return "".join(parts)
