local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local ServerScriptService = game:GetService("ServerScriptService")
local StudioTestService = game:GetService("StudioTestService")

local LOG_PREFIX = "[MCP2 Plugin]"
local RUNTIME_SCRIPT_NAME = "MCP2Runtime"
local RUNTIME_SCRIPT_MARKER = "clockp_mcp2_runtime_v1"

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
		return true, {
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
		}, nil
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

		local runtimeOk, runtimeErr = pcall(installPlayServerRuntimeScript)
		if not runtimeOk then
			return false, nil, "studio_play failed to install play_server runtime: " .. tostring(runtimeErr)
		end

		return true, {
			kind = kind,
			mode = requestedMode,
			status = "play_requested",
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
		warn(LOG_PREFIX, "command result delivery timed out; Studio transition will still be requested", "command_id=" .. tostring(commandId))
		return false, false, "timeout"
	end
	if delivered and recorded then
		print(LOG_PREFIX, "command result recorded", "command_id=" .. tostring(commandId))
	elseif delivered then
		warn(LOG_PREFIX, "command result delivery was accepted but not recorded; Studio transition will still be requested", "command_id=" .. tostring(commandId), "reason=" .. detail)
	else
		warn(LOG_PREFIX, "command result delivery failed; Studio transition will still be requested", "command_id=" .. tostring(commandId), "reason=" .. detail)
	end

	return delivered, recorded, detail
end

local function executeStudioTransitionAfterAck(command, result)
	local kind = tostring(command.kind or "")
	local status = if typeof(result) == "table" then tostring(result.status or "") else ""
	if kind == "studio_play" and status == "play_requested" then
		print(LOG_PREFIX, "executing studio_play", "command_id=" .. tostring(command.command_id))
		local ok, err = pcall(function()
			StudioTestService:ExecutePlayModeAsync({})
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
		if shouldRequestStudioTransition then
			executeStudioTransitionAfterAck(command, result)
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
