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

local DEFAULT_URI = "https://localhost"
local URI_PLACEHOLDER = "game1-studio-mcp-your-user.dev.clock-p.com"
local RECEIVE_ENDPOINT = "/request"
local SEND_ENDPOINT = "/response"
local PLUGIN_DISABLED_SETTING_KEY = "Disabled-MCP-plugin-setting"
local URI_SETTING_KEY = "MCP-plugin-uri"
local TOKEN_SETTING_KEY = "MCP-plugin-token"
local LOG_PREFIX = "[MCP Plugin]"

local STATUS_THEME = {
	info = {
		label = "Connecting",
		accent = Color3.fromRGB(72, 163, 255),
		background = Color3.fromRGB(21, 33, 48),
	},
	connected = {
		label = "Connected",
		accent = Color3.fromRGB(66, 188, 120),
		background = Color3.fromRGB(20, 39, 31),
	},
	warning = {
		label = "Disconnected",
		accent = Color3.fromRGB(255, 196, 87),
		background = Color3.fromRGB(48, 38, 18),
	},
	error = {
		label = "Error",
		accent = Color3.fromRGB(255, 107, 107),
		background = Color3.fromRGB(53, 23, 28),
	},
}

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

local function trim(value: string): string
	return value:gsub("^%s+", ""):gsub("%s+$", "")
end

local function summarizeText(value: string?): string
	if value == nil or value == "" then
		return ""
	end

	if #value <= 180 then
		return value
	end

	return string.sub(value, 1, 180) .. "..."
end

local function logInfo(message: string)
	print(LOG_PREFIX, message)
end

local function logWarn(message: string)
	warn(LOG_PREFIX, message)
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
	local normalizedUri = normalizeBaseUri(uri)
	PluginUtils.setSettings(URI_SETTING_KEY, normalizedUri)
	PluginUtils.setSettings(TOKEN_SETTING_KEY, token)
	logInfo("Saved connection settings for " .. normalizedUri)
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

	logInfo("Running tool " .. toolName)
	local recording = ChangeHistoryService:TryBeginRecording("StudioMCP")
	local success, response = pcall(ToolDispatcher.dispatchTool, toolName, toolArgs)
	if recording then
		ChangeHistoryService:FinishRecording(recording, Enum.FinishRecordingOperation.Commit)
	end

	if success then
		logInfo("Tool " .. toolName .. " succeeded: " .. summarizeText(response or ""))
		return response or ""
	end

	logWarn("Tool " .. toolName .. " failed: " .. tostring(response))
	error("Error handling request: " .. tostring(response))
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
	420,
	320,
	360,
	260
)
local widget = plugin:CreateDockWidgetPluginGui("MCPConnectionPanel", widgetInfo)
widget.Title = "Studio MCP"

local currentUri, currentToken = loadConnectionConfig()
local currentClient: MockWebSocketService.MockWebSocketClient?
local readyAnnounced = false
local currentStatusKind = "warning"
local currentStatusMessage = "Not connected"
local currentStatusDetail = "Click Connect after saving a Studio MCP domain and token."

local panel = Instance.new("Frame")
panel.Size = UDim2.fromScale(1, 1)
panel.BackgroundColor3 = Color3.fromRGB(15, 20, 26)
panel.BorderSizePixel = 0
panel.Parent = widget

local panelCorner = Instance.new("UICorner")
panelCorner.CornerRadius = UDim.new(0, 12)
panelCorner.Parent = panel

local layout = Instance.new("UIListLayout")
layout.Padding = UDim.new(0, 12)
layout.Parent = panel

local padding = Instance.new("UIPadding")
padding.PaddingLeft = UDim.new(0, 16)
padding.PaddingRight = UDim.new(0, 16)
padding.PaddingTop = UDim.new(0, 16)
padding.PaddingBottom = UDim.new(0, 16)
padding.Parent = panel

local header = Instance.new("Frame")
header.Size = UDim2.new(1, 0, 0, 46)
header.BackgroundTransparency = 1
header.Parent = panel

local titleLabel = Instance.new("TextLabel")
titleLabel.Size = UDim2.new(1, 0, 0, 22)
titleLabel.BackgroundTransparency = 1
titleLabel.TextXAlignment = Enum.TextXAlignment.Left
titleLabel.Font = Enum.Font.GothamBold
titleLabel.TextSize = 18
titleLabel.TextColor3 = Color3.fromRGB(243, 247, 250)
titleLabel.Text = "Roblox Studio MCP"
titleLabel.Parent = header

local subtitleLabel = Instance.new("TextLabel")
subtitleLabel.Position = UDim2.new(0, 0, 0, 24)
subtitleLabel.Size = UDim2.new(1, 0, 0, 22)
subtitleLabel.BackgroundTransparency = 1
subtitleLabel.TextXAlignment = Enum.TextXAlignment.Left
subtitleLabel.Font = Enum.Font.Gotham
subtitleLabel.TextSize = 12
subtitleLabel.TextColor3 = Color3.fromRGB(156, 169, 181)
subtitleLabel.TextWrapped = true
subtitleLabel.Text = "Bare hostnames use HTTPS on port 443. Successes and failures are logged to the Studio console."
subtitleLabel.Parent = header

local function createCard(height: number)
	local card = Instance.new("Frame")
	card.Size = UDim2.new(1, 0, 0, height)
	card.BackgroundColor3 = Color3.fromRGB(24, 31, 39)
	card.BorderSizePixel = 0
	card.Parent = panel

	local corner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 10)
	corner.Parent = card

	local stroke = Instance.new("UIStroke")
	stroke.Color = Color3.fromRGB(44, 55, 69)
	stroke.Thickness = 1
	stroke.Parent = card

	return card
end

local function createCardHeader(parent: Instance, title: string, detail: string)
	local label = Instance.new("TextLabel")
	label.Position = UDim2.new(0, 14, 0, 10)
	label.Size = UDim2.new(1, -28, 0, 18)
	label.BackgroundTransparency = 1
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.Font = Enum.Font.GothamSemibold
	label.TextSize = 14
	label.TextColor3 = Color3.fromRGB(237, 242, 247)
	label.Text = title
	label.Parent = parent

	local detailLabel = Instance.new("TextLabel")
	detailLabel.Position = UDim2.new(0, 14, 0, 30)
	detailLabel.Size = UDim2.new(1, -28, 0, 28)
	detailLabel.BackgroundTransparency = 1
	detailLabel.TextXAlignment = Enum.TextXAlignment.Left
	detailLabel.TextYAlignment = Enum.TextYAlignment.Top
	detailLabel.Font = Enum.Font.Gotham
	detailLabel.TextSize = 12
	detailLabel.TextColor3 = Color3.fromRGB(156, 169, 181)
	detailLabel.TextWrapped = true
	detailLabel.Text = detail
	detailLabel.Parent = parent
end

local function createInput(parent: Instance, top: number, placeholder: string, value: string, hidden: boolean?)
	local input = Instance.new("TextBox")
	input.Position = UDim2.new(0, 14, 0, top)
	input.Size = UDim2.new(1, -28, 0, 34)
	input.BackgroundColor3 = Color3.fromRGB(18, 24, 31)
	input.BorderSizePixel = 0
	input.ClearTextOnFocus = false
	input.TextXAlignment = Enum.TextXAlignment.Left
	input.Font = Enum.Font.Code
	input.TextSize = 13
	input.TextColor3 = Color3.fromRGB(240, 245, 250)
	input.PlaceholderColor3 = Color3.fromRGB(111, 125, 140)
	input.PlaceholderText = placeholder
	input.Text = value
	input.TextEditable = true
	input.Parent = parent
	input.TextWrapped = false
	if hidden then
		input.Text = value
	end

	local corner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 8)
	corner.Parent = input

	local stroke = Instance.new("UIStroke")
	stroke.Color = Color3.fromRGB(55, 68, 84)
	stroke.Thickness = 1
	stroke.Parent = input

	return input
end

local connectionCard = createCard(102)
createCardHeader(connectionCard, "Base URL", "Use the public Studio MCP domain. If you omit a scheme, the plugin assumes HTTPS on port 443.")
local uriInput = createInput(connectionCard, 60, URI_PLACEHOLDER, currentUri)

local authCard = createCard(102)
createCardHeader(authCard, "Authorization", "Paste either the raw feishu token or the full Bearer header. The token is stored in the plugin settings.")
local tokenInput = createInput(authCard, 60, "feishu token or Bearer <token>", currentToken, true)

local statusCard = createCard(94)

local statusPill = Instance.new("TextLabel")
statusPill.Position = UDim2.new(0, 14, 0, 12)
statusPill.Size = UDim2.new(0, 110, 0, 24)
statusPill.BackgroundColor3 = Color3.fromRGB(48, 38, 18)
statusPill.BorderSizePixel = 0
statusPill.Font = Enum.Font.GothamSemibold
statusPill.TextSize = 12
statusPill.TextColor3 = Color3.fromRGB(255, 196, 87)
statusPill.Text = "Disconnected"
statusPill.Parent = statusCard

local statusPillCorner = Instance.new("UICorner")
statusPillCorner.CornerRadius = UDim.new(0, 999)
statusPillCorner.Parent = statusPill

local statusTitle = Instance.new("TextLabel")
statusTitle.Position = UDim2.new(0, 14, 0, 44)
statusTitle.Size = UDim2.new(1, -28, 0, 18)
statusTitle.BackgroundTransparency = 1
statusTitle.TextXAlignment = Enum.TextXAlignment.Left
statusTitle.Font = Enum.Font.GothamSemibold
statusTitle.TextSize = 14
statusTitle.TextColor3 = Color3.fromRGB(243, 247, 250)
statusTitle.Text = currentStatusMessage
statusTitle.Parent = statusCard

local statusDetail = Instance.new("TextLabel")
statusDetail.Position = UDim2.new(0, 14, 0, 64)
statusDetail.Size = UDim2.new(1, -28, 0, 22)
statusDetail.BackgroundTransparency = 1
statusDetail.TextXAlignment = Enum.TextXAlignment.Left
statusDetail.TextYAlignment = Enum.TextYAlignment.Top
statusDetail.Font = Enum.Font.Gotham
statusDetail.TextSize = 12
statusDetail.TextColor3 = Color3.fromRGB(156, 169, 181)
statusDetail.TextWrapped = true
statusDetail.Text = currentStatusDetail
statusDetail.Parent = statusCard

local buttonRow = Instance.new("Frame")
buttonRow.Size = UDim2.new(1, 0, 0, 38)
buttonRow.BackgroundTransparency = 1
buttonRow.Parent = panel

local buttonLayout = Instance.new("UIListLayout")
buttonLayout.FillDirection = Enum.FillDirection.Horizontal
buttonLayout.HorizontalAlignment = Enum.HorizontalAlignment.Right
buttonLayout.Padding = UDim.new(0, 8)
buttonLayout.Parent = buttonRow

local function createButton(text: string, background: Color3, foreground: Color3)
	local button = Instance.new("TextButton")
	button.Size = UDim2.new(0, 116, 1, 0)
	button.AutoButtonColor = true
	button.BackgroundColor3 = background
	button.BorderSizePixel = 0
	button.Font = Enum.Font.GothamSemibold
	button.TextSize = 14
	button.TextColor3 = foreground
	button.Text = text
	button.Parent = buttonRow

	local corner = Instance.new("UICorner")
	corner.CornerRadius = UDim.new(0, 9)
	corner.Parent = button

	return button
end

local resetButton = createButton("Reset", Color3.fromRGB(52, 63, 76), Color3.fromRGB(230, 236, 241))
local saveButton = createButton("Save", Color3.fromRGB(44, 93, 153), Color3.fromRGB(245, 248, 252))
local connectButton = createButton("Connect", Color3.fromRGB(66, 188, 120), Color3.fromRGB(18, 24, 31))

local function updateStatusViews()
	local theme = STATUS_THEME[currentStatusKind] or STATUS_THEME.warning
	statusPill.BackgroundColor3 = theme.background
	statusPill.TextColor3 = theme.accent
	statusPill.Text = theme.label
	statusTitle.Text = currentStatusMessage
	statusDetail.Text = currentStatusDetail
	if currentClient ~= nil then
		connectButton.Text = "Disconnect"
		connectButton.BackgroundColor3 = Color3.fromRGB(255, 107, 107)
		connectButton.TextColor3 = Color3.fromRGB(26, 17, 18)
		toggleButton:SetActive(true)
	else
		connectButton.Text = "Connect"
		connectButton.BackgroundColor3 = Color3.fromRGB(66, 188, 120)
		connectButton.TextColor3 = Color3.fromRGB(18, 24, 31)
		toggleButton:SetActive(widget.Enabled)
	end
	uriInput.Text = currentUri
	tokenInput.Text = currentToken
end

local function setStatus(kind: string, message: string, detail: string?)
	currentStatusKind = kind
	currentStatusMessage = message
	if detail ~= nil then
		currentStatusDetail = detail
	end
	updateStatusViews()
end

local function persistCurrentInputs()
	currentUri = normalizeBaseUri(uriInput.Text)
	currentToken = tokenInput.Text
	uriInput.Text = currentUri
	saveConnectionConfig(currentUri, currentToken)
	setStatus(currentStatusKind, currentStatusMessage, "Saved settings for " .. currentUri)
end

local function isCurrentClient(client: MockWebSocketService.MockWebSocketClient): boolean
	return currentClient == client
end

local function handleClientStatus(kind: string, message: string)
	if kind == "error" then
		logWarn(message)
		setStatus("error", "Connection error", message)
		readyAnnounced = false
		return
	end

	if kind == "connected" then
		if not readyAnnounced then
			logInfo("The MCP Studio plugin is ready for prompts.")
			readyAnnounced = true
		end
		logInfo(message)
		setStatus("connected", "Ready for prompts", message)
		return
	end

	logInfo(message)
	setStatus("info", "Connection active", message)
	if kind == "warning" then
		readyAnnounced = false
	end
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
		logInfo("Using Authorization header for " .. uri)
	else
		logWarn("No Authorization header configured for " .. uri)
	end

	client.Opened:Connect(function()
		if isCurrentClient(client) then
			logInfo("Started long polling against " .. uri)
			setStatus("info", "Connecting", "Waiting for a successful response from " .. uri)
		end
	end)

	client.Closed:Connect(function()
		if isCurrentClient(client) then
			logWarn("Connection closed for " .. uri)
			setStatus("warning", "Disconnected", "Long polling stopped for " .. uri)
			readyAnnounced = false
		end
	end)

	client.ConnectionStatusChanged:Connect(function(kind, message)
		if isCurrentClient(client) then
			handleClientStatus(kind, message)
		end
	end)

	client.MessageReceived:Connect(function(message)
		if not isCurrentClient(client) then
			return
		end

		local decodeOk, body = pcall(function()
			return HttpService:JSONDecode(message)
		end)
		if not decodeOk then
			local decodeError = "Failed to decode request payload: " .. tostring(body)
			logWarn(decodeError)
			setStatus("error", "Invalid request payload", decodeError)
			return
		end

		if type(body) ~= "table" or body.id == nil or body.args == nil then
			local invalidError = "Received malformed request payload from " .. uri
			logWarn(invalidError)
			setStatus("error", "Malformed request payload", invalidError)
			return
		end

		local id = tostring(body.id)
		local responseSent = false
		local function sendResponseOnce(success: boolean, response: string)
			if responseSent then
				return
			end

			responseSent = true
			client:Send({
				id = id,
				success = success,
				response = response,
			})
		end

		local args = body.args :: Types.ToolArgs
		local toolName = next(args)
		logInfo("Received tool request " .. tostring(toolName) .. " (id=" .. id .. ")")

		local success, response = pcall(toolCallHandler, args)
		if success then
			sendResponseOnce(true, response)
			setStatus("connected", "Ready for prompts", "Handled tool " .. tostring(toolName))
		else
			local errorMessage = tostring(response)
			sendResponseOnce(false, errorMessage)
			setStatus("error", "Tool failed", errorMessage)
		end
	end)

	return client
end

local function disconnectCurrentClient()
	if currentClient ~= nil then
		currentClient:Close()
		currentClient = nil
		readyAnnounced = false
		PluginUtils.setSettings(PLUGIN_DISABLED_SETTING_KEY, true)
		logInfo("The MCP Studio plugin is stopped.")
		setStatus("warning", "Disconnected", "Stopped polling " .. currentUri)
	end
end

local function connectCurrentClient()
	persistCurrentInputs()
	if currentClient ~= nil then
		disconnectCurrentClient()
	end

	readyAnnounced = false
	logInfo("Connecting to " .. currentUri)
	currentClient = connectWebSocket(currentUri, currentToken)
	PluginUtils.setSettings(PLUGIN_DISABLED_SETTING_KEY, false)
	setStatus("info", "Connecting", "Opening a long-poll connection to " .. currentUri)
end

widget:GetPropertyChangedSignal("Enabled"):Connect(function()
	if currentClient == nil then
		toggleButton:SetActive(widget.Enabled)
	end
end)

uriInput.FocusLost:Connect(function()
	currentUri = normalizeBaseUri(uriInput.Text)
	uriInput.Text = currentUri
	updateStatusViews()
end)

tokenInput.FocusLost:Connect(function()
	currentToken = tokenInput.Text
	updateStatusViews()
end)

saveButton.MouseButton1Click:Connect(function()
	persistCurrentInputs()
end)

resetButton.MouseButton1Click:Connect(function()
	currentUri = DEFAULT_URI
	currentToken = ""
	uriInput.Text = currentUri
	tokenInput.Text = currentToken
	saveConnectionConfig(currentUri, currentToken)
	setStatus("warning", "Reset settings", "Reset the connection form to the HTTPS/443 default.")
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

updateStatusViews()

toggleButton.Click:Connect(function()
	widget.Enabled = not widget.Enabled
	updateStatusViews()
end)

plugin.Unloading:Connect(function()
	disconnectCurrentClient()
end)
