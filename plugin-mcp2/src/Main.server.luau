local HttpService = game:GetService("HttpService")
local EncodingService = game:GetService("EncodingService")
local RunService = game:GetService("RunService")
local ServerScriptService = game:GetService("ServerScriptService")
local StudioTestService = game:GetService("StudioTestService")

local LOG_PREFIX = "[MCP2 Plugin]"
local RUNTIME_SCRIPT_NAME = "MCP2Runtime"
local RUNTIME_SCRIPT_MARKER = "clockp_mcp2_runtime_v1"
local MAX_SAFE_INTEGER = 9007199254740991

local function readRunServiceFlag(reader)
	local ok, value = pcall(reader)
	if ok then
		return value
	end

	return "error: " .. tostring(value)
end

local isEdit = readRunServiceFlag(function()
	return RunService:IsEdit()
end)
local isRunning = readRunServiceFlag(function()
	return RunService:IsRunning()
end)
local isServer = readRunServiceFlag(function()
	return RunService:IsServer()
end)
local isClient = readRunServiceFlag(function()
	return RunService:IsClient()
end)
local isRunMode = readRunServiceFlag(function()
	return RunService:IsRunMode()
end)
local isStudio = readRunServiceFlag(function()
	return RunService:IsStudio()
end)

local mode = "unknown"
if isStudio == true then
	if isEdit == true and isRunning == false then
		mode = "edit"
	elseif isEdit == false and isRunning == true and isServer == true and isClient == false then
		mode = "play_server"
	elseif isEdit == false and isRunning == true and isServer == false and isClient == true then
		mode = "play_client"
	end
end

local modeSeq = DateTime.now().UnixTimestampMillis * 1000 + Random.new():NextInteger(0, 999)

local function nextModeSeq()
	local now = DateTime.now().UnixTimestampMillis * 1000 + Random.new():NextInteger(0, 999)
	if now <= modeSeq then
		return modeSeq + 1
	end

	return now
end

local function bumpModeSeq(reason)
	modeSeq = nextModeSeq()
	warn(LOG_PREFIX, "mode_seq bumped", "mode=" .. mode, "mode_seq=" .. tostring(modeSeq), "reason=" .. tostring(reason))
end

local function isPositiveSafeInteger(value)
	return typeof(value) == "number" and value > 0 and value <= MAX_SAFE_INTEGER and value % 1 == 0
end

local function isJSONObjectTable(value)
	if typeof(value) ~= "table" then
		return false
	end
	for key, _ in pairs(value) do
		if typeof(key) ~= "string" then
			return false
		end
	end
	return true
end

local function validatePlayArgs(value)
	if typeof(value) ~= "table" then
		return nil, "invalid_play_data: play_args must be a table"
	end
	local launchId = value.launch_id
	if not isPositiveSafeInteger(launchId) then
		return nil, "invalid_play_data: play_args.launch_id must be a positive safe integer"
	end
	local data = value.data
	if not isJSONObjectTable(data) then
		return nil, "invalid_play_data: play_args.data must be an object table"
	end
	return {
		launch_id = launchId,
		data = data,
	}, nil
end

local function getCurrentPlayLaunchId()
	if mode ~= "play_server" then
		return nil
	end
	local ok, testArgs = pcall(function()
		return StudioTestService:GetTestArgs()
	end)
	if not ok or typeof(testArgs) ~= "table" then
		return nil
	end
	local launchId = testArgs.launch_id
	if isPositiveSafeInteger(launchId) then
		return launchId
	end
	return nil
end

print(LOG_PREFIX, "mode=" .. mode)
print(LOG_PREFIX, "mode_seq=" .. tostring(modeSeq))

print(
	LOG_PREFIX,
	"RunService flags",
	"mode=" .. mode,
	"IsEdit=" .. tostring(isEdit),
	"IsRunning=" .. tostring(isRunning),
	"IsServer=" .. tostring(isServer),
	"IsClient=" .. tostring(isClient),
	"IsRunMode=" .. tostring(isRunMode),
	"IsStudio=" .. tostring(isStudio)
)

if mode == "play_client" then
	return
end

if mode ~= "edit" and mode ~= "play_server" then
	return
end

local HELPER_BASE_URL = "http://127.0.0.1:44750"
local RESPONSE_RESULT_URL = HELPER_BASE_URL .. "/plugin/mcp2/response_result"

local function isManagedRuntimeScript(instance)
	if instance.Name ~= RUNTIME_SCRIPT_NAME or not instance:IsA("Script") then
		return false
	end
	if instance:GetAttribute(RUNTIME_SCRIPT_MARKER) == true then
		return true
	end

	local ok, source = pcall(function()
		return instance.Source
	end)
	return ok and typeof(source) == "string" and string.find(source, RUNTIME_SCRIPT_MARKER, 1, true) ~= nil
end

local function removeManagedRuntimeScript()
	for _, child in ServerScriptService:GetChildren() do
		if isManagedRuntimeScript(child) then
			child:Destroy()
		end
	end
end

local function installPlayServerRuntimeScript()
	if mode ~= "edit" then
		return
	end

	local ok, source = pcall(function()
		return script.Source
	end)
	if not ok or typeof(source) ~= "string" or source == "" then
		error("unable to read mcp2 plugin source for play_server runtime: " .. tostring(source))
	end

	removeManagedRuntimeScript()
	local runtimeScript = Instance.new("Script")
	runtimeScript.Name = RUNTIME_SCRIPT_NAME
	runtimeScript.Source = source
	runtimeScript:SetAttribute(RUNTIME_SCRIPT_MARKER, true)
	runtimeScript.Parent = ServerScriptService
	print(LOG_PREFIX, "play_server runtime script installed", "name=" .. RUNTIME_SCRIPT_NAME)
end

local function pullCommandURL(onlyPing)
	local url = HELPER_BASE_URL
		.. "/plugin/mcp2/pull_command?mode="
		.. HttpService:UrlEncode(mode)
		.. "&mode_seq="
		.. HttpService:UrlEncode(tostring(modeSeq))
	if onlyPing then
		url ..= "&only_ping=1"
	end

	return url
end

local function requestJSON(options)
	local ok, response = pcall(function()
		return HttpService:RequestAsync(options)
	end)
	if not ok then
		return nil, tostring(response)
	end
	if not response.Success then
		return nil, tostring(response.StatusCode) .. " " .. tostring(response.Body)
	end

	local decodedOk, decoded = pcall(function()
		return HttpService:JSONDecode(response.Body)
	end)
	if not decodedOk then
		return nil, tostring(decoded)
	end

	return decoded, nil
end

local function pullCommand()
	return requestJSON({
		Url = pullCommandURL(false),
		Method = "GET",
		Compress = Enum.HttpCompression.None,
	})
end

local function ping()
	return requestJSON({
		Url = pullCommandURL(true),
		Method = "GET",
		Compress = Enum.HttpCompression.None,
	})
end

local function startCommandHeartbeat()
	local stopped = false
	task.spawn(function()
		while not stopped do
			task.wait(15)
			if stopped then
				return
			end

			local response, err = ping()
			if response == nil then
				warn(LOG_PREFIX, "command heartbeat failed", err)
			elseif response.type ~= "pong" then
				warn(LOG_PREFIX, "unexpected command heartbeat response", tostring(response.type))
			end
		end
	end)

	return function()
		stopped = true
	end
end

local FORBIDDEN_RUN_CODE_TOKENS = {
	"StudioTestService",
	"ExecutePlayModeAsync",
	"ExecuteRunModeAsync",
	"EndTest",
}

local function serializeRunCodeValue(value)
	if typeof(value) == "table" then
		local ok, encoded = pcall(function()
			return HttpService:JSONEncode(value)
		end)
		if ok then
			return encoded
		end
	end
	return tostring(value)
end

local function serializeRunCodeValues(...)
	local packed = table.pack(...)
	local values = table.create(packed.n)
	for index = 1, packed.n do
		values[index] = serializeRunCodeValue(packed[index])
	end
	return values
end

local function rejectForbiddenRunCodeTokens(code)
	for _, token in FORBIDDEN_RUN_CODE_TOKENS do
		if string.find(code, token, 1, true) ~= nil then
			return "run code cannot call Studio lifecycle control API token: " .. token
		end
	end
	return nil
end

local function runStudioCode(code)
	if mode ~= "edit" then
		return false, nil, "studio_run_code requires edit mode; current mode=" .. mode
	end
	if typeof(code) ~= "string" or code == "" then
		return false, nil, "studio_run_code requires non-empty code"
	end
	local forbiddenReason = rejectForbiddenRunCodeTokens(code)
	if forbiddenReason ~= nil then
		return false, nil, forbiddenReason
	end

	local chunk, compileError = loadstring(code)
	if chunk == nil then
		return true, {
			kind = "studio_run_code",
			success = false,
			stage = "compile",
			prints = {},
			warnings = {},
			returns = {},
			error = tostring(compileError),
		}, nil
	end

	local prints = {}
	local warnings = {}
	local chunkEnv = getfenv(chunk)
	local oldPrint = chunkEnv.print or print
	local oldWarn = chunkEnv.warn or warn
	chunkEnv.print = function(...)
		oldPrint(...)
		table.insert(prints, serializeRunCodeValues(...))
	end
	chunkEnv.warn = function(...)
		oldWarn(...)
		table.insert(warnings, serializeRunCodeValues(...))
	end

	local packedResults
	local ok, runtimeError = pcall(function()
		packedResults = table.pack(chunk())
	end)
	chunkEnv.print = oldPrint
	chunkEnv.warn = oldWarn

	if not ok then
		return true, {
			kind = "studio_run_code",
			success = false,
			stage = "runtime",
			prints = prints,
			warnings = warnings,
			returns = {},
			error = tostring(runtimeError),
		}, nil
	end

	local returns = {}
	for index = 1, packedResults.n do
		returns[index] = serializeRunCodeValue(packedResults[index])
	end
	return true, {
		kind = "studio_run_code",
		success = true,
		stage = "complete",
		prints = prints,
		warnings = warnings,
		returns = returns,
	}, nil
end

local function codeSyncS(value)
	local text = tostring(value)
	return tostring(#text) .. ":" .. text
end

local function codeSyncBytesToHex(value)
	local parts = table.create(#value)
	for index = 1, #value do
		parts[index] = string.format("%02x", string.byte(value, index))
	end
	return table.concat(parts)
end

local function codeSyncBlake3Hex(value)
	return codeSyncBytesToHex(EncodingService:ComputeStringHash(value, Enum.HashAlgorithm.Blake3))
end

local function codeSyncKind(instance)
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

local function codeSyncSortChildren(left, right)
	if left.name == right.name then
		return left.entry_kind < right.entry_kind
	end
	return left.name < right.name
end

local function codeSyncSourceOf(instance)
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

local function codeSyncChildSummaryBytes(child)
	return codeSyncS(child.name) .. codeSyncS(child.entry_kind) .. codeSyncS(child.entry_hash)
end

local function codeSyncNodeHash(kind, name, source, children)
	table.sort(children, codeSyncSortChildren)
	local encodedChildren = table.create(#children)
	for index, child in children do
		encodedChildren[index] = codeSyncChildSummaryBytes(child)
	end
	if kind == "Folder" then
		return codeSyncBlake3Hex(codeSyncS("clockp.code_sync.v1.folder") .. codeSyncS(name) .. codeSyncS(#children) .. table.concat(encodedChildren))
	end
	if kind == "ModuleScript" or kind == "Script" or kind == "LocalScript" then
		return codeSyncBlake3Hex(codeSyncS("clockp.code_sync.v1.script") .. codeSyncS(kind) .. codeSyncS(name) .. codeSyncS(source or "") .. codeSyncS(#children) .. table.concat(encodedChildren))
	end
	error("code_sync_unsupported_mapping: unsupported kind " .. tostring(kind))
end

local function codeSyncFindPath(path)
	local current = game:GetService(path[1])
	for index = 2, #path do
		current = current:FindFirstChild(path[index])
		if current == nil then
			return nil
		end
	end
	return current
end

local function codeSyncSummarize(instance)
	if instance == nil then
		return nil
	end
	local kind = codeSyncKind(instance)
	if kind == nil then
		return nil
	end
	local seen = {}
	local children = {}
	for _, child in instance:GetChildren() do
		local childEntryKind = codeSyncKind(child)
		if childEntryKind ~= nil then
			if seen[child.Name] then
				error("code_sync_ambiguous_remote_tree: duplicate child name " .. child.Name)
			end
			seen[child.Name] = true
			local childSummary = codeSyncSummarize(child)
			table.insert(children, {
				name = child.Name,
				entry_kind = childEntryKind,
				entry_hash = childSummary.entry_hash,
				children_count = childSummary.children_count,
			})
		end
	end
	table.sort(children, codeSyncSortChildren)
	local entryHash = codeSyncNodeHash(kind, instance.Name, codeSyncSourceOf(instance), children)
	return {
		name = instance.Name,
		root_kind = kind,
		entry_kind = kind,
		entry_hash = entryHash,
		children_count = #children,
		children_complete = true,
		children = children,
		source = codeSyncSourceOf(instance),
	}
end

local function codeSyncRootHash(rootId, mappingProfile, summary)
	return codeSyncBlake3Hex(codeSyncS("clockp.code_sync.v1.root") .. codeSyncS(rootId) .. codeSyncS(mappingProfile) .. codeSyncS(summary.root_kind) .. codeSyncS(summary.entry_hash))
end

local function codeSyncCombinedHash(mappingProfile, roots)
	table.sort(roots, function(left, right)
		return left.root_id < right.root_id
	end)
	local encoded = table.create(#roots)
	for index, root in roots do
		encoded[index] = codeSyncS(root.root_id) .. codeSyncS(root.root_hash)
	end
	return codeSyncBlake3Hex(codeSyncS("clockp.code_sync.v1.combined") .. codeSyncS(mappingProfile) .. codeSyncS(#roots) .. table.concat(encoded))
end

local function codeSyncGetManifest(args)
	if mode ~= "edit" then
		return false, nil, "code_sync_not_in_edit: current mode=" .. mode
	end
	if typeof(args) ~= "table" then
		return false, nil, "code_sync_invalid_config: args must be a table"
	end
	if args.protocol_version ~= 1 then
		return false, nil, "code_sync_protocol_version_unsupported"
	end
	if args.mapping_profile ~= "code_sync_lua_v1" then
		return false, nil, "code_sync_unsupported_mapping"
	end
	local result = {
		kind = "code_sync_get_manifest",
		protocol_version = 1,
		project_id = args.project_id,
		mapping_profile = args.mapping_profile,
		mode = mode,
		mode_seq = modeSeq,
		roots = {},
	}
	local combinedRoots = {}
	for _, root in args.roots do
		local summary = codeSyncSummarize(codeSyncFindPath(root.studio_path))
		local currentRootHash = nil
		if summary ~= nil then
			currentRootHash = codeSyncRootHash(root.root_id, args.mapping_profile, summary)
			table.insert(combinedRoots, { root_id = root.root_id, root_hash = currentRootHash })
		end
		table.insert(result.roots, {
			root_id = root.root_id,
			studio_path = root.studio_path,
			exists = summary ~= nil,
			root_hash = currentRootHash,
			summary = summary,
		})
	end
	result.combined_hash = codeSyncCombinedHash(args.mapping_profile, combinedRoots)
	return true, result, nil
end

local function codeSyncClassForKind(kind)
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

local function codeSyncEnsureChild(parent, name, className)
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

local function codeSyncEnsureRoot(path, tree)
	local current = game:GetService(path[1])
	for index = 2, #path do
		local className = if index == #path then codeSyncClassForKind(tree.kind) else "Folder"
		current = codeSyncEnsureChild(current, path[index], className)
	end
	return current
end

local function codeSyncWriteNode(instance, node)
	if node.kind == "ModuleScript" or node.kind == "Script" or node.kind == "LocalScript" then
		instance.Source = node.source or ""
	end
	instance:ClearAllChildren()
	for _, child in node.children do
		local childInstance = Instance.new(codeSyncClassForKind(child.kind))
		childInstance.Name = child.name
		childInstance.Parent = instance
		codeSyncWriteNode(childInstance, child)
	end
end

local function codeSyncApply(args)
	if mode ~= "edit" then
		return false, nil, "code_sync_not_in_edit: current mode=" .. mode
	end
	if typeof(args) ~= "table" then
		return false, nil, "code_sync_invalid_config: args must be a table"
	end
	if args.protocol_version ~= 1 then
		return false, nil, "code_sync_protocol_version_unsupported"
	end
	if args.mapping_profile ~= "code_sync_lua_v1" then
		return false, nil, "code_sync_unsupported_mapping"
	end
	local applied = {}
	for _, root in args.roots do
		local rootInstance = codeSyncEnsureRoot(root.studio_path, root.tree)
		codeSyncWriteNode(rootInstance, root.tree)
		table.insert(applied, root.root_id)
	end
	return true, {
		kind = "code_sync_apply",
		protocol_version = 1,
		project_id = args.project_id,
		mapping_profile = args.mapping_profile,
		mode = mode,
		mode_seq = modeSeq,
		applied_roots = applied,
	}, nil
end

local function processCommand(command)
	local kind = tostring(command.kind or "")
	if kind == "noop" then
		return true, {
			kind = kind,
		}, nil
	end
	if kind == "debug_echo" then
		local args = command.args
		if typeof(args) == "table" and typeof(args.sleep_ms) == "number" and args.sleep_ms > 0 then
			task.wait(args.sleep_ms / 1000)
		end
		return true, {
			kind = kind,
			args = command.args,
		}, nil
	end
	if kind == "debug_stop_loop" then
		return true, {
			kind = kind,
		}, nil
	end
	if kind == "studio_mode_query" then
		local result = {
			kind = kind,
			available = true,
			mode = mode,
			mode_seq = modeSeq,
			run_service = {
				IsEdit = isEdit,
				IsRunning = isRunning,
				IsServer = isServer,
				IsClient = isClient,
				IsRunMode = isRunMode,
				IsStudio = isStudio,
			},
		}
		local launchId = getCurrentPlayLaunchId()
		if launchId ~= nil then
			result.launch_id = launchId
		end
		return true, result, nil
	end
	if kind == "studio_run_code" then
		local args = command.args
		local code = if typeof(args) == "table" then args.code else nil
		return runStudioCode(code)
	end
	if kind == "code_sync_get_manifest" then
		return codeSyncGetManifest(command.args)
	end
	if kind == "code_sync_apply" then
		return codeSyncApply(command.args)
	end
	if kind == "studio_play" then
		if mode == "play_server" then
			return false, nil, "already_playing: Studio is already in play mode"
		end
		if mode ~= "edit" then
			return false, nil, "studio_play requires edit mode; current mode=" .. mode
		end

		local args = command.args
		local requestedMode = if typeof(args) == "table" and typeof(args.mode) == "string" then args.mode else "start_play"
		if requestedMode ~= "start_play" then
			return false, nil, "studio_play only supports start_play; requested mode=" .. tostring(requestedMode)
		end
		local playArgs, playArgsError = validatePlayArgs(if typeof(args) == "table" then args.play_args else nil)
		if playArgs == nil then
			return false, nil, playArgsError
		end

		local runtimeOk, runtimeErr = pcall(installPlayServerRuntimeScript)
		if not runtimeOk then
			return false, nil, "studio_play failed to install play_server runtime: " .. tostring(runtimeErr)
		end

		return true, {
			kind = kind,
			mode = requestedMode,
			status = "play_requested",
			launch_id = playArgs.launch_id,
		}, nil
	end
	if kind == "studio_stop" then
		if mode == "edit" then
			return true, {
				kind = kind,
				status = "already_stopped",
			}, nil
		end
		if mode ~= "play_server" then
			return false, nil, "studio_stop requires play_server mode; current mode=" .. mode
		end

		return true, {
			kind = kind,
			status = "stop_requested",
		}, nil
	end

	return false, nil, "unknown command kind: " .. kind
end

local function postResponseResult(commandId, ok, result, errorMessage)
	local body = HttpService:JSONEncode({
		command_id = commandId,
		mode = mode,
		mode_seq = modeSeq,
		ok = ok,
		result = result,
		error = errorMessage,
	})

	return HttpService:RequestAsync({
		Url = RESPONSE_RESULT_URL,
		Method = "POST",
		Headers = {
			["Content-Type"] = "application/json",
		},
		Body = body,
		Compress = Enum.HttpCompression.None,
	})
end

local function postResponseResultUntilRecorded(commandId, ok, result, errorMessage)
	while true do
		local responseOk, response = pcall(function()
			return postResponseResult(commandId, ok, result, errorMessage)
		end)
		if not responseOk then
			warn(LOG_PREFIX, "failed to post command result", tostring(response))
			task.wait(5)
		elseif not response.Success then
			warn(LOG_PREFIX, "command result not recorded", tostring(response.StatusCode), tostring(response.Body))
			task.wait(5)
		else
			local decodedOk, decoded = pcall(function()
				return HttpService:JSONDecode(response.Body)
			end)
			if decodedOk and typeof(decoded) == "table" and decoded.recorded == true then
				print(LOG_PREFIX, "command result recorded", "command_id=" .. tostring(commandId))
				return true
			end
			local reason = if decodedOk and typeof(decoded) == "table" then tostring(decoded.reason or "") else "invalid_response_body"
			warn(LOG_PREFIX, "command result ignored by helper", "command_id=" .. tostring(commandId), "reason=" .. reason)
			if reason == "invalid_lifecycle_source"
				or reason == "studio_pid_mismatch"
				or reason == "mode_seq_mismatch"
				or reason == "not_waiting_for_command"
			then
				bumpModeSeq("command result ignored by helper: " .. reason)
				return false
			end
			task.wait(5)
		end
	end
end

local function parsePostResponse(response)
	if not response.Success then
		return false, false, tostring(response.StatusCode) .. " " .. tostring(response.Body)
	end

	local decodedOk, decoded = pcall(function()
		return HttpService:JSONDecode(response.Body)
	end)
	if not decodedOk or typeof(decoded) ~= "table" then
		return true, false, "invalid_response_body"
	end

	return true, decoded.recorded == true, tostring(decoded.reason or "")
end

local function postResponseResultBounded(commandId, ok, result, errorMessage, timeoutSeconds)
	local finished = false
	local delivered = false
	local recorded = false
	local detail = "timeout"

	task.spawn(function()
		local responseOk, response = pcall(function()
			return postResponseResult(commandId, ok, result, errorMessage)
		end)
		if not responseOk then
			detail = tostring(response)
		else
			delivered, recorded, detail = parsePostResponse(response)
		end
		finished = true
	end)

	local deadline = os.clock() + timeoutSeconds
	while not finished and os.clock() < deadline do
		task.wait(0.05)
	end

	if not finished then
		warn(LOG_PREFIX, "command result delivery timed out", "command_id=" .. tostring(commandId))
		return false, false, "timeout"
	end
	if delivered and recorded then
		print(LOG_PREFIX, "command result recorded", "command_id=" .. tostring(commandId))
	elseif delivered then
		warn(LOG_PREFIX, "command result delivery was accepted but not recorded", "command_id=" .. tostring(commandId), "reason=" .. detail)
	else
		warn(LOG_PREFIX, "command result delivery failed", "command_id=" .. tostring(commandId), "reason=" .. detail)
	end

	return delivered, recorded, detail
end

local function executeStudioTransitionAfterAck(command, result)
	local kind = tostring(command.kind or "")
	local status = if typeof(result) == "table" then tostring(result.status or "") else ""
	if kind == "studio_play" and status == "play_requested" then
		print(LOG_PREFIX, "executing studio_play", "command_id=" .. tostring(command.command_id))
		local args = command.args
		local playArgs = if typeof(args) == "table" then args.play_args else nil
		local ok, err = pcall(function()
			StudioTestService:ExecutePlayModeAsync(playArgs)
		end)
		if not ok then
			bumpModeSeq("studio_play failed after pre-action ack: " .. tostring(err))
			warn(LOG_PREFIX, "studio_play failed after pre-action ack", tostring(err))
		end
	elseif kind == "studio_stop" and status == "stop_requested" then
		print(LOG_PREFIX, "executing studio_stop", "command_id=" .. tostring(command.command_id))
		local ok, err = pcall(function()
			StudioTestService:EndTest({})
		end)
		if not ok then
			bumpModeSeq("studio_stop failed after pre-action ack: " .. tostring(err))
			warn(LOG_PREFIX, "studio_stop failed after pre-action ack", tostring(err))
		end
	end
end

task.spawn(function()
	while true do
		local command, pullError = pullCommand()
		if command == nil then
			warn(LOG_PREFIX, "failed to pull command", pullError)
			task.wait(5)
			continue
		end

		local responseType = tostring(command.type or "")
		if responseType == "should_restart_pull" then
			if tostring(command.reason or "") == "invalid_lifecycle_source" then
				task.wait(5)
			end
			continue
		end

		if responseType ~= "command" then
			warn(LOG_PREFIX, "unknown pull response type", responseType)
			task.wait(1)
			continue
		end

		local commandId = command.command_id
		if typeof(commandId) ~= "number" then
			warn(LOG_PREFIX, "pulled command without numeric command_id")
			task.wait(1)
			continue
		end

		local stopHeartbeat = startCommandHeartbeat()
		local processOk, ok, result, errorMessage = pcall(processCommand, command)
		if not processOk then
			bumpModeSeq("command raised: " .. tostring(ok))
			errorMessage = "command raised: " .. tostring(ok)
			result = nil
			ok = false
		end
		if not ok then
			warn(
				LOG_PREFIX,
				"command failed",
				"command_id=" .. tostring(commandId),
				"kind=" .. tostring(command.kind),
				tostring(errorMessage)
			)
		end
		local shouldRequestStudioTransition = ok
			and (command.kind == "studio_play" or command.kind == "studio_stop")
			and typeof(result) == "table"
			and (result.status == "play_requested" or result.status == "stop_requested")
		local resultRecorded = false
		if shouldRequestStudioTransition then
			local _delivered, recorded, _detail = postResponseResultBounded(commandId, ok, result, errorMessage, 3)
			resultRecorded = recorded
		else
			resultRecorded = postResponseResultUntilRecorded(commandId, ok, result, errorMessage)
		end
		stopHeartbeat()
		if shouldRequestStudioTransition and resultRecorded then
			executeStudioTransitionAfterAck(command, result)
		elseif shouldRequestStudioTransition then
			warn(
				LOG_PREFIX,
				"studio transition skipped because response_result was not recorded",
				"command_id=" .. tostring(commandId),
				"kind=" .. tostring(command.kind)
			)
		end
		if command.kind == "debug_stop_loop" then
			print(LOG_PREFIX, "debug_stop_loop acknowledged; stopping command loop")
			return
		end

		if command.kind == "noop" then
			task.wait(1)
		end
	end
end)
