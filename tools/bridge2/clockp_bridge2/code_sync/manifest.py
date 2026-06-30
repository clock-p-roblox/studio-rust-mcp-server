from __future__ import annotations

import hashlib
import json
from pathlib import Path

from .config import load_config
from .hashing import combined_hash, config_hash, root_hash
from .mapping import build_logical_tree
from .project import load_project_targets
from .scanner import scan_root


PROTOCOL_VERSION = 1


def build_local_manifest(workspace: Path, config_path: Path, project_path: Path) -> dict:
    workspace = workspace.resolve()
    targets = load_project_targets(project_path)
    config = load_config(config_path, targets)
    config_dicts = [root.as_config_dict() for root in config.roots]
    cfg_hash = config_hash(PROTOCOL_VERSION, config.project_id, config.mapping_profile, targets, config_dicts)
    root_results = []
    total_files = 0
    total_source_bytes = 0
    root_hashes: list[tuple[str, str]] = []
    for root in config.roots:
        files = scan_root(workspace, root)
        logical = build_logical_tree(root.studio_path[-1], files)
        current_root_hash = root_hash(root.root_id, config.mapping_profile, logical)
        root_hashes.append((root.root_id, current_root_hash))
        total_files += len(files)
        total_source_bytes += sum(item.source_bytes for item in files)
        root_results.append(
            {
                "root_id": root.root_id,
                "local_path": root.local_path,
                "studio_path": root.studio_path,
                "root_kind": logical.kind,
                "root_hash": current_root_hash,
                "children_count": len(logical.children),
                "children_complete": True,
                "children": logical.child_summaries(),
                "file_count": len(files),
                "normalized_source_bytes": sum(item.source_bytes for item in files),
            }
        )
    payload = {
        "protocol_version": PROTOCOL_VERSION,
        "workspace_id": _workspace_id(workspace),
        "project_id": config.project_id,
        "mapping_profile": config.mapping_profile,
        "code_sync_config_hash": cfg_hash,
        "studio_target_allowlist": targets,
        "combined_hash": combined_hash(config.mapping_profile, root_hashes),
        "roots": root_results,
        "file_count": total_files,
        "normalized_source_bytes": total_source_bytes,
        "estimated_apply_body_bytes": _estimate_apply_body_bytes(root_results, total_source_bytes),
    }
    return payload


def _workspace_id(workspace: Path) -> str:
    normalized = workspace.as_posix().lower()
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()[:24]


def _estimate_apply_body_bytes(roots: list[dict], source_bytes: int) -> int:
    shell = {"protocol_version": PROTOCOL_VERSION, "roots": roots, "ops": []}
    return len(json.dumps(shell, ensure_ascii=False, separators=(",", ":")).encode("utf-8")) + source_bytes
