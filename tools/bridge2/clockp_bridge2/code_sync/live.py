from __future__ import annotations

import json
from pathlib import Path

from ..errors import BridgeError
from ..studio import mode, run_code_direct
from ..studio import code_sync_get_manifest as helper_code_sync_get_manifest
from ..session import Session
from .config import load_config
from .luau import long_string_literal
from .project import load_project_targets


def query_live_manifest(session: Session, workspace: Path, config_path: Path, project_path: Path, *, legacy_run_code: bool = False) -> dict:
    before_mode = _require_stable_edit_mode(mode(session))
    targets = load_project_targets(project_path)
    config = load_config(config_path, targets)
    roots = [
        {
            "root_id": root.root_id,
            "studio_path": root.studio_path,
        }
        for root in config.roots
    ]
    if not legacy_run_code:
        payload = {
            "protocol_version": 1,
            "project_id": config.project_id,
            "mapping_profile": config.mapping_profile,
            "roots": roots,
        }
        result = helper_code_sync_get_manifest(session, payload)
        after_mode = _require_stable_edit_mode(mode(session))
        if after_mode.get("mode_seq") != before_mode.get("mode_seq"):
            raise BridgeError("code_sync_state_stale", "Studio mode_seq changed during live code-sync manifest query", {"before_mode": before_mode, "after_mode": after_mode})
        return {
            "protocol_version": 1,
            "project_id": config.project_id,
            "mapping_profile": config.mapping_profile,
            "combined_hash": result.get("combined_hash"),
            "roots": result.get("roots", []),
            "mode": result.get("mode") or before_mode.get("mode"),
            "mode_seq": result.get("mode_seq") or before_mode.get("mode_seq"),
        }
    code = _live_manifest_luau(roots)
    result = run_code_direct(session, code)
    after_mode = _require_stable_edit_mode(mode(session))
    if after_mode.get("mode_seq") != before_mode.get("mode_seq"):
        raise BridgeError(
            "code_sync_state_stale",
            "Studio mode_seq changed during live code-sync manifest query",
            {"before_mode": before_mode, "after_mode": after_mode},
        )
    command_result = result.get("command_result")
    if isinstance(command_result, dict):
        result = command_result.get("result") or result
    nested_result = result.get("result") if isinstance(result, dict) else None
    if isinstance(nested_result, dict):
        result = nested_result
    returns = result.get("returns") if isinstance(result, dict) else None
    if not isinstance(returns, list) or not returns:
        raise BridgeError("code_sync_remote_query_failed", "live manifest command returned no JSON payload", {"result": result})
    try:
        live = json.loads(str(returns[0]))
    except json.JSONDecodeError as exc:
        raise BridgeError("code_sync_remote_query_failed", str(exc), {"return": returns[0]}) from exc
    return {
        "protocol_version": 1,
        "project_id": config.project_id,
        "mapping_profile": config.mapping_profile,
        "combined_hash": live.get("combined_hash"),
        "roots": live.get("roots", []),
        "mode": before_mode.get("mode"),
        "mode_seq": before_mode.get("mode_seq"),
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


def _live_manifest_luau(roots: list[dict]) -> str:
    roots_json = json.dumps(roots, ensure_ascii=False, separators=(",", ":"))
    return f"""
local HttpService = game:GetService("HttpService")
local EncodingService = game:GetService("EncodingService")
local roots = HttpService:JSONDecode({long_string_literal(roots_json)})
local mappingProfile = "code_sync_lua_v1"

local function S(value)
    local text = tostring(value)
    return tostring(#text) .. ":" .. text
end

local function bytesToHex(value)
    local parts = table.create(#value)
    for index = 1, #value do
        parts[index] = string.format("%02x", string.byte(value, index))
    end
    return table.concat(parts)
end

local function blake3Hex(value)
    return bytesToHex(EncodingService:ComputeStringHash(value, Enum.HashAlgorithm.Blake3))
end

local function childSummaryBytes(child)
    return S(child.name) .. S(child.entry_kind) .. S(child.entry_hash)
end

local function sortChildren(left, right)
    if left.name == right.name then
        return left.entry_kind < right.entry_kind
    end
    return left.name < right.name
end

local function childKind(instance)
    if instance:IsA("Folder") then
        return "Folder"
    elseif instance:IsA("ModuleScript") then
        return "ModuleScript"
    elseif instance:IsA("Script") then
        return "Script"
    elseif instance:IsA("LocalScript") then
        return "LocalScript"
    end
    return nil
end

local function findPath(path)
    local current = game:GetService(path[1])
    for index = 2, #path do
        current = current:FindFirstChild(path[index])
        if current == nil then
            return nil
        end
    end
    return current
end

local function sourceOf(instance)
    if not (instance:IsA("ModuleScript") or instance:IsA("Script") or instance:IsA("LocalScript")) then
        return nil
    end
    local ok, value = pcall(function()
        return instance.Source
    end)
    if ok then
        local cr = string.char(13)
        local lf = string.char(10)
        return string.gsub(string.gsub(value, cr .. lf, lf), cr, lf)
    end
    return nil
end

local function nodeHash(kind, name, source, children)
    table.sort(children, sortChildren)
    local encodedChildren = table.create(#children)
    for index, child in children do
        encodedChildren[index] = childSummaryBytes(child)
    end
    if kind == "Folder" then
        return blake3Hex(S("clockp.code_sync.v1.folder") .. S(name) .. S(#children) .. table.concat(encodedChildren))
    end
    if kind == "ModuleScript" or kind == "Script" or kind == "LocalScript" then
        return blake3Hex(S("clockp.code_sync.v1.script") .. S(kind) .. S(name) .. S(source or "") .. S(#children) .. table.concat(encodedChildren))
    end
    error("code_sync_unsupported_mapping: unsupported kind " .. tostring(kind))
end

local function summarize(instance)
    if instance == nil then
        return nil
    end
    local kind = childKind(instance)
    if kind == nil then
        return nil
    end
    local seen = {{}}
    local children = {{}}
    for _, child in instance:GetChildren() do
        local childEntryKind = childKind(child)
        if childEntryKind ~= nil then
            if seen[child.Name] then
                error("code_sync_ambiguous_remote_tree: duplicate child name " .. child.Name)
            end
            seen[child.Name] = true
            local childSummary = summarize(child)
            table.insert(children, {{
                name = child.Name,
                entry_kind = childEntryKind,
                entry_hash = childSummary.entry_hash,
                children_count = childSummary.children_count,
            }})
        end
    end
    table.sort(children, sortChildren)
    local entryHash = nodeHash(kind, instance.Name, sourceOf(instance), children)
    return {{
        name = instance.Name,
        root_kind = kind,
        entry_kind = kind,
        entry_hash = entryHash,
        children_count = #children,
        children_complete = true,
        children = children,
        source = sourceOf(instance),
    }}
end

local function rootHash(rootId, summary)
    return blake3Hex(S("clockp.code_sync.v1.root") .. S(rootId) .. S(mappingProfile) .. S(summary.root_kind) .. S(summary.entry_hash))
end

local function combinedHash(rootSummaries)
    table.sort(rootSummaries, function(left, right)
        return left.root_id < right.root_id
    end)
    local encoded = table.create(#rootSummaries)
    for index, root in rootSummaries do
        encoded[index] = S(root.root_id) .. S(root.root_hash)
    end
    return blake3Hex(S("clockp.code_sync.v1.combined") .. S(mappingProfile) .. S(#rootSummaries) .. table.concat(encoded))
end

local result = {{ roots = {{}} }}
local combinedRoots = {{}}
for _, root in roots do
    local summary = summarize(findPath(root.studio_path))
    local currentRootHash = nil
    if summary ~= nil then
        currentRootHash = rootHash(root.root_id, summary)
        table.insert(combinedRoots, {{ root_id = root.root_id, root_hash = currentRootHash }})
    end
    table.insert(result.roots, {{
        root_id = root.root_id,
        studio_path = root.studio_path,
        exists = summary ~= nil,
        root_hash = currentRootHash,
        summary = summary,
    }})
end
result.combined_hash = combinedHash(combinedRoots)
return HttpService:JSONEncode(result)
"""
