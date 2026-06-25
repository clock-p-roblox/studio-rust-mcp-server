local Main = script:FindFirstAncestor("MCPStudioPlugin")
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local PluginUtils = require(Main.Utils.PluginUtils)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)
local Types = require(Main.Types)

local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local ServerScriptService = game:GetService("ServerScriptService")
local StudioTestService = game:GetService("StudioTestService")

local START_TIMEOUT_SECONDS = 60
local STOP_TIMEOUT_SECONDS = 60
local POLL_INTERVAL_SECONDS = 0.25
local HELPER_PORT_SETTING_KEY = "MCP-helper-port"
local DEFAULT_HELPER_PORT = "44750"
local LOG_PAGE_SIZE = 400
local SESSION_CONTROL_SCRIPT_NAME = "MCPStudioSessionControl"
local SESSION_CONTROL_MARKER = "clockp_mcp_session_control_v1"

type LogBaseline = {
	path: string,
	totalLines: number,
}

type AsyncCallState = {
	done: boolean,
	success: boolean?,
	errorMessage: string?,
}

local START_PLAY_MARKERS = {
	"[runtime-launch]",
	"State: PlaySoloIdle",
	"PlaySoloPlayableTotalTime",
	"PlaySoloSuccess",
}

local currentHelperBaseUrl: string? = nil
local currentInstanceId: string? = nil
local currentTaskId: string? = nil
local lastHelperStopRequestId = 0

local function configureHelperContext(helperUrl: string?, instanceId: string?, taskId: string?)
	currentHelperBaseUrl = helperUrl
	currentInstanceId = instanceId
	currentTaskId = taskId
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

local function isManagedSessionControlScript(instance: Instance): boolean
	if instance.Name ~= SESSION_CONTROL_SCRIPT_NAME or not instance:IsA("Script") then
		return false
	end
	if instance:GetAttribute(SESSION_CONTROL_MARKER) == true then
		return true
	end
	local ok, source = pcall(function()
		return instance.Source
	end)
	return ok
		and type(source) == "string"
		and (
			string.find(source, SESSION_CONTROL_MARKER, 1, true) ~= nil
			or (
				string.find(source, "/v1/mcp/plugin/stop-request", 1, true) ~= nil
				and string.find(source, "runtime_stop_request_received", 1, true) ~= nil
				and string.find(source, "StudioTestService:EndTest", 1, true) ~= nil
			)
		)
end

local function removeSessionControlScript()
	for _, child in ServerScriptService:GetChildren() do
		if isManagedSessionControlScript(child) then
			child:Destroy()
		end
	end
end

local function requestHelperStop(): number
	if currentInstanceId == nil or currentInstanceId == "" then
		error("Cannot request Studio stop before plugin is registered with the helper.")
	end

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
	local payload = HttpService:JSONDecode(response.Body)
	local stopRequestId = tonumber(payload.stop_request_id) or 0
	if payload.stop_requested ~= true or stopRequestId <= 0 then
		error("helper did not record a valid stop request")
	end
	return stopRequestId
end

local function assertNoForeignSessionControlScript(targetMode: Types.StartStopPlayMode)
	for _, child in ServerScriptService:GetChildren() do
		if child.Name == SESSION_CONTROL_SCRIPT_NAME and not isManagedSessionControlScript(child) then
			logTransitionStage(targetMode, "control_script_name_conflict", child:GetFullName())
			error("Cannot install MCPStudioSessionControl because a non-clockp script already uses that name.")
		end
	end
end

local function luaStringLiteral(value: string): string
	return HttpService:JSONEncode(value)
end

local function ensureHttpEnabledForSessionControl(targetMode: Types.StartStopPlayMode)
	local enabled = false
	pcall(function()
		enabled = HttpService.HttpEnabled
	end)
	if enabled ~= true then
		logTransitionStage(targetMode, "http_disabled")
		error(
			"Studio session control requires HttpService HTTP requests to be enabled for the experience. "
				.. "Studio plugins and Rojo cannot enable this setting at launch time; enable HTTP in Game Settings before launching."
		)
	end
end

local function installSessionControlScript(targetMode: Types.StartStopPlayMode)
	if currentInstanceId == nil or currentInstanceId == "" then
		logTransitionStage(targetMode, "control_script_skipped", "reason=missing_instance_id")
		return
	end

	ensureHttpEnabledForSessionControl(targetMode)
	removeSessionControlScript()
	assertNoForeignSessionControlScript(targetMode)
	local helperUrl = activeHelperBaseUrl()
	local instanceId = currentInstanceId
	local source = string.format(
		[[
-- clockp_mcp_session_control_v1
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local StudioTestService = game:GetService("StudioTestService")

if not RunService:IsStudio() or not RunService:IsServer() then
	return
end

local httpEnabled = false
pcall(function()
	httpEnabled = HttpService.HttpEnabled
end)
if httpEnabled ~= true then
	print("[MCP Plugin] runtime_session_control_disabled reason=http_disabled")
	return
end

local helperBaseUrl = %s
local instanceId = %s
local targetMode = %s
local pollIntervalSeconds = 0.25
local heartbeatIntervalSeconds = 0.5
local lastHeartbeatAt = 0
local lastStopRequestId = 0

local function runtimeStateSummary()
	local pieces = {}
	local okStudio, isStudio = pcall(function()
		return RunService:IsStudio()
	end)
	table.insert(pieces, "is_studio=" .. tostring(okStudio and isStudio))
	local okServer, isServer = pcall(function()
		return RunService:IsServer()
	end)
	table.insert(pieces, "is_server=" .. tostring(okServer and isServer))
	local okClient, isClient = pcall(function()
		return RunService:IsClient()
	end)
	table.insert(pieces, "is_client=" .. tostring(okClient and isClient))
	local okRunning, isRunning = pcall(function()
		return RunService:IsRunning()
	end)
	table.insert(pieces, "is_running=" .. tostring(okRunning and isRunning))
	local okRunMode, isRunMode = pcall(function()
		return RunService:IsRunMode()
	end)
	table.insert(pieces, "is_run_mode=" .. tostring(okRunMode and isRunMode))
	return table.concat(pieces, ",")
end

local function postStopResult(stopRequestId, phase, errorMessage)
	local body = {
		instance_id = instanceId,
		stop_request_id = stopRequestId,
		phase = phase,
	}
	if errorMessage ~= nil then
		body.error = tostring(errorMessage)
	end
	print(string.format(
		"[MCP Plugin] runtime_stop_result_http_begin stop_request_id=%%d phase=%%s %%s",
		stopRequestId,
		tostring(phase),
		runtimeStateSummary()
	))
	local response = HttpService:RequestAsync({
		Url = helperBaseUrl .. "/v1/mcp/plugin/stop-result",
		Method = "POST",
		Headers = {
			["Content-Type"] = "application/json",
		},
		Body = HttpService:JSONEncode(body),
	})
	print(string.format(
		"[MCP Plugin] runtime_stop_result_http_end stop_request_id=%%d phase=%%s success=%%s status=%%s body_len=%%d %%s",
		stopRequestId,
		tostring(phase),
		tostring(response.Success),
		tostring(response.StatusCode),
		string.len(tostring(response.Body or "")),
		runtimeStateSummary()
	))
	if not response.Success then
		error(string.format("HTTP %%d: %%s", response.StatusCode, response.Body))
	end
end

local function inferRuntimeMode()
	local ok, isRunMode = pcall(function()
		return RunService:IsRunMode()
	end)
	if ok and isRunMode == true then
		return "run_server"
	end
	return targetMode
end

local function sendControlHeartbeat()
	if os.clock() - lastHeartbeatAt < heartbeatIntervalSeconds then
		return
	end
	local response = HttpService:RequestAsync({
		Url = helperBaseUrl .. "/v1/mcp/plugin/control-heartbeat",
		Method = "POST",
		Headers = {
			["Content-Type"] = "application/json",
		},
		Body = HttpService:JSONEncode({
			instance_id = instanceId,
			mode = inferRuntimeMode(),
		}),
	})
	if not response.Success then
		error(string.format("HTTP %%d: %%s", response.StatusCode, response.Body))
	end
	lastHeartbeatAt = os.clock()
end

local function pollStopRequest()
	local url = string.format(
		"%%s/v1/mcp/plugin/stop-request?instance_id=%%s&after_id=%%d",
		helperBaseUrl,
		HttpService:UrlEncode(instanceId),
		lastStopRequestId
	)
	local response = HttpService:RequestAsync({
		Url = url,
		Method = "GET",
	})
	if not response.Success then
		error(string.format("HTTP %%d: %%s", response.StatusCode, response.Body))
	end
	local payload = HttpService:JSONDecode(response.Body)
	local stopRequestId = tonumber(payload.stop_request_id) or 0
	if payload.stop_requested == true and stopRequestId > lastStopRequestId then
		local previousStopRequestId = lastStopRequestId
		lastStopRequestId = stopRequestId
		print(string.format(
			"[MCP Plugin] runtime_stop_request_received stop_request_id=%%d after_id=%%d %%s",
			stopRequestId,
			previousStopRequestId,
			runtimeStateSummary()
		))
		local observedPosted, observedPostErr = pcall(function()
			postStopResult(stopRequestId, "observed", nil)
		end)
		print(string.format(
			"[MCP Plugin] runtime_stop_observed_http_result stop_request_id=%%d success=%%s error=%%s %%s",
			stopRequestId,
			tostring(observedPosted),
			tostring(observedPostErr),
			runtimeStateSummary()
		))
		print(string.format("[MCP Plugin] runtime_stop_call_begin stop_request_id=%%d %%s", stopRequestId, runtimeStateSummary()))
		local ok, err = pcall(function()
			StudioTestService:EndTest({
				stopped_by = "mcp_session_control",
				stop_request_id = stopRequestId,
			})
		end)
		print(string.format(
			"[MCP Plugin] runtime_stop_call_end stop_request_id=%%d success=%%s error=%%s %%s",
			stopRequestId,
			tostring(ok),
			tostring(err),
			runtimeStateSummary()
		))
		if not ok then
			print(string.format("[MCP Plugin] runtime_stop_actuator_failed stop_request_id=%%d error=%%s", stopRequestId, tostring(err)))
			pcall(function()
				postStopResult(stopRequestId, "failed", tostring(err))
			end)
		end
	end
end

pcall(sendControlHeartbeat)
while true do
	pcall(sendControlHeartbeat)
	pcall(pollStopRequest)
	task.wait(pollIntervalSeconds)
end
]],
		luaStringLiteral(helperUrl),
		luaStringLiteral(instanceId),
		luaStringLiteral(targetMode)
	)

	local controlScript = Instance.new("Script")
	controlScript.Name = SESSION_CONTROL_SCRIPT_NAME
	controlScript.Source = source
	controlScript:SetAttribute(SESSION_CONTROL_MARKER, true)
	controlScript.Parent = ServerScriptService
	logTransitionStage(targetMode, "control_script_installed", string.format("instance_id=%s", instanceId))
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

local function refreshTrackerFromLiveDataModel()
	if RunService:IsRunning() then
		StudioModeTracker.observeCurrentDataModel()
	else
		StudioModeTracker.normalizeEditState()
	end
end

local function getStudioSessionState(): Types.StudioSessionState
	refreshTrackerFromLiveDataModel()
	local snapshot = StudioModeTracker.getSnapshot()
	if snapshot.pendingStartId > 0 then
		return "starting_play"
	end
	if snapshot.mode == "stop" then
		return "stop"
	end
	if StudioModeTracker.getStopRequestId() > 0 then
		return "stopping"
	end
	return "play"
end

local function getStudioSessionStateJson(): string
	local state = getStudioSessionState()
	local snapshot = StudioModeTracker.getSnapshot()
	return HttpService:JSONEncode({
		studio_session_state = state,
		mode = snapshot.mode,
		pending_start_id = snapshot.pendingStartId,
		active_session_id = snapshot.activeSessionId,
		stop_request_id = StudioModeTracker.getStopRequestId(),
		last_error = snapshot.lastError,
	})
end

local function getStudioMode(): Types.StartStopPlayMode
	refreshTrackerFromLiveDataModel()
	return StudioModeTracker.getSnapshot().mode
end

local function cleanupStoppedSessionControl(mode: Types.StartStopPlayMode)
	local snapshot = StudioModeTracker.getSnapshot()
	if mode == "stop" and snapshot.pendingStartId == 0 then
		removeSessionControlScript()
	end
end

local function isPreviousTestStillInProgress(errorMessage: string): boolean
	local lowered = string.lower(errorMessage)
	return string.find(lowered, "previous", 1, true) ~= nil
		and (
			string.find(lowered, "still in progress", 1, true) ~= nil
			or string.find(lowered, "has not been completed", 1, true) ~= nil
		)
end

local function previousTestInProgressError(errorMessage: string): string
	return "previous_test_in_progress: StudioTestService still has a previous test in progress; "
		.. "start_play or run_server was not retried. "
		.. "Restart Studio before launching again; start_stop_play(stop) is not a recovery path when the helper snapshot is already stop. "
		.. "Original error: "
		.. errorMessage
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

local function stop(force: boolean?): string?
	ConsoleOutput.outputMessage = ""
	local currentState = getStudioSessionState()
	local snapshot = StudioModeTracker.getSnapshot()
	if force ~= true and currentState == "stop" then
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

	if currentState == "starting_play" then
		logTransitionStage(
			"stop",
			"rejected_starting_play",
			string.format("pending_start_id=%d", snapshot.pendingStartId)
		)
		error("studio_starting_play: Studio is still entering play mode; wait for play or stop before issuing stop")
	end

	if currentState == "stopping" then
		logTransitionStage(
			"stop",
			"rejected_stop_in_progress",
			string.format("stop_request_id=%d", StudioModeTracker.getStopRequestId())
		)
		error("studio_stop_in_progress: Studio stop is already in progress")
	end

	if currentState == "play" then
		local stopRequestId = requestHelperStop()
		StudioModeTracker.markStopping(stopRequestId)
		logTransitionStage("stop", "requested", string.format("stop_request_id=%d", stopRequestId))
		StudioModeTracker.waitForStop(STOP_TIMEOUT_SECONDS)
		removeSessionControlScript()
		logTransitionStage("stop", "observed", string.format("stop_request_id=%d", stopRequestId))
		return "Stopped"
	end

	logTransitionStage(
		"stop",
		"rejected_edit_runtime_stop",
		string.format("current_state=%s pending_start_id=%d force=%s", currentState, snapshot.pendingStartId, tostring(force == true))
	)
	error(
		"studio_session_state_not_stoppable: current_state=" .. currentState
	)
end

local function waitForStartPlayLog(
	baseline: LogBaseline,
	commandId: number,
	startCall: AsyncCallState,
	deadline: number
)
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
				removeSessionControlScript()
				StudioModeTracker.recordTransitionError(commandId, errorMessage)
				error(previousTestInProgressError(errorMessage))
			end
			if getStudioMode() == "stop" then
				removeSessionControlScript()
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
	local currentState = getStudioSessionState()
	local snapshot = StudioModeTracker.getSnapshot()
	if currentState ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage(
			"start_play",
			"rejected_running_session",
			string.format("current_state=%s pending_start_id=%d", currentState, snapshot.pendingStartId)
		)
		if currentState == "starting_play" then
			error("launch_already_in_progress: Studio is already entering play/run mode")
		elseif currentState == "stopping" then
			error("studio_stop_in_progress: Studio stop is already in progress")
		elseif currentState == "play" then
			error("studio_already_playing: Studio is already in play/run mode")
		end
		error("studio_session_state_not_launchable: current_state=" .. currentState)
	end

	local commandId = StudioModeTracker.beginStart("start_play")
	local baseline = captureLogBaseline()
	installSessionControlScript("start_play")
	logTransitionStage("start_play", "requested", string.format("command_id=%d", commandId))
	local startCall = startAsyncCall(function()
		return StudioTestService:ExecutePlayModeAsync({})
	end)

	local deadline = os.clock() + START_TIMEOUT_SECONDS
	waitForStartPlayLog(baseline, commandId, startCall, deadline)
	return successMessage
end

local function startRunServerMode(executor: () -> any, successMessage: string): string?
	ConsoleOutput.outputMessage = ""
	local currentState = getStudioSessionState()
	local snapshot = StudioModeTracker.getSnapshot()
	if currentState ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage(
			"run_server",
			"rejected_running_session",
			string.format("current_state=%s pending_start_id=%d", currentState, snapshot.pendingStartId)
		)
		if currentState == "starting_play" then
			error("launch_already_in_progress: Studio is already entering play/run mode")
		elseif currentState == "stopping" then
			error("studio_stop_in_progress: Studio stop is already in progress")
		elseif currentState == "play" then
			error("studio_already_playing: Studio is already in play/run mode")
		end
		error("studio_session_state_not_launchable: current_state=" .. currentState)
	end

	local commandId = StudioModeTracker.beginStart("run_server")
	installSessionControlScript("run_server")
	logTransitionStage("run_server", "requested", string.format("command_id=%d", commandId))

	local success, result = pcall(executor)
	if not success then
		local errorMessage = tostring(result)
		StudioModeTracker.recordTransitionError(commandId, errorMessage)
		if isPreviousTestStillInProgress(errorMessage) then
			removeSessionControlScript()
			error(previousTestInProgressError(errorMessage))
		end
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

	if startStopPlayArgs.mode == "stop" then
		return stop(false)
	end

	error("Invalid mode in StartStopPlay; start_stop_play only supports stop. Use LaunchStudioSession to enter start_play or run_server.")
end

local function buildLaunchMessage(requestedMode: Types.TestMode): string
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
	local previousState = getStudioSessionState()
	local previousSnapshot = StudioModeTracker.getSnapshot()
	local actions = {}

	if previousState ~= "stop" or previousSnapshot.pendingStartId ~= 0 then
		logTransitionStage(
			requestedMode,
			"rejected_launch_not_in_stop",
			string.format("previous_state=%s pending_start_id=%d", previousState, previousSnapshot.pendingStartId)
		)
		if previousState == "starting_play" then
			error("launch_already_in_progress: Studio is already entering play/run mode")
		elseif previousState == "stopping" then
			error("studio_stop_in_progress: Studio stop is already in progress")
		elseif previousState == "play" then
			error("studio_already_playing: Studio is already in play/run mode")
		end
		error("LaunchStudioSession requires Studio to be in edit mode before launch.")
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
		restart_applied = false,
		previous_mode = previousState,
		final_mode = finalMode,
		actions = actions,
		message = buildLaunchMessage(requestedMode),
	})
end

return {
	configureHelperContext = configureHelperContext,
	cleanupStoppedSessionControl = cleanupStoppedSessionControl,
	getStudioMode = getStudioMode,
	getStudioSessionState = getStudioSessionState,
	getStudioSessionStateJson = getStudioSessionStateJson,
	handleLaunchStudioSession = handleLaunchStudioSession,
	handleStartStopPlay = handleStartStopPlay,
	installSessionControlScript = installSessionControlScript,
}
