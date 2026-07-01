from __future__ import annotations


def diff_manifests(local: dict, live: dict) -> dict:
    local_targets = {str(target["target_id"]): target for target in local.get("targets", []) if isinstance(target, dict)}
    live_targets = {str(target["target_id"]): target for target in live.get("targets", []) if isinstance(target, dict)}
    mismatches = []
    for target_id, local_target in sorted(local_targets.items(), key=lambda item: str(item[0]).encode("utf-8")):
        live_target = live_targets.get(target_id)
        if live_target is None or not live_target.get("exists"):
            mismatches.append({"target_id": target_id, "kind": "missing_remote_target", "local_target_hash": local_target.get("target_hash"), "live_target_hash": None})
            continue
        local_hash = local_target.get("target_hash")
        live_hash = live_target.get("target_hash")
        if local_hash != live_hash:
            mismatches.append({"target_id": target_id, "kind": "target_hash_mismatch", "local_target_hash": local_hash, "live_target_hash": live_hash})
    for target_id, live_target in sorted(live_targets.items(), key=lambda item: str(item[0]).encode("utf-8")):
        if target_id not in local_targets:
            mismatches.append({"target_id": target_id, "kind": "unexpected_live_target", "live_target_hash": live_target.get("target_hash")})
    local_combined = local.get("combined_hash")
    live_combined = live.get("combined_hash")
    return {
        "matched": local_combined == live_combined and not mismatches,
        "local_combined_hash": local_combined,
        "live_combined_hash": live_combined,
        "mismatch_count": len(mismatches),
        "mismatches": mismatches[:50],
    }
