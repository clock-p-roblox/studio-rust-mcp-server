local Main = script:FindFirstAncestor("MCPStudioPlugin")
local Types = require(Main.Types)
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local GameStopUtil = require(Main.Utils.GameStopUtil)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)

local HttpService = game:GetService("HttpService")
local StudioTestService = game:GetService("StudioTestService")

local START_TIMEOUT_SECONDS = 15
local STOP_TIMEOUT_SECONDS = 15

local function logTransitionStage(mode: Types.StartStopPlayMode, stage: string, detail: string?)
	local message = string.format("[MCP Plugin] studio_transition mode=%s stage=%s", mode, stage)
	if detail ~= nil and detail ~= "" then
		message ..= " " .. detail
	end
	print(message)
end

local function stop(): string?
	ConsoleOutput.outputMessage = ""
	local snapshot = StudioModeTracker.getSnapshot()
	if snapshot.mode == "stop" and snapshot.pendingStartId == 0 then
		logTransitionStage("stop", "noop")
		return "Stopped"
	end

	local stopRequestId = GameStopUtil.stopPlay()
	logTransitionStage("stop", "requested", string.format("current_mode=%s stop_request_id=%d", snapshot.mode, stopRequestId))
	StudioModeTracker.waitForStop(STOP_TIMEOUT_SECONDS)
	logTransitionStage("stop", "observed")
	return "Stopped"
end

local function ensureStoppedForStart(targetMode: Types.StartStopPlayMode): boolean
	local snapshot = StudioModeTracker.getSnapshot()
	if snapshot.mode ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage(targetMode, "stop_before_start", string.format("current_mode=%s", snapshot.mode))
		stop()
		return true
	end

	return false
end

local function startMode(targetMode: Types.StartStopPlayMode, executor: () -> any, successMessage: string): (string?, boolean)
	ConsoleOutput.outputMessage = ""
	local restartApplied = ensureStoppedForStart(targetMode)

	local commandId = StudioModeTracker.beginStart(targetMode)
	logTransitionStage(targetMode, "requested", string.format("command_id=%d", commandId))

	task.spawn(function()
		local success, result = pcall(executor)
		if success then
			logTransitionStage(targetMode, "execute_returned", string.format("command_id=%d", commandId))
			StudioModeTracker.markStopped()
			return
		end

		local errorMessage = tostring(result)
		logTransitionStage(targetMode, "execute_failed", string.format("command_id=%d error=%s", commandId, errorMessage))
		StudioModeTracker.recordTransitionError(commandId, errorMessage)
	end)

	StudioModeTracker.waitForStart(commandId, targetMode, START_TIMEOUT_SECONDS)
	logTransitionStage(targetMode, "observed", string.format("command_id=%d", commandId))
	return successMessage, restartApplied
end

local function handleStartStopPlay(startStopPlayArgs: Types.StartStopPlayArgs): string?
	if type(startStopPlayArgs.mode) ~= "string" then
		error("Missing mode in StartStopPlay")
	end

	if startStopPlayArgs.mode == "start_play" then
		local message = startMode("start_play", function()
			return StudioTestService:ExecutePlayModeAsync({})
		end, "Started play")
		return message
	elseif startStopPlayArgs.mode == "stop" then
		return stop()
	elseif startStopPlayArgs.mode == "run_server" then
		local message = startMode("run_server", function()
			return StudioTestService:ExecuteRunModeAsync({})
		end, "Ran server")
		return message
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
	local actions = {}

	local restartApplied = false
	if requestedMode == "start_play" then
		_, restartApplied = startMode("start_play", function()
			return StudioTestService:ExecutePlayModeAsync({})
		end, "Started play")
	else
		_, restartApplied = startMode("run_server", function()
			return StudioTestService:ExecuteRunModeAsync({})
		end, "Ran server")
	end

	if restartApplied then
		table.insert(actions, "stop")
	end
	table.insert(actions, requestedMode)

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
