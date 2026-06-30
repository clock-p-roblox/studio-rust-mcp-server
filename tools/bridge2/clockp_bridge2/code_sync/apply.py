from __future__ import annotations

import json
from pathlib import Path

from ..errors import BridgeError
from ..session import Session
from ..studio import code_sync_apply as helper_code_sync_apply
from ..studio import mode, run_code_direct
from .config import load_config
from .diff import diff_manifests
from .live import _require_stable_edit_mode, query_live_manifest
from .luau import long_string_literal
from .manifest import build_local_manifest
from .mapping import build_logical_tree
from .rojo_project import load_project_targets
from .scanner import scan_root


def apply_code_sync(session: Session, workspace: Path, config_path: Path, rojo_project_path: Path, *, legacy_run_code: bool = False) -> dict:
    before_mode = _require_stable_edit_mode(mode(session))
    local_manifest = build_local_manifest(workspace, config_path, rojo_project_path)
    roots_payload = _build_roots_payload(workspace, config_path, rojo_project_path)
    if legacy_run_code:
        code = _apply_luau(roots_payload)
        result = run_code_direct(session, code)
        _ensure_run_code_success(result)
    else:
        result = helper_code_sync_apply(
            session,
            {
                "protocol_version": 1,
                "project_id": local_manifest["project_id"],
                "mapping_profile": local_manifest["mapping_profile"],
                "roots": roots_payload,
            },
        )
    after_mode = _require_stable_edit_mode(mode(session))
    if before_mode.get("mode_seq") != after_mode.get("mode_seq"):
        raise BridgeError("code_sync_state_stale", "Studio mode_seq changed during apply", {"before_mode": before_mode, "after_mode": after_mode})
    live_manifest = query_live_manifest(session, workspace, config_path, rojo_project_path)
    diff = diff_manifests(local_manifest, live_manifest)
    if not diff["matched"]:
        raise BridgeError("code_sync_verify_failed", "live Studio manifest hash does not match local manifest after apply", {"local": local_manifest, "live": live_manifest, "diff": diff})
    return {"applied": True, "local": local_manifest, "live": live_manifest, "diff": diff, "apply_result": result}


def _build_roots_payload(workspace: Path, config_path: Path, rojo_project_path: Path) -> list[dict]:
    targets = load_project_targets(rojo_project_path)
    config = load_config(config_path, targets)
    roots = []
    for root in config.roots:
        files = scan_root(workspace, root)
        logical = build_logical_tree(root.studio_path[-1], files)
        roots.append({"root_id": root.root_id, "studio_path": root.studio_path, "tree": logical.to_payload()})
    return roots


def _ensure_run_code_success(result: dict) -> None:
    command_result = result.get("command_result")
    if isinstance(command_result, dict):
        inner = command_result.get("result")
        if isinstance(inner, dict) and inner.get("success") is False:
            raise BridgeError("code_sync_apply_failed", str(inner.get("error") or "Studio apply code failed"), {"result": result})
    nested = result.get("result")
    if isinstance(nested, dict) and nested.get("success") is False:
        raise BridgeError("code_sync_apply_failed", str(nested.get("error") or "Studio apply code failed"), {"result": result})
    if result.get("ok") is False:
        raise BridgeError("code_sync_apply_failed", str(result.get("message") or result.get("error") or "apply failed"), {"result": result})


def _apply_luau(roots: list[dict]) -> str:
    roots_json = json.dumps(roots, ensure_ascii=False, separators=(",", ":"))
    return f"""
local HttpService = game:GetService("HttpService")
local roots = HttpService:JSONDecode({long_string_literal(roots_json)})

local function classForKind(kind)
    if kind == "Folder" then
        return "Folder"
    elseif kind == "ModuleScript" then
        return "ModuleScript"
    elseif kind == "Script" then
        return "Script"
    elseif kind == "LocalScript" then
        return "LocalScript"
    end
    error("code_sync_unsupported_mapping: unsupported kind " .. tostring(kind))
end

local function ensureChild(parent, name, className)
    local existing = parent:FindFirstChild(name)
    if existing ~= nil and existing.ClassName ~= className then
        existing:Destroy()
        existing = nil
    end
    if existing == nil then
        existing = Instance.new(className)
        existing.Name = name
        existing.Parent = parent
    end
    return existing
end

local function ensureRoot(path, tree)
    local current = game:GetService(path[1])
    for index = 2, #path do
        local className = if index == #path then classForKind(tree.kind) else "Folder"
        current = ensureChild(current, path[index], className)
    end
    return current
end

local function writeNode(instance, node)
    if node.kind == "ModuleScript" or node.kind == "Script" or node.kind == "LocalScript" then
        instance.Source = node.source or ""
    end
    instance:ClearAllChildren()
    for _, child in node.children do
        local childInstance = Instance.new(classForKind(child.kind))
        childInstance.Name = child.name
        childInstance.Parent = instance
        writeNode(childInstance, child)
    end
end

local applied = {{}}
for _, root in roots do
    local rootInstance = ensureRoot(root.studio_path, root.tree)
    writeNode(rootInstance, root.tree)
    table.insert(applied, root.root_id)
end
return HttpService:JSONEncode({{ applied_roots = applied }})
"""
