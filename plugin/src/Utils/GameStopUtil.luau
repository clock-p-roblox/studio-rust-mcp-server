local Main = script:FindFirstAncestor("MCPStudioPlugin")
local StudioModeTracker = require(Main.Utils.StudioModeTracker)

local StudioTestService = game:GetService("StudioTestService")

local STOP_POLL_INTERVAL_SECONDS = 0.05

local function stopPlay()
	return StudioModeTracker.requestStop()
end

local function monitorForStopPlay()
	local snapshot = StudioModeTracker.getSnapshot()
	-- Ignore stop requests that were already visible before this session became active.
	local lastHandledStopRequestId = math.max(snapshot.activeSessionId, StudioModeTracker.getStopRequestId())
	StudioModeTracker.markControlReadyForActiveSession()
	while true do
		local stopRequestId = StudioModeTracker.getStopRequestId()
		if stopRequestId > lastHandledStopRequestId then
			lastHandledStopRequestId = stopRequestId
			local ok, errorMessage = pcall(function()
				StudioTestService:EndTest({})
			end)
			if not ok then
				print(string.format("[MCP Plugin] game_stop_ignored stop_request_id=%d error=%s", stopRequestId, tostring(errorMessage)))
			end
		end
		task.wait(STOP_POLL_INTERVAL_SECONDS)
	end
end

return { stopPlay = stopPlay, monitorForStopPlay = monitorForStopPlay }
