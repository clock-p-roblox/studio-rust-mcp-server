local Main = script:FindFirstAncestor("MCPStudioPlugin")
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local PluginUtils = require(Main.Utils.PluginUtils)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)
local Types = require(Main.Types)

local HttpService = game:GetService("HttpService")
local ServerScriptService = game:GetService("ServerScriptService")
local StudioTestService = game:GetService("StudioTestService")

local START_TIMEOUT_SECONDS = 60
local STOP_TIMEOUT_SECONDS = 60
local POLL_INTERVAL_SECONDS = 0.25
local HELPER_PORT_SETTING_KEY = "MCP-helper-port"
local DEFAULT_HELPER_PORT = "44750"
local LOG_PAGE_SIZE = 400
local MODE_LOG_PAGE_SIZE = 2_000
local SESSION_CONTROL_SCRIPT_NAME = "MCPStudioSessionControl"

type LogBaseline = {
	path: string,
	totalLines: number,
}

type ModeLogInference = {
	mode: Types.StartStopPlayMode,
	path: string,
	lineNumber: number,
}

type AsyncCallState = {
	done: boolean,
	success: boolean?,
	errorMessage: string?,
}

local STOP_MARKERS = {
	"State: StopPlaySolo",
	"StudioStopPlaySolo",
	"Handling test has ended",
	"State: PlaceIdle",
}

local START_PLAY_MARKERS = {
	"[runtime-launch]",
	"State: PlaySoloIdle",
	"PlaySoloPlayableTotalTime",
	"PlaySoloSuccess",
}

local lastModeInferencePath: string? = nil
local lastModeInferenceLineNumber = 0
local currentHelperBaseUrl: string? = nil
local currentInstanceId: string? = nil
local lastHelperStopRequestId = 0

local function configureHelperContext(helperUrl: string?, instanceId: string?)
	currentHelperBaseUrl = helperUrl
	currentInstanceId = instanceId
end

local function logTransitionStage(mode: Types.StartStopPlayMode, stage: string, detail: string?)
	local message = string.format("[MCP Plugin] studio_transition mode=%s stage=%s", mode, stage)
	if detail ~= nil and detail ~= "" then
		message ..= " " .. detail
	end
	print(message)
end

local function normalizeHelperPort(port: any): string
	if type(port) ~= "string" then
		return DEFAULT_HELPER_PORT
	end
	local digits = string.match(port, "^%s*(%d+)%s*$")
	if digits == nil or digits == "" then
		return DEFAULT_HELPER_PORT
	end
	return digits
end

local function helperBaseUrl(): string
	return "http://127.0.0.1:" .. normalizeHelperPort(PluginUtils.getSettings(HELPER_PORT_SETTING_KEY))
end

local function activeHelperBaseUrl(): string
	return currentHelperBaseUrl or helperBaseUrl()
end

local function removeSessionControlScript()
	local existing = ServerScriptService:FindFirstChild(SESSION_CONTROL_SCRIPT_NAME)
	if existing then
		existing:Destroy()
	end
end

local function luaStringLiteral(value: string): string
	return HttpService:JSONEncode(value)
end

local function installSessionControlScript(targetMode: Types.StartStopPlayMode)
	if currentInstanceId == nil or currentInstanceId == "" then
		logTransitionStage(targetMode, "control_script_skipped", "reason=missing_instance_id")
		return
	end

	removeSessionControlScript()
	local helperUrl = activeHelperBaseUrl()
	local instanceId = currentInstanceId
	local afterId = lastHelperStopRequestId
	local source = string.format(
		[[
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local StudioTestService = game:GetService("StudioTestService")

if not RunService:IsServer() then
	return
end

local helperBaseUrl = %s
local instanceId = %s
local afterId = %d
local pollIntervalSeconds = 0.25

local function pollStopRequest()
	local url = string.format("%%s/v1/mcp/plugin/stop-request?instance_id=%%s&after_id=%%d", helperBaseUrl, instanceId, afterId)
	local response = HttpService:RequestAsync({
		Url = url,
		Method = "GET",
	})
	if not response.Success then
		return nil
	end
	local ok, payload = pcall(function()
		return HttpService:JSONDecode(response.Body)
	end)
	if not ok or type(payload) ~= "table" then
		return nil
	end
	return payload
end

while true do
	local payload = pollStopRequest()
	if payload ~= nil then
		local stopRequestId = tonumber(payload.stop_request_id) or afterId
		if payload.stop_requested == true and stopRequestId > afterId then
			print(string.format("[MCP Plugin] session_control_stop stop_request_id=%%d", stopRequestId))
			local ok, errorMessage = pcall(function()
				StudioTestService:EndTest({
					stopped_by = "mcp_session_control",
					stop_request_id = stopRequestId,
				})
			end)
			if not ok then
				print(string.format("[MCP Plugin] session_control_stop_ignored stop_request_id=%%d error=%%s", stopRequestId, tostring(errorMessage)))
			end
			return
		end
		afterId = math.max(afterId, stopRequestId)
	end
	task.wait(pollIntervalSeconds)
end
]],
		luaStringLiteral(helperUrl),
		luaStringLiteral(instanceId),
		afterId
	)

	local controlScript = Instance.new("Script")
	controlScript.Name = SESSION_CONTROL_SCRIPT_NAME
	controlScript.Source = source
	controlScript.Parent = ServerScriptService
	logTransitionStage(targetMode, "control_script_installed", string.format("instance_id=%s after_id=%d", instanceId, afterId))
end

local function requestStudioLog(startLine: number, lineCount: number)
	local url =
		string.format("%s/v1/helper/studio-log?start_line=%d&line_count=%d", activeHelperBaseUrl(), startLine, lineCount)
	local response = HttpService:RequestAsync({
		Url = url,
		Method = "GET",
	})
	if not response.Success then
		error(string.format("Windows helper studio-log failed with HTTP %d: %s", response.StatusCode, response.Body))
	end
	return HttpService:JSONDecode(response.Body)
end

local function captureLogBaseline(): LogBaseline
	local payload = requestStudioLog(-1, 1)
	return {
		path = tostring(payload.path or ""),
		totalLines = tonumber(payload.total_lines) or 0,
	}
end

local function readLogAfter(baseline: LogBaseline)
	local current = requestStudioLog(-1, 1)
	local currentPath = tostring(current.path or "")
	local currentTotalLines = tonumber(current.total_lines) or 0
	local startLine = 1
	if currentPath == baseline.path then
		startLine = baseline.totalLines + 1
	end
	if currentTotalLines < startLine then
		return currentPath, {}
	end

	local lineCount = math.max(LOG_PAGE_SIZE, currentTotalLines - startLine + 1)
	local payload = requestStudioLog(startLine, lineCount)
	return tostring(payload.path or ""), payload.lines or {}
end

local function lineMatchesAny(line: string, markers: { string }): boolean
	for _, marker in ipairs(markers) do
		if string.find(line, marker, 1, true) ~= nil then
			return true
		end
	end
	return false
end

local function tryFindLogMarker(baseline: LogBaseline, markers: { string }): string?
	local _path, lines = readLogAfter(baseline)
	for _, line in ipairs(lines) do
		local text = tostring(line)
		if lineMatchesAny(text, markers) then
			return text
		end
	end
	return nil
end

local function inferStudioModeFromRecentLog(): ModeLogInference?
	local payload = requestStudioLog(-MODE_LOG_PAGE_SIZE, MODE_LOG_PAGE_SIZE)
	local lines = payload.lines
	if type(lines) ~= "table" then
		return nil
	end

	local path = tostring(payload.path or "")
	local startLine = 1
	local range = payload.range
	if type(range) == "table" then
		startLine = tonumber(range[1]) or startLine
	end

	local inference: ModeLogInference? = nil
	for index, line in ipairs(lines) do
		local text = tostring(line)
		if lineMatchesAny(text, START_PLAY_MARKERS) then
			inference = {
				mode = "start_play",
				path = path,
				lineNumber = startLine + index - 1,
			}
		elseif lineMatchesAny(text, STOP_MARKERS) then
			inference = {
				mode = "stop",
				path = path,
				lineNumber = startLine + index - 1,
			}
		end
	end

	return inference
end

local function isNewModeInference(inference: ModeLogInference): boolean
	return inference.path ~= lastModeInferencePath or inference.lineNumber ~= lastModeInferenceLineNumber
end

local function rememberModeInference(inference: ModeLogInference)
	lastModeInferencePath = inference.path
	lastModeInferenceLineNumber = inference.lineNumber
end

local function getStudioMode(): Types.StartStopPlayMode
	local success, inference = pcall(inferStudioModeFromRecentLog)
	if success and type(inference) == "table" then
		if inference.mode == "stop" then
			rememberModeInference(inference)
			StudioModeTracker.markStopped()
			return "stop"
		elseif inference.mode == "start_play" or inference.mode == "run_server" then
			if isNewModeInference(inference) then
				rememberModeInference(inference)
				StudioModeTracker.markObservedRunning(inference.mode)
				return inference.mode
			end
		end
	end

	StudioModeTracker.normalizeEditState()
	return StudioModeTracker.getSnapshot().mode
end

local function isPreviousTestStillInProgress(errorMessage: string): boolean
	return string.find(errorMessage, "previous", 1, true) ~= nil
		and string.find(errorMessage, "still in progress", 1, true) ~= nil
end

local function startAsyncCall(executor: () -> any): AsyncCallState
	local state: AsyncCallState = {
		done = false,
	}
	task.spawn(function()
		local success, result = pcall(executor)
		state.done = true
		state.success = success
		if not success then
			state.errorMessage = tostring(result)
		end
	end)
	return state
end

local function waitForStopLog(baseline: LogBaseline, timeoutSeconds: number): string
	local deadline = os.clock() + timeoutSeconds
	local lastLogError: string? = nil

	while os.clock() < deadline do
		local success, matchedLine = pcall(function()
			return tryFindLogMarker(baseline, STOP_MARKERS)
		end)
		if success and matchedLine ~= nil then
			return matchedLine
		elseif not success then
			lastLogError = tostring(matchedLine)
		end

		StudioModeTracker.normalizeEditState()
		task.wait(POLL_INTERVAL_SECONDS)
	end

	local snapshot = StudioModeTracker.getSnapshot()
	error(
		string.format(
			"Timed out waiting for Studio stop log. mode=%s active_session_id=%d pending_start_id=%d last_log_error=%s",
			snapshot.mode,
			snapshot.activeSessionId,
			snapshot.pendingStartId,
			tostring(lastLogError)
		)
	)
end

local function stop(force: boolean?): string?
	ConsoleOutput.outputMessage = ""
	local currentMode = getStudioMode()
	local snapshot = StudioModeTracker.getSnapshot()
	if force ~= true and currentMode == "stop" and snapshot.activeSessionId == 0 then
		StudioModeTracker.markStopped()
		removeSessionControlScript()
		if snapshot.pendingStartId > 0 then
			logTransitionStage(
				"stop",
				"noop_cleared_stale_pending",
				string.format("pending_start_id=%d", snapshot.pendingStartId)
			)
		else
			logTransitionStage("stop", "noop")
		end
		return "Stopped"
	end

	local baseline = captureLogBaseline()
	local stopRequestId = 0
	local helperStopSignaled = false
	if currentInstanceId ~= nil and currentInstanceId ~= "" then
		local signalOk, signalResult = pcall(function()
			local response = HttpService:RequestAsync({
				Url = activeHelperBaseUrl() .. "/v1/mcp/plugin/stop-request",
				Method = "POST",
				Headers = {
					["Content-Type"] = "application/json",
				},
				Body = HttpService:JSONEncode({
					instance_id = currentInstanceId,
				}),
			})
			if not response.Success then
				error(string.format("HTTP %d: %s", response.StatusCode, response.Body))
			end
			return HttpService:JSONDecode(response.Body)
		end)
		if signalOk and type(signalResult) == "table" then
			lastHelperStopRequestId = math.max(lastHelperStopRequestId, tonumber(signalResult.stop_request_id) or 0)
			stopRequestId = lastHelperStopRequestId
			helperStopSignaled = true
			logTransitionStage("stop", "helper_stop_signaled", string.format("stop_request_id=%d", lastHelperStopRequestId))
		else
			logTransitionStage("stop", "helper_stop_signal_failed", tostring(signalResult))
		end
	end
	if not helperStopSignaled then
		error("Unable to signal the running Studio session to stop through the helper")
	end
	logTransitionStage(
		"stop",
		"requested",
		string.format(
			"current_mode=%s stop_request_id=%d force=%s",
			currentMode,
			stopRequestId,
			tostring(force == true)
		)
	)

	local matchedLine = waitForStopLog(baseline, STOP_TIMEOUT_SECONDS)

	StudioModeTracker.markStopped()
	removeSessionControlScript()
	logTransitionStage("stop", "observed", string.format("matched_line=%s", matchedLine))
	return "Stopped"
end

local function waitForStartPlayLogOrRetry(
	baseline: LogBaseline,
	commandId: number,
	startCall: AsyncCallState,
	deadline: number
): (boolean, string?)
	local lastLogError: string? = nil
	while os.clock() < deadline do
		local success, matchedLine = pcall(function()
			return tryFindLogMarker(baseline, START_PLAY_MARKERS)
		end)
		if success and matchedLine ~= nil then
			StudioModeTracker.markStarted(commandId, "start_play")
			logTransitionStage(
				"start_play",
				"observed",
				string.format("command_id=%d matched_line=%s", commandId, matchedLine)
			)
			return true, nil
		elseif not success then
			lastLogError = tostring(matchedLine)
		end

		if startCall.done and startCall.success == false then
			local errorMessage = tostring(startCall.errorMessage)
			if isPreviousTestStillInProgress(errorMessage) then
				return false, errorMessage
			end
			StudioModeTracker.recordTransitionError(commandId, errorMessage)
			error(errorMessage)
		end

		task.wait(POLL_INTERVAL_SECONDS)
	end

	local snapshot = StudioModeTracker.getSnapshot()
	local errorMessage = string.format(
		"Timed out waiting for Studio to enter start_play. mode=%s active_session_id=%d pending_start_id=%d last_log_error=%s",
		snapshot.mode,
		snapshot.activeSessionId,
		snapshot.pendingStartId,
		tostring(lastLogError)
	)
	StudioModeTracker.recordTransitionError(commandId, errorMessage)
	error(errorMessage)
end

local function startPlayMode(successMessage: string): string?
	ConsoleOutput.outputMessage = ""
	local currentMode = getStudioMode()
	local snapshot = StudioModeTracker.getSnapshot()
	if currentMode ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage("start_play", "stop_before_start", string.format("current_mode=%s", currentMode))
		stop(false)
	end

	local deadline = os.clock() + START_TIMEOUT_SECONDS
	local attempt = 0
	local lastCommandId = 0

	while os.clock() < deadline do
		local commandId = StudioModeTracker.beginStart("start_play")
		lastCommandId = commandId
		attempt += 1
		local baseline = captureLogBaseline()
		installSessionControlScript("start_play")
		logTransitionStage("start_play", "requested", string.format("command_id=%d attempt=%d", commandId, attempt))
		local startCall = startAsyncCall(function()
			return StudioTestService:ExecutePlayModeAsync({})
		end)

		local observed, retryReason = waitForStartPlayLogOrRetry(baseline, commandId, startCall, deadline)
		if observed then
			return successMessage
		end

		logTransitionStage(
			"start_play",
			"retry_wait",
			string.format("command_id=%d attempt=%d error=%s", commandId, attempt, tostring(retryReason))
		)
		stop(true)
		task.wait(POLL_INTERVAL_SECONDS)
	end

	local snapshotAfterTimeout = StudioModeTracker.getSnapshot()
	local errorMessage = string.format(
		"Timed out waiting for Studio to accept start_play. mode=%s active_session_id=%d pending_start_id=%d",
		snapshotAfterTimeout.mode,
		snapshotAfterTimeout.activeSessionId,
		snapshotAfterTimeout.pendingStartId
	)
	StudioModeTracker.recordTransitionError(lastCommandId, errorMessage)
	error(errorMessage)
end

local function startRunServerMode(executor: () -> any, successMessage: string): string?
	ConsoleOutput.outputMessage = ""
	local currentMode = getStudioMode()
	local snapshot = StudioModeTracker.getSnapshot()
	if currentMode ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage("run_server", "stop_before_start", string.format("current_mode=%s", currentMode))
		stop(false)
	end

	local commandId = StudioModeTracker.beginStart("run_server")
	installSessionControlScript("run_server")
	logTransitionStage("run_server", "requested", string.format("command_id=%d", commandId))

	local success, result = pcall(executor)
	if not success then
		local errorMessage = tostring(result)
		StudioModeTracker.recordTransitionError(commandId, errorMessage)
		error(errorMessage)
	end

	StudioModeTracker.waitForStart(commandId, "run_server", START_TIMEOUT_SECONDS)
	logTransitionStage("run_server", "observed", string.format("command_id=%d", commandId))
	return successMessage
end

local function handleStartStopPlay(startStopPlayArgs: Types.StartStopPlayArgs): string?
	if type(startStopPlayArgs.mode) ~= "string" then
		error("Missing mode in StartStopPlay")
	end

	if startStopPlayArgs.mode == "start_play" then
		return startPlayMode("Started play")
	elseif startStopPlayArgs.mode == "stop" then
		return stop(false)
	elseif startStopPlayArgs.mode == "run_server" then
		return startRunServerMode(function()
			return StudioTestService:ExecuteRunModeAsync({})
		end, "Ran server")
	end

	error("Invalid mode in StartStopPlay, must be start_play, stop, or run_server")
end

local function buildLaunchMessage(requestedMode: Types.TestMode, restartApplied: boolean): string
	if restartApplied then
		return string.format("Stopped the previous Studio session and launched %s.", requestedMode)
	end
	return string.format("Launched Studio session in %s.", requestedMode)
end

local function handleLaunchStudioSession(launchStudioSessionArgs: Types.LaunchStudioSessionArgs): string?
	if type(launchStudioSessionArgs.mode) ~= "string" then
		error("Missing mode in LaunchStudioSession")
	end
	if launchStudioSessionArgs.mode ~= "start_play" and launchStudioSessionArgs.mode ~= "run_server" then
		error("Invalid mode in LaunchStudioSession, must be start_play or run_server")
	end

	local requestedMode = launchStudioSessionArgs.mode
	local previousMode = getStudioMode()
	local previousSnapshot = StudioModeTracker.getSnapshot()
	local restartApplied = previousMode ~= "stop" or previousSnapshot.pendingStartId ~= 0
	local actions = {}

	if restartApplied then
		table.insert(actions, "stop")
		stop(false)
	end

	table.insert(actions, requestedMode)
	if requestedMode == "start_play" then
		startPlayMode("Started play")
	else
		startRunServerMode(function()
			return StudioTestService:ExecuteRunModeAsync({})
		end, "Ran server")
	end

	local finalMode = getStudioMode()
	if finalMode ~= requestedMode then
		error(string.format("LaunchStudioSession ended in %s, expected %s", finalMode, requestedMode))
	end

	return HttpService:JSONEncode({
		requested_mode = requestedMode,
		restart_applied = restartApplied,
		previous_mode = previousMode,
		final_mode = finalMode,
		actions = actions,
		message = buildLaunchMessage(requestedMode, restartApplied),
	})
end

return {
	configureHelperContext = configureHelperContext,
	getStudioMode = getStudioMode,
	handleLaunchStudioSession = handleLaunchStudioSession,
	handleStartStopPlay = handleStartStopPlay,
}
