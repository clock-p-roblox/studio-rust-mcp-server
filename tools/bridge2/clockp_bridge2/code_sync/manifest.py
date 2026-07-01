from __future__ import annotations

import hashlib
import json
from pathlib import Path

from .config import MAPPING_PROFILE, load_config
from .hashing import canonical_studio_path, combined_hash, config_hash, target_hash
from .targets import build_targets


PROTOCOL_VERSION = 2


def build_local_manifest(workspace: Path, config_path: Path) -> dict:
    workspace = workspace.resolve()
    config, targets = build_targets(workspace, config_path)
    config_dicts = [node.as_config_dict() for node in config.nodes]
    cfg_hash = config_hash(PROTOCOL_VERSION, MAPPING_PROFILE, config_dicts)
    target_results = []
    total_files = 0
    total_source_bytes = 0
    target_hashes: list[tuple[str, str]] = []
    for target in targets:
        current_target_hash = target_hash(target.studio_path, MAPPING_PROFILE, target.tree)
        target_id = canonical_studio_path(target.studio_path)
        target_hashes.append((target_id, current_target_hash))
        total_files += target.file_count
        total_source_bytes += target.normalized_source_bytes
        target_results.append(
            {
                "target_id": target_id,
                "studio_path": target.studio_path,
                "root_kind": target.tree.kind,
                "target_hash": current_target_hash,
                "children_count": len(target.tree.children),
                "children_complete": True,
                "children": target.tree.child_summaries(),
                "file_count": target.file_count,
                "normalized_source_bytes": target.normalized_source_bytes,
            }
        )
    payload = {
        "protocol_version": PROTOCOL_VERSION,
        "workspace_id": _workspace_id(workspace),
        "mapping_profile": MAPPING_PROFILE,
        "code_sync_config_hash": cfg_hash,
        "combined_hash": combined_hash(MAPPING_PROFILE, target_hashes),
        "targets": target_results,
        "file_count": total_files,
        "normalized_source_bytes": total_source_bytes,
        "estimated_apply_body_bytes": _estimate_apply_body_bytes(target_results, total_source_bytes),
    }
    return payload


def _workspace_id(workspace: Path) -> str:
    normalized = workspace.as_posix().lower()
    return hashlib.sha256(normalized.encode("utf-8")).hexdigest()[:24]


def _estimate_apply_body_bytes(targets: list[dict], source_bytes: int) -> int:
    shell = {"protocol_version": PROTOCOL_VERSION, "targets": targets, "ops": []}
    return len(json.dumps(shell, ensure_ascii=False, separators=(",", ":")).encode("utf-8")) + source_bytes
