local Main = script:FindFirstAncestor("MCPStudioPlugin")
local StudioSessionControl = require(Main.Utils.StudioSessionControl)
local Types = require(Main.Types)

local function handleGetStudioSessionState(): string?
	return StudioSessionControl.getStudioSessionStateJson()
end

return handleGetStudioSessionState :: Types.ToolFunction
