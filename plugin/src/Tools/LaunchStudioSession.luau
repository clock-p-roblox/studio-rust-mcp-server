local Main = script:FindFirstAncestor("MCPStudioPlugin")
local StudioSessionControl = require(Main.Utils.StudioSessionControl)
local Types = require(Main.Types)

return StudioSessionControl.handleLaunchStudioSession :: Types.ToolFunction
