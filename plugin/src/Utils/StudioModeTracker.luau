local Main = script:FindFirstAncestor("MCPStudioPlugin")
local PluginUtils = require(Main.Utils.PluginUtils)
local Types = require(Main.Types)

local RunService = game:GetService("RunService")

local MODE_KEY = "MCP_STUDIO_MODE_TRACKER_MODE"
local NEXT_COMMAND_ID_KEY = "MCP_STUDIO_MODE_TRACKER_NEXT_COMMAND_ID"
local PENDING_START_ID_KEY = "MCP_STUDIO_MODE_TRACKER_PENDING_START_ID"
local PENDING_START_MODE_KEY = "MCP_STUDIO_MODE_TRACKER_PENDING_START_MODE"
local ACTIVE_SESSION_ID_KEY = "MCP_STUDIO_MODE_TRACKER_ACTIVE_SESSION_ID"
local CONTROL_READY_SESSION_ID_KEY = "MCP_STUDIO_MODE_TRACKER_CONTROL_READY_SESSION_ID"
local STOP_REQUEST_ID_KEY = "MCP_STUDIO_MODE_TRACKER_STOP_REQUEST_ID"
local LAST_ERROR_ID_KEY = "MCP_STUDIO_MODE_TRACKER_LAST_ERROR_ID"
local LAST_ERROR_KEY = "MCP_STUDIO_MODE_TRACKER_LAST_ERROR"
local WAIT_STEP_SECONDS = 0.05

export type TrackerSnapshot = {
	mode: Types.StartStopPlayMode,
	pendingStartId: number,
	pendingStartMode: Types.StartStopPlayMode?,
	activeSessionId: number,
	controlReadySessionId: number,
	lastErrorId: number,
	lastError: string?,
}

local function readNonNegativeNumber(key: string): number
	local value = PluginUtils.getSettings(key)
	if type(value) == "number" and value >= 0 then
		return value
	end
	return 0
end

local function sanitizeMode(value: any): Types.StartStopPlayMode
	if value == "start_play" or value == "run_server" then
		return value
	end
	return "stop"
end

local function readPendingStartMode(): Types.StartStopPlayMode?
	local value = PluginUtils.getSettings(PENDING_START_MODE_KEY)
	if value == "start_play" or value == "run_server" then
		return value
	end
	return nil
end

local function writePendingStartMode(mode: Types.StartStopPlayMode?)
	if mode == "start_play" or mode == "run_server" then
		PluginUtils.setSettings(PENDING_START_MODE_KEY, mode)
		return
	end
	PluginUtils.setSettings(PENDING_START_MODE_KEY, "")
end

local function writeLastError(errorId: number, errorMessage: string?)
	PluginUtils.setSettings(LAST_ERROR_ID_KEY, errorId)
	PluginUtils.setSettings(LAST_ERROR_KEY, errorMessage or "")
end

local function nextCommandId(): number
	local nextId = readNonNegativeNumber(NEXT_COMMAND_ID_KEY) + 1
	PluginUtils.setSettings(NEXT_COMMAND_ID_KEY, nextId)
	return nextId
end

local function getSnapshot(): TrackerSnapshot
	local lastError = PluginUtils.getSettings(LAST_ERROR_KEY)
	if type(lastError) ~= "string" or lastError == "" then
		lastError = nil
	end

	return {
		mode = sanitizeMode(PluginUtils.getSettings(MODE_KEY)),
		pendingStartId = readNonNegativeNumber(PENDING_START_ID_KEY),
		pendingStartMode = readPendingStartMode(),
		activeSessionId = readNonNegativeNumber(ACTIVE_SESSION_ID_KEY),
		controlReadySessionId = readNonNegativeNumber(CONTROL_READY_SESSION_ID_KEY),
		lastErrorId = readNonNegativeNumber(LAST_ERROR_ID_KEY),
		lastError = lastError,
	}
end

local function clearPendingStart()
	PluginUtils.setSettings(PENDING_START_ID_KEY, 0)
	writePendingStartMode(nil)
end

local function clearLastError()
	writeLastError(0, nil)
end

local function markStopped()
	PluginUtils.setSettings(MODE_KEY, "stop")
	PluginUtils.setSettings(ACTIVE_SESSION_ID_KEY, 0)
	PluginUtils.setSettings(CONTROL_READY_SESSION_ID_KEY, 0)
	clearPendingStart()
	clearLastError()
end

local function normalizeEditState()
	if RunService:IsRunning() then
		return
	end

	if RunService:IsEdit() then
		markStopped()
	end
end

local function inferLiveMode(): Types.StartStopPlayMode
	if RunService:IsEdit() and not RunService:IsRunning() then
		return "stop"
	end

	local isRunMode = false
	local ok, result = pcall(function()
		return RunService:IsRunMode()
	end)
	if ok and result == true then
		isRunMode = true
	end

	if isRunMode then
		return "run_server"
	end

	if RunService:IsRunning() or RunService:IsServer() or RunService:IsClient() then
		return "start_play"
	end

	return "stop"
end

local function observeCurrentDataModel()
	local liveMode = inferLiveMode()
	if liveMode == "stop" then
		markStopped()
		return
	end

	local snapshot = getSnapshot()
	local activeSessionId = snapshot.pendingStartId
	if activeSessionId == 0 then
		if snapshot.activeSessionId > 0 and snapshot.mode == liveMode then
			activeSessionId = snapshot.activeSessionId
		else
			-- Manual Studio launches must still advance on the same global command
			-- timeline as start/stop requests, otherwise a stale stop_request_id from
			-- an older session can immediately kill the new session.
			activeSessionId = nextCommandId()
		end
	end

	PluginUtils.setSettings(MODE_KEY, liveMode)
	PluginUtils.setSettings(ACTIVE_SESSION_ID_KEY, activeSessionId)
	if snapshot.mode ~= liveMode or snapshot.activeSessionId ~= activeSessionId then
		PluginUtils.setSettings(CONTROL_READY_SESSION_ID_KEY, 0)
	end
	clearPendingStart()
	clearLastError()
end

local function beginStart(targetMode: Types.StartStopPlayMode): number
	assert(targetMode == "start_play" or targetMode == "run_server", "beginStart expects a running mode")
	local commandId = nextCommandId()
	PluginUtils.setSettings(PENDING_START_ID_KEY, commandId)
	writePendingStartMode(targetMode)
	writeLastError(0, nil)
	return commandId
end

local function recordTransitionError(commandId: number, errorMessage: string)
	local snapshot = getSnapshot()
	if snapshot.pendingStartId == commandId then
		clearPendingStart()
	end
	writeLastError(commandId, errorMessage)
end

local function getStopRequestId(): number
	return readNonNegativeNumber(STOP_REQUEST_ID_KEY)
end

local function markControlReadyForActiveSession()
	local snapshot = getSnapshot()
	if snapshot.activeSessionId > 0 then
		PluginUtils.setSettings(CONTROL_READY_SESSION_ID_KEY, snapshot.activeSessionId)
	end
end

local function requestStop(): number
	local commandId = nextCommandId()
	PluginUtils.setSettings(STOP_REQUEST_ID_KEY, commandId)
	clearLastError()
	return commandId
end

local function waitForStart(commandId: number, targetMode: Types.StartStopPlayMode, timeoutSeconds: number): TrackerSnapshot
	local deadline = os.clock() + timeoutSeconds
	while os.clock() < deadline do
		local snapshot = getSnapshot()
		if snapshot.lastErrorId == commandId and snapshot.lastError ~= nil then
			error(snapshot.lastError)
		end
		if snapshot.activeSessionId == commandId and snapshot.mode == targetMode and snapshot.controlReadySessionId == commandId then
			return snapshot
		end
		task.wait(WAIT_STEP_SECONDS)
	end

	local snapshot = getSnapshot()
	error(
		string.format(
			"Timed out waiting for Studio to enter %s. mode=%s active_session_id=%d control_ready_session_id=%d pending_start_id=%d",
			targetMode,
			snapshot.mode,
			snapshot.activeSessionId,
			snapshot.controlReadySessionId,
			snapshot.pendingStartId
		)
	)
end

local function waitForStop(timeoutSeconds: number): TrackerSnapshot
	local deadline = os.clock() + timeoutSeconds
	while os.clock() < deadline do
		local snapshot = getSnapshot()
		if snapshot.mode == "stop" and snapshot.activeSessionId == 0 and snapshot.pendingStartId == 0 then
			return snapshot
		end
		task.wait(WAIT_STEP_SECONDS)
	end

	local snapshot = getSnapshot()
	error(
		string.format(
			"Timed out waiting for Studio to stop. mode=%s active_session_id=%d pending_start_id=%d",
			snapshot.mode,
			snapshot.activeSessionId,
			snapshot.pendingStartId
		)
	)
end

return {
	beginStart = beginStart,
	getSnapshot = getSnapshot,
	getStopRequestId = getStopRequestId,
	inferLiveMode = inferLiveMode,
	markControlReadyForActiveSession = markControlReadyForActiveSession,
	markStopped = markStopped,
	normalizeEditState = normalizeEditState,
	observeCurrentDataModel = observeCurrentDataModel,
	recordTransitionError = recordTransitionError,
	requestStop = requestStop,
	waitForStart = waitForStart,
	waitForStop = waitForStop,
}
