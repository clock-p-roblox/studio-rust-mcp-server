from __future__ import annotations

from pathlib import Path

from ..errors import BridgeError
from ..session import Session
from ..studio import code_sync_apply as helper_code_sync_apply
from ..studio import mode
from .config import MAPPING_PROFILE, load_config
from .diff import diff_manifests
from .live import _require_stable_edit_mode, query_live_manifest
from .manifest import build_local_manifest
from .mapping import build_logical_tree
from .scanner import scan_root


def apply_code_sync(session: Session, workspace: Path, config_path: Path) -> dict:
    before_mode = _require_stable_edit_mode(mode(session))
    local_manifest = build_local_manifest(workspace, config_path)
    roots_payload = _build_roots_payload(workspace, config_path)
    result = helper_code_sync_apply(
        session,
        {
            "protocol_version": 2,
            "mapping_profile": MAPPING_PROFILE,
            "roots": roots_payload,
        },
    )
    after_mode = _require_stable_edit_mode(mode(session))
    if before_mode.get("mode_seq") != after_mode.get("mode_seq"):
        raise BridgeError("code_sync_state_stale", "Studio mode_seq changed during apply", {"before_mode": before_mode, "after_mode": after_mode})
    live_manifest = query_live_manifest(session, workspace, config_path)
    diff = diff_manifests(local_manifest, live_manifest)
    if not diff["matched"]:
        raise BridgeError("code_sync_verify_failed", "live Studio manifest hash does not match local manifest after apply", {"local": local_manifest, "live": live_manifest, "diff": diff})
    return {"applied": True, "local": local_manifest, "live": live_manifest, "diff": diff, "apply_result": result}


def _build_roots_payload(workspace: Path, config_path: Path) -> list[dict]:
    config = load_config(config_path)
    roots = []
    for root in config.roots:
        files = scan_root(workspace, root)
        logical = build_logical_tree(root.studio_path[-1], files)
        roots.append({"root_id": root.root_id, "studio_path": root.studio_path, "tree": logical.to_payload()})
    return roots
