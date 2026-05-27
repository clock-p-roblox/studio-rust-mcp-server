local Main = script:FindFirstAncestor("MCPStudioPlugin")
local Types = require(Main.Types)
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local PluginUtils = require(Main.Utils.PluginUtils)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)

local HttpService = game:GetService("HttpService")
local StudioTestService = game:GetService("StudioTestService")

local START_TIMEOUT_SECONDS = 60
local STOP_TIMEOUT_SECONDS = 60
local POLL_INTERVAL_SECONDS = 0.25
local HELPER_PORT_SETTING_KEY = "MCP-helper-port"
local DEFAULT_HELPER_PORT = "44750"
local LOG_PAGE_SIZE = 400

type LogBaseline = {
	path: string,
	totalLines: number,
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

local function requestStudioLog(startLine: number, lineCount: number)
	local url = string.format("%s/v1/helper/studio-log?start_line=%d&line_count=%d", helperBaseUrl(), startLine, lineCount)
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
	StudioModeTracker.normalizeEditState()
	local snapshot = StudioModeTracker.getSnapshot()
	if force ~= true and snapshot.mode == "stop" and snapshot.pendingStartId == 0 then
		logTransitionStage("stop", "noop")
		return "Stopped"
	end

	local baseline = captureLogBaseline()
	local stopRequestId = StudioModeTracker.requestStop()
	logTransitionStage(
		"stop",
		"requested",
		string.format("current_mode=%s stop_request_id=%d force=%s", snapshot.mode, stopRequestId, tostring(force == true))
	)

	local stopCall = startAsyncCall(function()
		return StudioTestService:EndTest({})
	end)
	local matchedLine = waitForStopLog(baseline, STOP_TIMEOUT_SECONDS)
	if stopCall.done and stopCall.success == false then
		logTransitionStage("stop", "edit_end_test_ignored", tostring(stopCall.errorMessage))
	end

	StudioModeTracker.markStopped()
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
			logTransitionStage("start_play", "observed", string.format("command_id=%d matched_line=%s", commandId, matchedLine))
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
	error(
		string.format(
			"Timed out waiting for Studio to enter start_play. mode=%s active_session_id=%d pending_start_id=%d last_log_error=%s",
			snapshot.mode,
			snapshot.activeSessionId,
			snapshot.pendingStartId,
			tostring(lastLogError)
		)
	)
end

local function startPlayMode(successMessage: string): string?
	ConsoleOutput.outputMessage = ""
	local snapshot = StudioModeTracker.getSnapshot()
	if snapshot.mode ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage("start_play", "stop_before_start", string.format("current_mode=%s", snapshot.mode))
		stop(false)
	end

	local deadline = os.clock() + START_TIMEOUT_SECONDS
	local attempt = 0

	while os.clock() < deadline do
		local commandId = StudioModeTracker.beginStart("start_play")
		attempt += 1
		local baseline = captureLogBaseline()
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
	error(
		string.format(
			"Timed out waiting for Studio to accept start_play. mode=%s active_session_id=%d pending_start_id=%d",
			snapshotAfterTimeout.mode,
			snapshotAfterTimeout.activeSessionId,
			snapshotAfterTimeout.pendingStartId
		)
	)
end

local function startRunServerMode(executor: () -> any, successMessage: string): string?
	ConsoleOutput.outputMessage = ""
	local snapshot = StudioModeTracker.getSnapshot()
	if snapshot.mode ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage("run_server", "stop_before_start", string.format("current_mode=%s", snapshot.mode))
		stop(false)
	end

	local commandId = StudioModeTracker.beginStart("run_server")
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
	local previousSnapshot = StudioModeTracker.getSnapshot()
	local previousMode = previousSnapshot.mode
	local restartApplied = previousSnapshot.mode ~= "stop" or previousSnapshot.pendingStartId ~= 0
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

	local finalMode = StudioModeTracker.getSnapshot().mode
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
	handleLaunchStudioSession = handleLaunchStudioSession,
	handleStartStopPlay = handleStartStopPlay,
}
