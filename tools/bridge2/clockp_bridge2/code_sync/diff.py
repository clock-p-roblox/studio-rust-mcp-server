from __future__ import annotations


def diff_manifests(local: dict, live: dict) -> dict:
    local_roots = {root["root_id"]: root for root in local.get("roots", []) if isinstance(root, dict)}
    live_roots = {root["root_id"]: root for root in live.get("roots", []) if isinstance(root, dict)}
    mismatches = []
    for root_id, local_root in sorted(local_roots.items(), key=lambda item: str(item[0]).encode("utf-8")):
        live_root = live_roots.get(root_id)
        if live_root is None or not live_root.get("exists"):
            mismatches.append({"root_id": root_id, "kind": "missing_remote_root", "local_root_hash": local_root.get("root_hash"), "live_root_hash": None})
            continue
        local_hash = local_root.get("root_hash")
        live_hash = live_root.get("root_hash")
        if local_hash != live_hash:
            mismatches.append({"root_id": root_id, "kind": "root_hash_mismatch", "local_root_hash": local_hash, "live_root_hash": live_hash})
    for root_id, live_root in sorted(live_roots.items(), key=lambda item: str(item[0]).encode("utf-8")):
        if root_id not in local_roots:
            mismatches.append({"root_id": root_id, "kind": "unexpected_live_root", "live_root_hash": live_root.get("root_hash")})
    local_combined = local.get("combined_hash")
    live_combined = live.get("combined_hash")
    return {
        "matched": local_combined == live_combined and not mismatches,
        "local_combined_hash": local_combined,
        "live_combined_hash": live_combined,
        "mismatch_count": len(mismatches),
        "mismatches": mismatches[:50],
    }
