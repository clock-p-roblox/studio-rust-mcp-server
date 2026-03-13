local Main = script:FindFirstAncestor("MCPStudioPlugin")
local StudioModeTracker = require(Main.Utils.StudioModeTracker)
local Types = require(Main.Types)

local function handleGetStudioMode(): string?
	local snapshot = StudioModeTracker.getSnapshot()
	return snapshot.mode
end

return handleGetStudioMode :: Types.ToolFunction
