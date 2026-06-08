local Main = script:FindFirstAncestor("MCPStudioPlugin")
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local StudioSessionControl = require(Main.Utils.StudioSessionControl)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)
local Types = require(Main.Types)

local HttpService = game:GetService("HttpService")
local ServerScriptService = game:GetService("ServerScriptService")
local StudioTestService = game:GetService("StudioTestService")

local TEST_SCRIPT_NAME = "AssistantTestScript"
local DEFAULT_TEST_SCRIPT_TIMEOUT_SECONDS = 30
local MAX_TEST_SCRIPT_TIMEOUT_SECONDS = 90
local STUDIO_TEST_SERVICE_SETTLE_TIMEOUT_SECONDS = 30
local STUDIO_TEST_SERVICE_SETTLE_INTERVAL_SECONDS = 0.5

local function logRunScriptStage(stage: string, detail: string?)
	local message = string.format("[MCP Plugin] run_script_in_play_mode stage=%s", stage)
	if detail ~= nil and detail ~= "" then
		message ..= " " .. detail
	end
	print(message)
end

local function isPreviousTestStillInProgress(errorMessage: string): boolean
	local lowered = string.lower(errorMessage)
	return string.find(lowered, "previous", 1, true) ~= nil
		and (
			string.find(lowered, "still in progress", 1, true) ~= nil
			or string.find(lowered, "has not been completed", 1, true) ~= nil
		)
end

local function assertCanStartOneShotTest()
	local currentMode = StudioSessionControl.getStudioMode()
	local snapshot = StudioModeTracker.getSnapshot()
	if currentMode ~= "stop" or snapshot.activeSessionId > 0 or snapshot.pendingStartId > 0 then
		error(
			string.format(
				"run_script_in_play_mode requires Studio to be stopped before starting. current_mode=%s active_session_id=%d pending_start_id=%d. Call start_stop_play(stop) once and inspect Studio logs before retrying.",
				currentMode,
				snapshot.activeSessionId,
				snapshot.pendingStartId
			)
		)
	end
end

local function buildTestRunnerSource(userCode: string, timeout: number): string
	local timeoutStr = tostring(timeout)
	return [[
local StudioTestService = game:GetService("StudioTestService")
local LogService = game:GetService("LogService")
local RunService = game:GetService("RunService")

if not RunService:IsRunning() then
	return
end

local capturedLogs = {}
local logConnection = nil

local MESSAGE_TYPE_MAP = {
	[Enum.MessageType.MessageOutput] = "output",
	[Enum.MessageType.MessageInfo] = "info",
	[Enum.MessageType.MessageWarning] = "warning",
	[Enum.MessageType.MessageError] = "error",
}

logConnection = LogService.MessageOut:Connect(function(message, messageType)
	if string.sub(message, 1, 5) == "[MCP]" then return end
	table.insert(capturedLogs, {
		level = MESSAGE_TYPE_MAP[messageType] or "output",
		message = message,
		ts = os.clock(),
	})
end)

local startTime = os.clock()
local isTimeout = false
local ended = false
local ok = false
local result = nil

local function endTest()
	if ended then
		return
	end
	ended = true

	local duration = os.clock() - startTime
	task.wait(0.1)

	if logConnection then logConnection:Disconnect() end

	local errors = {}
	for _, log in ipairs(capturedLogs) do
		if log.level == "error" or log.level == "warning" then
			table.insert(errors, log)
		end
	end

	StudioTestService:EndTest({
		success = ok,
		value = if ok then tostring(result) else nil,
		error = if not ok then tostring(result) else nil,
		logs = capturedLogs,
		errors = errors,
		duration = duration,
		isTimeout = isTimeout,
	})
end


task.spawn(function()
	wait(]] .. timeoutStr .. [[)
	if ended then
		return
	end
	isTimeout = true
	ok = false
	result = "Timed out waiting for test script to finish"
	endTest()
end)

ok, result = pcall(function()
]] .. userCode .. [[

end)

endTest()
]]
end

local function removeTestScript()
	local testScript = ServerScriptService:FindFirstChild(TEST_SCRIPT_NAME)
	if testScript then
		testScript:Destroy()
	end
end

local function injectTestScript(args: Types.RunScriptInPlayModeArgs)
	removeTestScript()

	local source = buildTestRunnerSource(args.code, args.timeout or DEFAULT_TEST_SCRIPT_TIMEOUT_SECONDS)
	local testScript = Instance.new("Script")
	testScript.Name = TEST_SCRIPT_NAME
	testScript.Parent = ServerScriptService
	testScript.Source = source
end

local function executeRunScriptAttempt(args: Types.RunScriptInPlayModeArgs): (boolean, any)
	injectTestScript(args)

	ConsoleOutput.outputMessage = ""
	local commandId = StudioModeTracker.beginStart(args.mode)
	local success, result = pcall(function()
		if args.mode == "start_play" then
			return StudioTestService:ExecutePlayModeAsync({})
		else
			return StudioTestService:ExecuteRunModeAsync({})
		end
	end)

	if success then
		StudioModeTracker.markStopped()
	else
		StudioModeTracker.recordTransitionError(commandId, tostring(result))
	end
	removeTestScript()

	if success then
		return true, result
	end

	return false, tostring(result)
end

local function waitForStudioTestServiceSettle(attempt: number, errorMessage: string)
	local snapshot = StudioModeTracker.getSnapshot()
	logRunScriptStage(
		"studio_test_service_settling",
		string.format(
			"attempt=%d mode=%s active_session_id=%d pending_start_id=%d error=%s",
			attempt,
			snapshot.mode,
			snapshot.activeSessionId,
			snapshot.pendingStartId,
			errorMessage
		)
	)
	task.wait(STUDIO_TEST_SERVICE_SETTLE_INTERVAL_SECONDS)
end

local function runScriptInPlayMode(args: Types.RunScriptInPlayModeArgs): string?
	local deadline = os.clock() + STUDIO_TEST_SERVICE_SETTLE_TIMEOUT_SECONDS
	local attempt = 0
	local lastError = ""

	while os.clock() < deadline do
		assertCanStartOneShotTest()
		attempt += 1
		local success, result = executeRunScriptAttempt(args)
		if success then
			return HttpService:JSONEncode(result)
		end

		lastError = tostring(result)
		if not isPreviousTestStillInProgress(lastError) then
			error("Failed to run script in play mode: " .. lastError)
		end

		waitForStudioTestServiceSettle(attempt, lastError)
	end

	error(
		string.format(
			"Failed to run script in play mode: previous Studio test is still in progress after waiting %d seconds. Last error: %s",
			STUDIO_TEST_SERVICE_SETTLE_TIMEOUT_SECONDS,
			lastError
		)
	)
end

local function handleRunScriptInPlayMode(runScriptInPlayModeArgs: Types.RunScriptInPlayModeArgs): string?
	if type(runScriptInPlayModeArgs.code) ~= "string" or runScriptInPlayModeArgs.code == "" then
		error("Missing code in RunScriptInPlayMode")
	end

	if runScriptInPlayModeArgs.mode ~= "start_play" and runScriptInPlayModeArgs.mode ~= "run_server" then
		error("Invalid mode in RunScriptInPlayMode, must be start_play or run_server")
	end

	local timeout = runScriptInPlayModeArgs.timeout or DEFAULT_TEST_SCRIPT_TIMEOUT_SECONDS
	if type(timeout) ~= "number" or timeout <= 0 then
		error("Timeout must be a number greater than 0 in RunScriptInPlayMode")
	end
	if timeout > MAX_TEST_SCRIPT_TIMEOUT_SECONDS then
		error(
			string.format(
				"Timeout must be <= %d seconds in RunScriptInPlayMode so the one-shot test stays within the Studio control response budget",
				MAX_TEST_SCRIPT_TIMEOUT_SECONDS
			)
		)
	end
	runScriptInPlayModeArgs.timeout = timeout

	return runScriptInPlayMode(runScriptInPlayModeArgs)
end

return handleRunScriptInPlayMode :: Types.ToolFunction
