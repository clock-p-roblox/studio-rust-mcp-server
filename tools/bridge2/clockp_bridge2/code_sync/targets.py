from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

from .config import CodeSyncConfig, CodeSyncNode, load_config
from .hashing import LogicalNode, canonical_studio_path
from .mapping import build_logical_tree, graft_child_tree
from .scanner import scan_root


@dataclass(frozen=True)
class CodeSyncTarget:
    studio_path: list[str]
    tree: LogicalNode
    file_count: int
    normalized_source_bytes: int

    @property
    def target_id(self) -> str:
        return canonical_studio_path(self.studio_path)

    def route_payload(self) -> dict:
        return {"studio_path": list(self.studio_path)}

    def apply_payload(self) -> dict:
        return {"studio_path": list(self.studio_path), "tree": self.tree.to_payload()}


def build_targets(workspace: Path, config_path: Path) -> tuple[CodeSyncConfig, list[CodeSyncTarget]]:
    config = load_config(config_path)
    workspace = workspace.resolve()
    tree_by_path: dict[tuple[str, ...], LogicalNode] = {}
    stats_by_path: dict[tuple[str, ...], tuple[int, int]] = {}
    for node in config.nodes:
        files = scan_root(workspace, node)
        tree = build_logical_tree(node.studio_path[-1], files, node.kind)
        path = tuple(node.studio_path)
        tree_by_path[path] = tree
        stats_by_path[path] = (len(files), sum(item.source_bytes for item in files))
    paths = sorted(tree_by_path, key=len, reverse=True)
    child_paths_by_parent = _child_paths_by_parent([tuple(node.studio_path) for node in config.nodes])
    for parent_path in sorted(child_paths_by_parent, key=len, reverse=True):
        child_paths = child_paths_by_parent[parent_path]
        parent_tree = tree_by_path[parent_path]
        for child_path in sorted(child_paths, key=lambda item: item):
            relative = list(child_path[len(parent_path) :])
            graft_child_tree(parent_tree, relative, tree_by_path[child_path])
            parent_files, parent_bytes = stats_by_path[parent_path]
            child_files, child_bytes = stats_by_path[child_path]
            stats_by_path[parent_path] = (parent_files + child_files, parent_bytes + child_bytes)
    child_set = {child for children in child_paths_by_parent.values() for child in children}
    targets = []
    for path in sorted((path for path in paths if path not in child_set), key=lambda item: canonical_studio_path(list(item))):
        file_count, source_bytes = stats_by_path[path]
        targets.append(CodeSyncTarget(studio_path=list(path), tree=tree_by_path[path], file_count=file_count, normalized_source_bytes=source_bytes))
    return config, targets


def _child_paths_by_parent(paths: list[tuple[str, ...]]) -> dict[tuple[str, ...], list[tuple[str, ...]]]:
    result: dict[tuple[str, ...], list[tuple[str, ...]]] = {}
    ordered = sorted(paths, key=len)
    for path in ordered:
        parent = _nearest_parent(path, ordered)
        if parent is not None:
            result.setdefault(parent, []).append(path)
    return result


def _nearest_parent(path: tuple[str, ...], candidates: list[tuple[str, ...]]) -> tuple[str, ...] | None:
    best: tuple[str, ...] | None = None
    for candidate in candidates:
        if candidate == path or len(candidate) >= len(path):
            continue
        if path[: len(candidate)] != candidate:
            continue
        if best is None or len(candidate) > len(best):
            best = candidate
    return best
