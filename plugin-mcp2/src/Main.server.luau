local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local StudioTestService = game:GetService("StudioTestService")

local LOG_PREFIX = "[MCP2 Plugin]"

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

local modeSeq = DateTime.now().UnixTimestampMillis

local function nextModeSeq()
	local now = DateTime.now().UnixTimestampMillis
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

		print(LOG_PREFIX, "executing studio_play", "command_id=" .. tostring(command.command_id))
		local ok, err = pcall(function()
			StudioTestService:ExecutePlayModeAsync({})
		end)
		if not ok then
			bumpModeSeq("studio_play failed: " .. tostring(err))
			return false, nil, "studio_play failed: " .. tostring(err)
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

		print(LOG_PREFIX, "executing studio_stop", "command_id=" .. tostring(command.command_id))
		local ok, err = pcall(function()
			StudioTestService:EndTest({})
		end)
		if not ok then
			bumpModeSeq("studio_stop failed: " .. tostring(err))
			return false, nil, "studio_stop failed: " .. tostring(err)
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

local function postResponseResultUntilAcknowledged(commandId, ok, result, errorMessage)
	while true do
		local responseOk, response = pcall(function()
			return postResponseResult(commandId, ok, result, errorMessage)
		end)
		if not responseOk then
			warn(LOG_PREFIX, "failed to post command result", tostring(response))
			task.wait(5)
		elseif not response.Success then
			warn(LOG_PREFIX, "command result not acknowledged", tostring(response.StatusCode), tostring(response.Body))
			task.wait(5)
		else
			print(LOG_PREFIX, "command result acknowledged", "command_id=" .. tostring(commandId))
			return
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
		stopHeartbeat()
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
		postResponseResultUntilAcknowledged(commandId, ok, result, errorMessage)
		if command.kind == "debug_stop_loop" then
			print(LOG_PREFIX, "debug_stop_loop acknowledged; stopping command loop")
			return
		end

		if command.kind == "noop" then
			task.wait(1)
		end
	end
end)
