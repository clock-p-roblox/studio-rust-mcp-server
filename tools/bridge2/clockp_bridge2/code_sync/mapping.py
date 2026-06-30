from __future__ import annotations

from dataclasses import dataclass, field
from pathlib import PurePosixPath

from ..errors import BridgeError
from .hashing import LogicalNode
from .scanner import SourceFile


@dataclass
class _DirNode:
    name: str
    files: dict[str, SourceFile] = field(default_factory=dict)
    dirs: dict[str, "_DirNode"] = field(default_factory=dict)


def build_logical_tree(root_name: str, files: list[SourceFile]) -> LogicalNode:
    root = _DirNode(root_name)
    for source_file in files:
        parts = list(PurePosixPath(source_file.relative_path).parts)
        if not parts:
            continue
        current = root
        for part in parts[:-1]:
            current = current.dirs.setdefault(part, _DirNode(part))
        if parts[-1] in current.files:
            raise BridgeError("code_sync_invalid_config", "duplicate logical file path", {"path": source_file.relative_path})
        current.files[parts[-1]] = source_file
    return _dir_to_logical(root)


def _dir_to_logical(directory: _DirNode) -> LogicalNode:
    init_file = _choose_init_file(directory)
    if init_file is not None:
        file_name, source_file, kind = init_file
        children = _children_for_dir(directory, skip_file=file_name)
        return LogicalNode(name=directory.name, kind=kind, source=source_file.source, children=children)
    return LogicalNode(name=directory.name, kind="Folder", children=_children_for_dir(directory, skip_file=None))


def _children_for_dir(directory: _DirNode, *, skip_file: str | None) -> list[LogicalNode]:
    children: list[LogicalNode] = []
    seen_names: set[str] = set()
    for child_dir in directory.dirs.values():
        child = _dir_to_logical(child_dir)
        if child.name in seen_names:
            raise BridgeError("code_sync_unsupported_mapping", "file and directory map to the same logical name", {"name": child.name})
        seen_names.add(child.name)
        children.append(child)
    for file_name, source_file in directory.files.items():
        if file_name == skip_file:
            continue
        mapped = _map_script_file(file_name, source_file)
        if mapped is None:
            raise BridgeError("code_sync_unsupported_mapping", "unsupported file in code-sync root", {"path": source_file.relative_path})
        if mapped.name in seen_names:
            raise BridgeError("code_sync_unsupported_mapping", "file and directory map to the same logical name", {"name": mapped.name, "path": source_file.relative_path})
        seen_names.add(mapped.name)
        children.append(mapped)
    return children


def _choose_init_file(directory: _DirNode) -> tuple[str, SourceFile, str] | None:
    candidates = [
        ("init.server.lua", "Script"),
        ("init.server.luau", "Script"),
        ("init.client.lua", "LocalScript"),
        ("init.client.luau", "LocalScript"),
        ("init.lua", "ModuleScript"),
        ("init.luau", "ModuleScript"),
    ]
    matches = [(file_name, directory.files[file_name], kind) for file_name, kind in candidates if file_name in directory.files]
    if len(matches) > 1:
        raise BridgeError("code_sync_unsupported_mapping", "multiple init files in one directory are not supported", {"directory": directory.name, "files": [match[0] for match in matches]})
    return matches[0] if matches else None


def _map_script_file(file_name: str, source_file: SourceFile) -> LogicalNode | None:
    for suffix, kind in (
        (".server.lua", "Script"),
        (".server.luau", "Script"),
        (".client.lua", "LocalScript"),
        (".client.luau", "LocalScript"),
        (".lua", "ModuleScript"),
        (".luau", "ModuleScript"),
    ):
        if file_name.endswith(suffix):
            return LogicalNode(name=file_name[: -len(suffix)], kind=kind, source=source_file.source)
    return None
