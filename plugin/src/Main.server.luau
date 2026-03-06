local Main = script:FindFirstAncestor("MCPStudioPlugin")
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local DataModelType = require(Main.Utils.DataModelType)
local GameStopUtil = require(Main.Utils.GameStopUtil)
local MockWebSocketService = require(Main.MockWebSocketService)
local PluginUtils = require(Main.Utils.PluginUtils)
local ToolDispatcher = require(Main.Utils.ToolDispatcher)
local Types = require(Main.Types)

local ChangeHistoryService = game:GetService("ChangeHistoryService")
local HttpService = game:GetService("HttpService")
local RunService = game:GetService("RunService")
local StudioService = game:GetService("StudioService")

local DEFAULT_URI = "http://localhost:44755"
local RECEIVE_ENDPOINT = "/request"
local SEND_ENDPOINT = "/response"
local PLUGIN_DISABLED_SETTING_KEY = "Disabled-MCP-plugin-setting"
local URI_SETTING_KEY = "MCP-plugin-uri"
local TOKEN_SETTING_KEY = "MCP-plugin-token"

PluginUtils.plugin = plugin

local datamodelType = DataModelType.getDataModelType()

if datamodelType == "Server" then
	task.spawn(GameStopUtil.monitorForStopPlay)
end

if RunService:IsRunning() then
	return
end

local connMessageOut = ConsoleOutput.startListener()
plugin.Unloading:Connect(function()
	connMessageOut:Disconnect()
end)

local old_warn = warn
local function log(...)
	if false then
		old_warn(...)
	end
end

local function trim(value: string): string
	return value:gsub("^%s+", ""):gsub("%s+$", "")
end

local function normalizeBaseUri(value: string): string
	local uri = trim(value)
	if uri == "" then
		return DEFAULT_URI
	end

	uri = uri:gsub("/+$", "")
	if not string.find(uri, "^https?://") then
		uri = "https://" .. uri
	end

	return uri
end

local function normalizeAuthHeader(value: string): string?
	local token = trim(value)
	if token == "" then
		return nil
	end

	if string.find(token, "%s") then
		return token
	end

	return "Bearer " .. token
end

local function loadConnectionConfig(): (string, string)
	local savedUri = PluginUtils.getSettings(URI_SETTING_KEY)
	local savedToken = PluginUtils.getSettings(TOKEN_SETTING_KEY)

	local uri = if type(savedUri) == "string" then savedUri else DEFAULT_URI
	local token = if type(savedToken) == "string" then savedToken else ""

	return normalizeBaseUri(uri), token
end

local function saveConnectionConfig(uri: string, token: string)
	PluginUtils.setSettings(URI_SETTING_KEY, normalizeBaseUri(uri))
	PluginUtils.setSettings(TOKEN_SETTING_KEY, token)
end

local function toolCallHandler(args: Types.ToolArgs): string
	local toolName = next(args)
	if not toolName then
		error("No tool name found in request")
	end

	local toolArgs = args[toolName]
	if not toolArgs or type(toolArgs) ~= "table" then
		error("Invalid tool args found for tool name: " .. toolName)
	end

	local recording = ChangeHistoryService:TryBeginRecording("StudioMCP")
	local success, response = pcall(ToolDispatcher.dispatchTool, toolName, toolArgs)
	if recording then
		ChangeHistoryService:FinishRecording(recording, Enum.FinishRecordingOperation.Commit)
	end

	if success then
		return response or ""
	end
	error("Error handling request: " .. tostring(response))
end

local function connectWebSocket(uri: string, token: string)
	local client = MockWebSocketService:CreateClient(uri)
	client:SetReceiveEndpoint(RECEIVE_ENDPOINT)
	client:SetSendEndpoint(SEND_ENDPOINT)

	local authHeader = normalizeAuthHeader(token)
	if authHeader ~= nil then
		client:SetHeaders({
			Authorization = authHeader,
		})
	end

	client.Opened:Once(function()
		log("[MCP] Connection opened")
	end)

	client.Closed:Once(function()
		log("[MCP] Connection closed")
	end)

	client.MessageReceived:Connect(function(message)
		log("[MCP] Message received")

		local body = HttpService:JSONDecode(message)
		assert(body and body.id and body.args, "Invalid message received")

		local id: string = body.id
		local responseSent = false
		local function sendResponseOnce(success: boolean, response: string)
			if not responseSent then
				log("[MCP] Sending response:", response)
				responseSent = true
				client:Send({
					id = id,
					success = success,
					response = response,
				})
			end
		end

		local args: Types.ToolArgs = body.args

		local success, response = pcall(toolCallHandler, args)
		sendResponseOnce(success, response)

		log("[MCP] Successfully handled request")
	end)

	return client
end

local function getButtonImage()
	local ok, response = pcall(function()
		return StudioService:GetClassIcon("PackageLink").Image
	end)
	return ok and response or "rbxasset://textures/ui/GuiImagePlaceholder.png"
end

local toolbar = plugin:CreateToolbar("MCP")
local toggleButton = toolbar:CreateButton("MCP", "Show MCP connection panel", getButtonImage())
toggleButton.ClickableWhenViewportHidden = true

local widgetInfo = DockWidgetPluginGuiInfo.new(
	Enum.InitialDockState.Right,
	false,
	false,
	340,
	190,
	280,
	160
)
local widget = plugin:CreateDockWidgetPluginGui("MCPConnectionPanel", widgetInfo)
widget.Title = "MCP"

local currentUri, currentToken = loadConnectionConfig()
local currentClient: MockWebSocketService.MockWebSocketClient?

local panel = Instance.new("Frame")
panel.Size = UDim2.fromScale(1, 1)
panel.BackgroundTransparency = 1
panel.Parent = widget

local layout = Instance.new("UIListLayout")
layout.Padding = UDim.new(0, 10)
layout.Parent = panel

local padding = Instance.new("UIPadding")
padding.PaddingLeft = UDim.new(0, 16)
padding.PaddingRight = UDim.new(0, 16)
padding.PaddingTop = UDim.new(0, 16)
padding.PaddingBottom = UDim.new(0, 16)
padding.Parent = panel

local function createLabel(text: string)
	local label = Instance.new("TextLabel")
	label.Size = UDim2.new(1, 0, 0, 18)
	label.BackgroundTransparency = 1
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.Font = Enum.Font.Code
	label.TextSize = 14
	label.Text = text
	label.Parent = panel
	return label
end

local function createInput(placeholder: string, value: string)
	local input = Instance.new("TextBox")
	input.Size = UDim2.new(1, 0, 0, 32)
	input.BackgroundTransparency = 0
	input.BorderSizePixel = 1
	input.ClearTextOnFocus = false
	input.TextXAlignment = Enum.TextXAlignment.Left
	input.Font = Enum.Font.Code
	input.TextSize = 14
	input.PlaceholderText = placeholder
	input.Text = value
	input.Parent = panel
	return input
end

createLabel("Base URL")
local uriInput = createInput("https://example.dev.clock-p.com", currentUri)
createLabel("Authorization")
local tokenInput = createInput("Bearer <token>", currentToken)
local statusLabel = createLabel("Status: Disconnected")

local buttonRow = Instance.new("Frame")
buttonRow.Size = UDim2.new(1, 0, 0, 32)
buttonRow.BackgroundTransparency = 1
buttonRow.Parent = panel

local buttonLayout = Instance.new("UIListLayout")
buttonLayout.FillDirection = Enum.FillDirection.Horizontal
buttonLayout.HorizontalAlignment = Enum.HorizontalAlignment.Right
buttonLayout.Padding = UDim.new(0, 8)
buttonLayout.Parent = buttonRow

local function createButton(text: string)
	local button = Instance.new("TextButton")
	button.Size = UDim2.new(0, 100, 1, 0)
	button.Font = Enum.Font.SourceSansSemibold
	button.TextSize = 16
	button.Text = text
	button.Parent = buttonRow
	return button
end

local saveButton = createButton("Save")
local connectButton = createButton("Connect")

local function persistCurrentInputs()
	currentUri = normalizeBaseUri(uriInput.Text)
	currentToken = tokenInput.Text
	uriInput.Text = currentUri
	saveConnectionConfig(currentUri, currentToken)
end

local function updateStatusLabel()
	if currentClient ~= nil then
		statusLabel.Text = "Status: Connected to " .. currentUri
		connectButton.Text = "Disconnect"
		toggleButton:SetActive(true)
	else
		statusLabel.Text = "Status: Disconnected"
		connectButton.Text = "Connect"
		toggleButton:SetActive(widget.Enabled)
	end
	uriInput.Text = currentUri
	tokenInput.Text = currentToken
end

local function disconnectCurrentClient()
	if currentClient ~= nil then
		currentClient:Close()
		currentClient = nil
		print("The MCP Studio plugin is stopped.")
		PluginUtils.setSettings(PLUGIN_DISABLED_SETTING_KEY, true)
		updateStatusLabel()
	end
end

local function connectCurrentClient()
	persistCurrentInputs()
	if currentClient ~= nil then
		disconnectCurrentClient()
	end
	currentClient = connectWebSocket(currentUri, currentToken)
	print("The MCP Studio plugin is ready for prompts.")
	PluginUtils.setSettings(PLUGIN_DISABLED_SETTING_KEY, false)
	updateStatusLabel()
end

widget:GetPropertyChangedSignal("Enabled"):Connect(function()
	if currentClient == nil then
		toggleButton:SetActive(widget.Enabled)
	end
end)

uriInput.FocusLost:Connect(function()
	persistCurrentInputs()
	updateStatusLabel()
end)

tokenInput.FocusLost:Connect(function()
	persistCurrentInputs()
	updateStatusLabel()
end)

saveButton.MouseButton1Click:Connect(function()
	persistCurrentInputs()
	updateStatusLabel()
end)

connectButton.MouseButton1Click:Connect(function()
	if currentClient == nil then
		connectCurrentClient()
	else
		disconnectCurrentClient()
	end
end)

if not plugin:GetSetting(PLUGIN_DISABLED_SETTING_KEY) then
	connectCurrentClient()
	widget.Enabled = false
end

updateStatusLabel()

toggleButton.Click:Connect(function()
	widget.Enabled = not widget.Enabled
	updateStatusLabel()
end)

plugin.Unloading:Connect(function()
	disconnectCurrentClient()
end)
