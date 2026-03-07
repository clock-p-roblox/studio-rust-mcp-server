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

local DEFAULT_HELPER_PORT = "44750"
local HELPER_PORT_SETTING_KEY = "MCP-helper-port"
local AUTO_CONNECT_SETTING_KEY = "MCP-auto-connect"
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

local function normalizeHelperPort(value: string): string
	local digits = trim(value):gsub("%D", "")
	if digits == "" then
		return DEFAULT_HELPER_PORT
	end
	return digits
end

local function helperBaseUrl(port: string): string
	return "http://127.0.0.1:" .. normalizeHelperPort(port)
end

local function loadConnectionConfig(): (string, boolean)
	local savedPort = PluginUtils.getSettings(HELPER_PORT_SETTING_KEY)
	local savedAutoConnect = PluginUtils.getSettings(AUTO_CONNECT_SETTING_KEY)
	local helperPort = if type(savedPort) == "string" then savedPort else DEFAULT_HELPER_PORT
	local autoConnect = if type(savedAutoConnect) == "boolean" then savedAutoConnect else false
	return normalizeHelperPort(helperPort), autoConnect
end

local function saveConnectionConfig(helperPort: string, autoConnect: boolean)
	local normalizedPort = normalizeHelperPort(helperPort)
	PluginUtils.setSettings(HELPER_PORT_SETTING_KEY, normalizedPort)
	PluginUtils.setSettings(AUTO_CONNECT_SETTING_KEY, autoConnect)
	logInfo("Saved helper settings for port " .. normalizedPort)
	return normalizedPort
end

local function requestJson(baseUrl: string, method: string, path: string, body: any?)
	local request: { [string]: any } = {
		Url = baseUrl .. path,
		Method = method,
		Headers = {
			["Content-Type"] = "application/json",
		},
	}

	if body ~= nil then
		request.Body = HttpService:JSONEncode(body)
	end

	local ok, result = pcall(function()
		return HttpService:RequestAsync(request)
	end)
	if not ok then
		error("Request to helper failed: " .. tostring(result))
	end

	local response = result
	if not response.Success then
		error(string.format("Helper request %s %s failed with status %d: %s", method, path, response.StatusCode, summarizeText(response.Body)))
	end

	if response.Body == "" then
		return {}
	end

	local decodeOk, decoded = pcall(function()
		return HttpService:JSONDecode(response.Body)
	end)
	if not decodeOk then
		error("Failed to decode helper JSON response: " .. tostring(decoded))
	end
	return decoded
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
local toggleButton = toolbar:CreateButton("MCP", "Show MCP helper connection panel", getButtonImage())
toggleButton.ClickableWhenViewportHidden = true

local widgetInfo = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Right, false, false, 420, 300, 360, 240)
local widget = plugin:CreateDockWidgetPluginGui("MCPConnectionPanel", widgetInfo)
widget.Title = "Studio MCP"

local currentHelperPort, currentAutoConnect = loadConnectionConfig()
local currentClient: MockWebSocketService.MockWebSocketClient?
local currentInstanceId: string? = nil
local currentHelperBaseUrl: string? = nil
local readyAnnounced = false
local staleReconnectInFlight = false
local currentStatusKind = "warning"
local currentStatusMessage = "Not connected"
local currentStatusDetail = "Connect to the local helper. The helper derives place URLs automatically from the current PlaceId."
local connectCurrentClient = nil

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
subtitleLabel.Text = "Only the local helper port is configurable. The helper owns tokens, derives public MCP domains, and routes requests for this PlaceId."
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

local function createInput(parent: Instance, top: number, placeholder: string, value: string)
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
createCardHeader(connectionCard, "Helper Port", "The helper must be running locally. The plugin asks the helper to register this PlaceId and receive tool requests.")
local helperPortInput = createInput(connectionCard, 60, DEFAULT_HELPER_PORT, currentHelperPort)

local autoConnectCard = createCard(86)
createCardHeader(autoConnectCard, "Auto Connect", "When enabled, the plugin attempts a single helper connection during load. Failures stop immediately until you click Connect again.")

local autoConnectLabel = Instance.new("TextLabel")
autoConnectLabel.Position = UDim2.new(0, 14, 0, 56)
autoConnectLabel.Size = UDim2.new(0.65, 0, 0, 20)
autoConnectLabel.BackgroundTransparency = 1
autoConnectLabel.TextXAlignment = Enum.TextXAlignment.Left
autoConnectLabel.Font = Enum.Font.GothamSemibold
autoConnectLabel.TextSize = 13
autoConnectLabel.TextColor3 = Color3.fromRGB(237, 242, 247)
autoConnectLabel.Parent = autoConnectCard

local autoConnectButton = Instance.new("TextButton")
autoConnectButton.Position = UDim2.new(1, -124, 0, 48)
autoConnectButton.Size = UDim2.new(0, 110, 0, 28)
autoConnectButton.AutoButtonColor = true
autoConnectButton.BorderSizePixel = 0
autoConnectButton.Font = Enum.Font.GothamSemibold
autoConnectButton.TextSize = 13
autoConnectButton.Parent = autoConnectCard

local autoConnectButtonCorner = Instance.new("UICorner")
autoConnectButtonCorner.CornerRadius = UDim.new(0, 999)
autoConnectButtonCorner.Parent = autoConnectButton

local statusCard = createCard(94)

local statusPill = Instance.new("TextLabel")
statusPill.Position = UDim2.new(0, 14, 0, 12)
statusPill.Size = UDim2.new(0, 110, 0, 24)
statusPill.BorderSizePixel = 0
statusPill.Font = Enum.Font.GothamSemibold
statusPill.TextSize = 12
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

local function updateAutoConnectViews()
	autoConnectLabel.Text = if currentAutoConnect then "Auto connect once is enabled" else "Auto connect once is disabled"
	autoConnectButton.Text = if currentAutoConnect then "Enabled" else "Disabled"
	autoConnectButton.BackgroundColor3 = if currentAutoConnect then Color3.fromRGB(66, 188, 120) else Color3.fromRGB(52, 63, 76)
	autoConnectButton.TextColor3 = if currentAutoConnect then Color3.fromRGB(18, 24, 31) else Color3.fromRGB(230, 236, 241)
end

local function updateStatusViews()
	local theme = STATUS_THEME[currentStatusKind] or STATUS_THEME.warning
	statusPill.BackgroundColor3 = theme.background
	statusPill.TextColor3 = theme.accent
	statusPill.Text = theme.label
	statusTitle.Text = currentStatusMessage
	statusDetail.Text = currentStatusDetail
	helperPortInput.Text = currentHelperPort
	updateAutoConnectViews()
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
	currentHelperPort = saveConnectionConfig(helperPortInput.Text, currentAutoConnect)
	helperPortInput.Text = currentHelperPort
	setStatus(currentStatusKind, currentStatusMessage, "Saved helper settings for port " .. currentHelperPort)
end

local function isCurrentClient(client: MockWebSocketService.MockWebSocketClient): boolean
	return currentClient == client
end

local function unregisterCurrentInstance()
	if currentHelperBaseUrl ~= nil and currentInstanceId ~= nil then
		pcall(function()
			requestJson(currentHelperBaseUrl, "POST", "/v1/mcp/unregister", {
				instance_id = currentInstanceId,
			})
		end)
	end
	currentInstanceId = nil
	currentHelperBaseUrl = nil
end

local function handleClientStatus(kind: string, message: string)
	local function tryRecoverExpiredRegistration()
		if staleReconnectInFlight then
			return
		end

		staleReconnectInFlight = true
		task.defer(function()
			setStatus("info", "Reconnecting", "The helper forgot this plugin instance. Re-registering with the local helper.")
			local connected = if connectCurrentClient ~= nil then connectCurrentClient() else false
			staleReconnectInFlight = false
			if not connected then
				setStatus("error", "Failed to reconnect to helper", "The helper instance expired and the automatic re-register attempt failed.")
			end
		end)
	end

	if kind == "error" then
		logWarn(message)
		setStatus("error", "Helper request failed", message)
		readyAnnounced = false
		return
	end

	if kind == "connected" then
		if not readyAnnounced then
			logInfo("The MCP Studio plugin is ready for prompts.")
			readyAnnounced = true
		end
		logInfo(message)
		setStatus("connected", "Connected to helper", message)
		return
	end

	logInfo(message)
	setStatus("info", "Waiting on helper", message)
	if kind == "warning" then
		if string.find(string.lower(message), "expired") then
			tryRecoverExpiredRegistration()
			return
		end
		readyAnnounced = false
	end
end

local function connectToHelper(helperPort: string)
	local baseUrl = helperBaseUrl(helperPort)
	local registration = requestJson(baseUrl, "POST", "/v1/mcp/register", {
		place_id = tostring(game.PlaceId),
	})
	local instanceId = registration.instance_id
	if type(instanceId) ~= "string" or instanceId == "" then
		error("Helper register did not return an instance_id")
	end

	local client = MockWebSocketService:CreateClient(baseUrl)
	client:SetReceiveEndpoint("/v1/mcp/plugin/request?instance_id=" .. instanceId)
	client:SetSendEndpoint("/v1/mcp/plugin/response")

	client.Opened:Connect(function()
		if isCurrentClient(client) then
			logInfo("Started local long polling against helper on port " .. helperPort)
			setStatus("info", "Waiting on helper", "Registered place " .. tostring(game.PlaceId) .. " as instance " .. instanceId .. ". Waiting for a healthy poll response.")
		end
	end)

	client.Closed:Connect(function()
		if isCurrentClient(client) then
			logWarn("Helper connection closed for port " .. helperPort)
			setStatus("warning", "Disconnected", "Stopped polling the local helper on port " .. helperPort)
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
			local decodeError = "Failed to decode helper request payload: " .. tostring(body)
			logWarn(decodeError)
			setStatus("error", "Invalid request payload", decodeError)
			return
		end

		if type(body) ~= "table" or body.id == nil or body.args == nil then
			local invalidError = "Received malformed helper request payload"
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
				instance_id = instanceId,
				id = id,
				success = success,
				response = response,
			})
		end

		local args = body.args :: Types.ToolArgs
		local toolName = next(args)
		logInfo("Received helper tool request " .. tostring(toolName) .. " (id=" .. id .. ")")

		local success, response = pcall(toolCallHandler, args)
		if success then
			sendResponseOnce(true, response)
			setStatus("connected", "Connected to helper", "Handled tool " .. tostring(toolName))
		else
			local errorMessage = tostring(response)
			sendResponseOnce(false, errorMessage)
			setStatus("error", "Tool failed", errorMessage)
		end
	end)

	currentInstanceId = instanceId
	currentHelperBaseUrl = baseUrl
	return client, instanceId
end

local function disconnectCurrentClient()
	if currentClient ~= nil then
		currentClient:Close()
		currentClient = nil
	end
	unregisterCurrentInstance()
	readyAnnounced = false
	logInfo("The MCP Studio plugin is stopped.")
	setStatus("warning", "Disconnected", "Stopped polling the local helper")
end

connectCurrentClient = function()
	persistCurrentInputs()
	if currentClient ~= nil then
		disconnectCurrentClient()
	end

	readyAnnounced = false
	staleReconnectInFlight = false
	logInfo("Connecting to local helper on port " .. currentHelperPort)
	setStatus("info", "Connecting", "Registering PlaceId " .. tostring(game.PlaceId) .. " with the local helper on port " .. currentHelperPort)

	local ok, clientOrError, instanceId = pcall(function()
		return connectToHelper(currentHelperPort)
	end)
	if not ok then
		local errorMessage = tostring(clientOrError)
		logWarn(errorMessage)
		setStatus("error", "Failed to connect to helper", errorMessage)
		return false
	end

	currentClient = clientOrError
	setStatus("info", "Waiting on helper", "Registered helper instance " .. tostring(instanceId) .. " for PlaceId " .. tostring(game.PlaceId) .. ". Waiting for helper poll readiness.")
	return true
end

widget:GetPropertyChangedSignal("Enabled"):Connect(function()
	if currentClient == nil then
		toggleButton:SetActive(widget.Enabled)
	end
end)

helperPortInput.FocusLost:Connect(function()
	currentHelperPort = normalizeHelperPort(helperPortInput.Text)
	helperPortInput.Text = currentHelperPort
	persistCurrentInputs()
end)

autoConnectButton.MouseButton1Click:Connect(function()
	currentAutoConnect = not currentAutoConnect
	persistCurrentInputs()
	updateStatusViews()
end)

saveButton.MouseButton1Click:Connect(function()
	persistCurrentInputs()
end)

resetButton.MouseButton1Click:Connect(function()
	currentHelperPort = DEFAULT_HELPER_PORT
	currentAutoConnect = false
	helperPortInput.Text = currentHelperPort
	saveConnectionConfig(currentHelperPort, currentAutoConnect)
	setStatus("warning", "Reset settings", "Reset helper settings to the default local port and disabled auto connect.")
end)

connectButton.MouseButton1Click:Connect(function()
	if currentClient == nil then
		connectCurrentClient()
	else
		disconnectCurrentClient()
	end
end)

if currentAutoConnect then
	task.defer(function()
		local connected = connectCurrentClient()
		if connected then
			widget.Enabled = false
		end
	end)
end

updateStatusViews()

toggleButton.Click:Connect(function()
	widget.Enabled = not widget.Enabled
	updateStatusViews()
end)

plugin.Unloading:Connect(function()
	disconnectCurrentClient()
end)
