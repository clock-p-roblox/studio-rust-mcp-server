from __future__ import annotations

from pathlib import Path

from ..errors import BridgeError
from ..session import Session
from ..studio import code_sync_apply as helper_code_sync_apply
from ..studio import mode
from .config import MAPPING_PROFILE
from .diff import diff_manifests
from .live import _require_stable_edit_mode, query_live_manifest
from .manifest import build_local_manifest
from .targets import build_targets


def apply_code_sync(session: Session, workspace: Path, config_path: Path) -> dict:
    before_mode = _require_stable_edit_mode(mode(session))
    local_manifest = build_local_manifest(workspace, config_path)
    targets_payload = _build_targets_payload(workspace, config_path)
    result = helper_code_sync_apply(
        session,
        {
            "protocol_version": 2,
            "mapping_profile": MAPPING_PROFILE,
            "targets": targets_payload,
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


def _build_targets_payload(workspace: Path, config_path: Path) -> list[dict]:
    _config, targets = build_targets(workspace, config_path)
    return [target.apply_payload() for target in targets]
