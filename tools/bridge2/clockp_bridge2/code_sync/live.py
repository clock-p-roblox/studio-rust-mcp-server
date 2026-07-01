from __future__ import annotations

from pathlib import Path

from ..errors import BridgeError
from ..session import Session
from ..studio import code_sync_get_manifest as helper_code_sync_get_manifest
from ..studio import mode
from .config import MAPPING_PROFILE, load_config


def query_live_manifest(session: Session, workspace: Path, config_path: Path) -> dict:
    before_mode = _require_stable_edit_mode(mode(session))
    config = load_config(config_path)
    roots = [
        {
            "root_id": root.root_id,
            "studio_path": root.studio_path,
        }
        for root in config.roots
    ]
    result = helper_code_sync_get_manifest(
        session,
        {
            "protocol_version": 2,
            "mapping_profile": MAPPING_PROFILE,
            "roots": roots,
        },
    )
    after_mode = _require_stable_edit_mode(mode(session))
    if after_mode.get("mode_seq") != before_mode.get("mode_seq"):
        raise BridgeError("code_sync_state_stale", "Studio mode_seq changed during live code-sync manifest query", {"before_mode": before_mode, "after_mode": after_mode})
    return {
        "protocol_version": 2,
        "mapping_profile": MAPPING_PROFILE,
        "combined_hash": result.get("combined_hash"),
        "roots": result.get("roots", []),
        "mode": result.get("mode") or before_mode.get("mode"),
        "mode_seq": result.get("mode_seq") or before_mode.get("mode_seq"),
    }


def _require_stable_edit_mode(payload: dict) -> dict:
    if payload.get("ok") is not True or payload.get("available") is False:
        raise BridgeError("code_sync_not_in_edit", "code-sync requires a live Studio edit mode", {"mode": payload})
    if payload.get("mode") != "edit":
        raise BridgeError("code_sync_not_in_edit", "code-sync live manifest requires Studio edit mode", {"mode": payload})
    mode_seq = payload.get("mode_seq")
    if not isinstance(mode_seq, int) or mode_seq <= 0:
        raise BridgeError("code_sync_remote_query_failed", "Studio edit mode query did not include a valid mode_seq", {"mode": payload})
    return payload
