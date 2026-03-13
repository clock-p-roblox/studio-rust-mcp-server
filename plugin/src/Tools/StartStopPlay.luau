local Main = script:FindFirstAncestor("MCPStudioPlugin")
local Types = require(Main.Types)
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local GameStopUtil = require(Main.Utils.GameStopUtil)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)

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

local function startMode(targetMode: Types.StartStopPlayMode, executor: () -> any, successMessage: string): string?
	ConsoleOutput.outputMessage = ""
	local snapshot = StudioModeTracker.getSnapshot()
	if snapshot.mode ~= "stop" or snapshot.pendingStartId ~= 0 then
		logTransitionStage(targetMode, "stop_before_start", string.format("current_mode=%s", snapshot.mode))
		stop()
	end

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
	return successMessage
end

local function handleStartStopPlay(startStopPlayArgs: Types.StartStopPlayArgs): string?
	if type(startStopPlayArgs.mode) ~= "string" then
		error("Missing mode in StartStopPlay")
	end

	if startStopPlayArgs.mode == "start_play" then
		return startMode("start_play", function()
			return StudioTestService:ExecutePlayModeAsync({})
		end, "Started play")
	elseif startStopPlayArgs.mode == "stop" then
		return stop()
	elseif startStopPlayArgs.mode == "run_server" then
		return startMode("run_server", function()
			return StudioTestService:ExecuteRunModeAsync({})
		end, "Ran server")
	end

	error("Invalid mode in StartStopPlay, must be start_play, stop, or run_server")
end

return handleStartStopPlay :: Types.ToolFunction
