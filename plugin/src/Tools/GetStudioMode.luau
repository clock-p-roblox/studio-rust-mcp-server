local Main = script:FindFirstAncestor("MCPStudioPlugin")
local StudioSessionControl = require(Main.Utils.StudioSessionControl)
local Types = require(Main.Types)

local function handleGetStudioMode(): string?
	return StudioSessionControl.getStudioMode()
end

return handleGetStudioMode :: Types.ToolFunction
