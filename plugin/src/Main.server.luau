local Main = script:FindFirstAncestor("MCPStudioPlugin")
local ConsoleOutput = require(Main.Utils.ConsoleOutput)
local DataModelType = require(Main.Utils.DataModelType)
local GameStopUtil = require(Main.Utils.GameStopUtil)
local MockWebSocketService = require(Main.MockWebSocketService)
local PluginUtils = require(Main.Utils.PluginUtils)
local StudioModeTracker = require(Main.Utils.StudioModeTracker)
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
local STATUS_POLL_INTERVAL_SECONDS = 1

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
StudioModeTracker.observeCurrentDataModel()
if datamodelType == "Server" then
	task.spawn(GameStopUtil.monitorForStopPlay)
end

if RunService:IsRunning() then
	return
end

StudioModeTracker.normalizeEditState()

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

local function formatAgeMs(value: number?): string?
	if value == nil then
		return nil
	end

	if value < 1000 then
		return "just now"
	end

	local seconds = math.floor(value / 1000)
	if seconds < 60 then
		return tostring(seconds) .. "s ago"
	end

	local minutes = math.floor(seconds / 60)
	if minutes < 60 then
		return tostring(minutes) .. "m ago"
	end

	local hours = math.floor(minutes / 60)
	return tostring(hours) .. "h ago"
end

local function joinSentences(parts: { string }): string
	local filtered = {}
	for _, part in parts do
		if part ~= "" then
			table.insert(filtered, part)
		end
	end

	return table.concat(filtered, " ")
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
	local autoConnect = if type(savedAutoConnect) == "boolean" then savedAutoConnect else true
	return normalizeHelperPort(helperPort), autoConnect
end

local function saveConnectionConfig(helperPort: string, autoConnect: boolean)
	local normalizedPort = normalizeHelperPort(helperPort)
	PluginUtils.setSettings(HELPER_PORT_SETTING_KEY, normalizedPort)
	PluginUtils.setSettings(AUTO_CONNECT_SETTING_KEY, autoConnect)
	logInfo("Saved helper settings for port " .. normalizedPort)
	return normalizedPort
end

local function createDefaultStatusSections()
	return {
		helper = {
			title = "Local Helper",
			kind = "warning",
			message = "Disconnected",
			detail = "Connect to the local helper to start MCP routing.",
		},
		registration = {
			title = "Plugin Registration",
			kind = "warning",
			message = "Inactive",
			detail = "The plugin is not registered with the helper yet.",
		},
		remote = {
			title = "Remote MCP",
			kind = "warning",
			message = "Inactive",
			detail = "The helper has not opened a remote MCP bridge for this place.",
		},
	}
end

local function matchesTaskIdentity(payload: any, taskId: string?)
	if type(payload) ~= "table" then
		return false
	end
	if taskId ~= nil and tostring(payload.task_id) ~= taskId then
		return false
	end
	return true
end

local function findRegisteredInstance(statusPayload: any, instanceId: string, taskId: string?)
	if type(statusPayload) ~= "table" or type(statusPayload.instances) ~= "table" then
		return nil
	end

	for _, instance in statusPayload.instances do
		if type(instance) == "table" and tostring(instance.instance_id) == instanceId then
			if taskId == nil then
				return instance
			end
			if matchesTaskIdentity(instance, taskId) then
				return instance
			end
		end
	end

	return nil
end

local function findClaimedTask(statusPayload: any, taskId: string)
	if type(statusPayload) ~= "table" or type(statusPayload.claimed_tasks) ~= "table" then
		return nil
	end

	for _, taskInfo in statusPayload.claimed_tasks do
		if type(taskInfo) == "table" and matchesTaskIdentity(taskInfo, taskId) then
			return taskInfo
		end
	end

	return nil
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

local widgetInfo = DockWidgetPluginGuiInfo.new(Enum.InitialDockState.Right, false, false, 420, 420, 360, 300)
local widget = plugin:CreateDockWidgetPluginGui("MCPConnectionPanel", widgetInfo)
widget.Title = "Studio MCP"

local currentHelperPort, currentAutoConnect = loadConnectionConfig()
local currentClient: MockWebSocketService.MockWebSocketClient?
local currentInstanceId: string? = nil
local currentHelperBaseUrl: string? = nil
local currentRemoteBaseUrl: string? = nil
local currentTaskId: string? = nil
local readyAnnounced = false
local currentStatusKind = "warning"
local currentStatusMessage = "Not connected"
local currentStatusDetail = "Connect to the local helper. The helper registers this Studio with a task-scoped remote MCP route."
local statusSections = createDefaultStatusSections()
local statusPollToken = 0
local connectCurrentClient = nil

local panel = Instance.new("ScrollingFrame")
panel.Size = UDim2.fromScale(1, 1)
panel.BackgroundColor3 = Color3.fromRGB(15, 20, 26)
panel.BorderSizePixel = 0
panel.AutomaticCanvasSize = Enum.AutomaticSize.Y
panel.CanvasSize = UDim2.fromOffset(0, 0)
panel.ScrollBarThickness = 6
panel.ScrollingDirection = Enum.ScrollingDirection.Y
panel.ScrollBarImageColor3 = Color3.fromRGB(55, 68, 84)
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
subtitleLabel.Text = "Only the local helper port is configurable. The helper owns tokens, resolves the claimed task, and routes requests for the active Studio task."
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
createCardHeader(connectionCard, "Helper Port", "The helper must be running locally. The plugin asks the helper to register this Studio against the claimed task and receive tool requests.")
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

local statusCard = createCard(244)

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
statusDetail.Size = UDim2.new(1, -28, 0, 30)
statusDetail.BackgroundTransparency = 1
statusDetail.TextXAlignment = Enum.TextXAlignment.Left
statusDetail.TextYAlignment = Enum.TextYAlignment.Top
statusDetail.Font = Enum.Font.Gotham
statusDetail.TextSize = 12
statusDetail.TextColor3 = Color3.fromRGB(156, 169, 181)
statusDetail.TextWrapped = true
statusDetail.Parent = statusCard

local function createStatusRow(parent: Instance, top: number, title: string)
	local row = Instance.new("Frame")
	row.Position = UDim2.new(0, 14, 0, top)
	row.Size = UDim2.new(1, -28, 0, 42)
	row.BackgroundTransparency = 1
	row.Parent = parent

	local label = Instance.new("TextLabel")
	label.Size = UDim2.new(0.44, 0, 0, 18)
	label.BackgroundTransparency = 1
	label.TextXAlignment = Enum.TextXAlignment.Left
	label.Font = Enum.Font.GothamSemibold
	label.TextSize = 12
	label.TextColor3 = Color3.fromRGB(220, 228, 235)
	label.Text = title
	label.Parent = row

	local pill = Instance.new("TextLabel")
	pill.AnchorPoint = Vector2.new(1, 0)
	pill.Position = UDim2.new(1, 0, 0, 0)
	pill.Size = UDim2.new(0, 100, 0, 20)
	pill.BorderSizePixel = 0
	pill.Font = Enum.Font.GothamSemibold
	pill.TextSize = 11
	pill.Parent = row

	local pillCorner = Instance.new("UICorner")
	pillCorner.CornerRadius = UDim.new(0, 999)
	pillCorner.Parent = pill

	local detail = Instance.new("TextLabel")
	detail.Position = UDim2.new(0, 0, 0, 18)
	detail.Size = UDim2.new(1, 0, 0, 24)
	detail.BackgroundTransparency = 1
	detail.TextXAlignment = Enum.TextXAlignment.Left
	detail.TextYAlignment = Enum.TextYAlignment.Top
	detail.Font = Enum.Font.Gotham
	detail.TextSize = 11
	detail.TextColor3 = Color3.fromRGB(156, 169, 181)
	detail.TextWrapped = true
	detail.Parent = row

	return {
		pill = pill,
		detail = detail,
	}
end

local helperStatusRow = createStatusRow(statusCard, 110, statusSections.helper.title)
local registrationStatusRow = createStatusRow(statusCard, 152, statusSections.registration.title)
local remoteStatusRow = createStatusRow(statusCard, 194, statusSections.remote.title)

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

local statusRows = {
	helper = helperStatusRow,
	registration = registrationStatusRow,
	remote = remoteStatusRow,
}

local function recomputeSummaryStatus()
	local helper = statusSections.helper
	local registration = statusSections.registration
	local remote = statusSections.remote

	if helper.kind == "error" then
		currentStatusKind = "error"
		currentStatusMessage = "Local helper unavailable"
		currentStatusDetail = helper.detail
		return
	end

	if registration.kind == "error" then
		currentStatusKind = "error"
		currentStatusMessage = "Plugin registration failed"
		currentStatusDetail = registration.detail
		return
	end

	if registration.kind == "warning" and currentClient ~= nil then
		currentStatusKind = "warning"
		currentStatusMessage = "Plugin registration lost"
		currentStatusDetail = registration.detail
		return
	end

	if remote.kind == "error" then
		currentStatusKind = "error"
		currentStatusMessage = "Remote MCP unavailable"
		currentStatusDetail = remote.detail
		return
	end

	if remote.kind == "warning" and currentClient ~= nil then
		currentStatusKind = "warning"
		currentStatusMessage = "Remote MCP degraded"
		currentStatusDetail = remote.detail
		return
	end

	if helper.kind == "connected" and registration.kind == "connected" and remote.kind == "connected" then
		currentStatusKind = "connected"
		currentStatusMessage = "MCP ready"
		currentStatusDetail = remote.detail
		return
	end

	if helper.kind == "connected" and registration.kind == "connected" then
		currentStatusKind = "info"
		currentStatusMessage = "Waiting for remote MCP"
		currentStatusDetail = remote.detail
		return
	end

	if helper.kind == "connected" then
		currentStatusKind = "info"
		currentStatusMessage = "Helper connected"
		currentStatusDetail = registration.detail
		return
	end

	currentStatusKind = helper.kind
	currentStatusMessage = helper.message
	currentStatusDetail = helper.detail
end

local function reconcileReadyAnnouncement()
	local isReady = currentClient ~= nil and currentStatusKind == "connected"
	if isReady and not readyAnnounced then
		logInfo("The MCP Studio plugin is ready for prompts.")
		readyAnnounced = true
		return
	end

	if not isReady then
		readyAnnounced = false
	end
end

local function updateStatusViews()
	local theme = STATUS_THEME[currentStatusKind] or STATUS_THEME.warning
	statusPill.BackgroundColor3 = theme.background
	statusPill.TextColor3 = theme.accent
	statusPill.Text = theme.label
	statusTitle.Text = currentStatusMessage
	statusDetail.Text = currentStatusDetail
	for name, row in statusRows do
		local section = statusSections[name]
		local rowTheme = STATUS_THEME[section.kind] or STATUS_THEME.warning
		row.pill.BackgroundColor3 = rowTheme.background
		row.pill.TextColor3 = rowTheme.accent
		row.pill.Text = section.message
		row.detail.Text = section.detail
	end
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

local function setStatusSection(name: string, kind: string, message: string, detail: string)
	local section = statusSections[name]
	if section == nil then
		return
	end

	section.kind = kind
	section.message = message
	section.detail = detail
	recomputeSummaryStatus()
	reconcileReadyAnnouncement()
	updateStatusViews()
end

local function resetStatusSections(detail: string?)
	statusSections = createDefaultStatusSections()
	if detail ~= nil then
		statusSections.helper.detail = detail
	end
	recomputeSummaryStatus()
	reconcileReadyAnnouncement()
	updateStatusViews()
	return statusSections
end

local function persistCurrentInputs()
	currentHelperPort = saveConnectionConfig(helperPortInput.Text, currentAutoConnect)
	helperPortInput.Text = currentHelperPort
	updateStatusViews()
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
	currentRemoteBaseUrl = nil
	currentTaskId = nil
end

local function stopStatusPolling()
	statusPollToken = statusPollToken + 1
end

local function refreshStatusFromHelper()
	if currentClient == nil or currentHelperBaseUrl == nil then
		return
	end

	local ok, statusPayload = pcall(function()
		return requestJson(currentHelperBaseUrl, "GET", "/status")
	end)
	if not ok then
		local errorMessage = tostring(statusPayload)
		setStatusSection("helper", "error", "Unavailable", summarizeText(errorMessage))
		setStatusSection(
			"registration",
			"warning",
			"Unknown",
			"Could not confirm plugin registration while the helper status endpoint is unavailable."
		)
		setStatusSection(
			"remote",
			"warning",
			"Unknown",
			currentRemoteBaseUrl ~= nil
				and ("Remote MCP status is unknown for " .. currentRemoteBaseUrl .. ".")
				or "Remote MCP status is unknown while the helper status endpoint is unavailable."
		)
		return
	end

	local helperPort = tostring(statusPayload.helper_port or currentHelperPort)
	setStatusSection("helper", "connected", "Connected", "Local helper reachable on port " .. helperPort .. ".")

	local registeredInstance = if currentInstanceId ~= nil
		then findRegisteredInstance(statusPayload, currentInstanceId, currentTaskId)
		else nil

	if type(registeredInstance) == "table" then
		if type(registeredInstance.task_id) == "string" and registeredInstance.task_id ~= "" then
			currentTaskId = registeredInstance.task_id
		end
		if type(registeredInstance.remote_base_url) == "string" and registeredInstance.remote_base_url ~= "" then
			currentRemoteBaseUrl = registeredInstance.remote_base_url
		end
		local taskSuffix = if currentTaskId ~= nil
			then " for task " .. currentTaskId .. "."
			else "."
		setStatusSection(
			"registration",
			"connected",
			"Registered",
			"Helper tracks plugin instance " .. currentInstanceId .. " for PlaceId " .. tostring(game.PlaceId) .. taskSuffix
		)
	else
		local registrationDetail = if currentInstanceId ~= nil
			then "Helper does not report plugin instance " .. currentInstanceId .. " for PlaceId " .. tostring(game.PlaceId) .. "."
			else "The plugin has not registered an instance_id with the helper yet."
		setStatusSection("registration", "warning", "Missing", registrationDetail)
	end

	local claimedTask = if currentTaskId ~= nil
		then findClaimedTask(statusPayload, currentTaskId)
		else nil
	if type(claimedTask) ~= "table" then
		local remoteDetail = if currentRemoteBaseUrl ~= nil
			then "Helper has not opened the remote MCP bridge at " .. currentRemoteBaseUrl .. "."
			else "Helper has not opened the remote MCP bridge for this place yet."
		setStatusSection("remote", "warning", "Missing", remoteDetail)
		return
	end

	local remoteSource = claimedTask
	if type(remoteSource.task_id) == "string" and remoteSource.task_id ~= "" then
		currentTaskId = remoteSource.task_id
	end
	local remoteBaseUrl = if type(remoteSource.remote_base_url) == "string" and remoteSource.remote_base_url ~= ""
		then remoteSource.remote_base_url
		elseif type(remoteSource.mcp_base_url) == "string" and remoteSource.mcp_base_url ~= ""
		then remoteSource.mcp_base_url
		else currentRemoteBaseUrl
	if remoteBaseUrl ~= nil then
		currentRemoteBaseUrl = remoteBaseUrl
	end

	local remoteState = tostring(remoteSource.remote_state or "disconnected")
	local remoteConnectionId = if type(remoteSource.remote_connection_id) == "string" and remoteSource.remote_connection_id ~= ""
		then remoteSource.remote_connection_id
		else nil
	local remoteLastError = if type(remoteSource.remote_last_error) == "string" and remoteSource.remote_last_error ~= ""
		then remoteSource.remote_last_error
		else nil
	local lastReadyAge = formatAgeMs(tonumber(remoteSource.remote_last_ready_age_ms))
	local lastServerAge = formatAgeMs(tonumber(remoteSource.remote_last_server_message_age_ms))

	if remoteState == "connected" then
		setStatusSection(
			"remote",
			"connected",
			"Connected",
			joinSentences({
				remoteBaseUrl ~= nil and ("Remote bridge healthy at " .. remoteBaseUrl .. ".") or "",
				remoteConnectionId ~= nil and ("Connection " .. remoteConnectionId .. ".") or "",
				lastServerAge ~= nil and ("Last server traffic " .. lastServerAge .. ".") or "",
			})
		)
		return
	end

	if remoteState == "connecting" then
		setStatusSection(
			"remote",
			"info",
			"Connecting",
			joinSentences({
				remoteBaseUrl ~= nil and ("Opening remote bridge at " .. remoteBaseUrl .. ".") or "",
				lastReadyAge ~= nil and ("Last ready acknowledgement " .. lastReadyAge .. ".") or "",
			})
		)
		return
	end

	if remoteState == "retrying" then
		setStatusSection(
			"remote",
			"error",
			"Retrying",
			joinSentences({
				remoteBaseUrl ~= nil and ("Remote bridge retrying at " .. remoteBaseUrl .. ".") or "",
				remoteLastError ~= nil and ("Last error: " .. remoteLastError .. ".") or "",
				lastReadyAge ~= nil and ("Last healthy acknowledgement " .. lastReadyAge .. ".") or "",
			})
		)
		return
	end

	setStatusSection(
		"remote",
		"warning",
		"Disconnected",
		joinSentences({
			remoteBaseUrl ~= nil and ("No active remote bridge at " .. remoteBaseUrl .. ".") or "",
			remoteLastError ~= nil and ("Last error: " .. remoteLastError .. ".") or "",
		})
	)
end

local function startStatusPolling()
	stopStatusPolling()
	local token = statusPollToken
	task.spawn(function()
		while token == statusPollToken do
			if currentClient == nil or currentHelperBaseUrl == nil then
				return
			end

			refreshStatusFromHelper()
			task.wait(STATUS_POLL_INTERVAL_SECONDS)
		end
	end)
end

local function handleClientStatus(kind: string, message: string)
	local lowerMessage = string.lower(message)

	if kind == "error" then
		logWarn(message)
		setStatusSection("helper", "error", "Error", message)
		setStatusSection("remote", "warning", "Unknown", "Remote MCP status is unavailable while helper requests are failing.")
		return
	end

	if kind == "connected" then
		logInfo(message)
		setStatusSection("helper", "connected", "Connected", message)
		refreshStatusFromHelper()
		return
	end

	logInfo(message)
	if kind == "warning" then
		if string.find(lowerMessage, "expired") then
			setStatusSection("helper", "connected", "Connected", "Local helper responded, but the plugin registration expired.")
			setStatusSection("registration", "warning", "Expired", message)
			setStatusSection("remote", "warning", "Unknown", "Remote MCP is unavailable until the plugin registers a fresh helper instance.")
			return
		end
		setStatusSection("helper", "warning", "Disconnected", message)
		return
	end

	setStatusSection("helper", "info", "Polling", message)
end

local function connectToHelper(helperPort: string)
	local baseUrl = helperBaseUrl(helperPort)
	local registrationPayload = {
		place_id = tostring(game.PlaceId),
	}
	local registration = requestJson(baseUrl, "POST", "/v1/mcp/register", registrationPayload)
	local instanceId = registration.instance_id
	if type(instanceId) ~= "string" or instanceId == "" then
		error("Helper register did not return an instance_id")
	end
	local remoteBaseUrl = if type(registration.remote_base_url) == "string" and registration.remote_base_url ~= ""
		then registration.remote_base_url
		else nil
	local taskId = if type(registration.task_id) == "string" and registration.task_id ~= ""
		then registration.task_id
		else nil

	local client = MockWebSocketService:CreateClient(baseUrl)
	client:SetReceiveEndpoint("/v1/mcp/plugin/request?instance_id=" .. instanceId)
	client:SetSendEndpoint("/v1/mcp/plugin/response")

	client.Opened:Connect(function()
		if isCurrentClient(client) then
			logInfo("Started local long polling against helper on port " .. helperPort)
			setStatusSection("helper", "info", "Polling", "Started local long polling against helper on port " .. helperPort .. ".")
			setStatusSection(
			"registration",
			"info",
			"Registering",
			"Registered place " .. tostring(game.PlaceId) .. " as helper instance " .. instanceId .. ((taskId ~= nil) and (" for task " .. taskId .. ".") or ".")
		)
			setStatusSection(
				"remote",
				"info",
				"Checking",
				remoteBaseUrl ~= nil
					and ("Waiting for helper to establish the remote MCP bridge at " .. remoteBaseUrl .. ".")
					or "Waiting for helper to establish the remote MCP bridge."
			)
			refreshStatusFromHelper()
		end
	end)

	client.Closed:Connect(function()
		if isCurrentClient(client) then
			logWarn("Helper connection closed for port " .. helperPort)
			stopStatusPolling()
			currentClient = nil
			currentInstanceId = nil
			currentHelperBaseUrl = nil
			currentRemoteBaseUrl = nil
			currentTaskId = nil
			resetStatusSections("Stopped polling the local helper on port " .. helperPort .. ".")
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
			setStatusSection("registration", "error", "Invalid payload", decodeError)
			return
		end

		if type(body) ~= "table" or body.id == nil or body.args == nil then
			local invalidError = "Received malformed helper request payload"
			logWarn(invalidError)
			setStatusSection("registration", "error", "Malformed payload", invalidError)
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
			setStatusSection("registration", "connected", "Registered", "Handled tool " .. tostring(toolName) .. ".")
		else
			local errorMessage = tostring(response)
			sendResponseOnce(false, errorMessage)
			setStatusSection("registration", "error", "Tool failed", errorMessage)
		end
	end)

	currentInstanceId = instanceId
	currentHelperBaseUrl = baseUrl
	currentRemoteBaseUrl = remoteBaseUrl
	currentTaskId = taskId
	return client, instanceId
end

local function disconnectCurrentClient()
	if currentClient ~= nil then
		local client = currentClient
		currentClient = nil
		stopStatusPolling()
		client:Close()
	end
	unregisterCurrentInstance()
	logInfo("The MCP Studio plugin is stopped.")
	resetStatusSections("Stopped polling the local helper.")
end

connectCurrentClient = function()
	persistCurrentInputs()
	if currentClient ~= nil then
		disconnectCurrentClient()
	end

	readyAnnounced = false
	logInfo("Connecting to local helper on port " .. currentHelperPort)
	setStatusSection(
		"helper",
		"info",
		"Connecting",
		"Registering PlaceId " .. tostring(game.PlaceId) .. " with the local helper on port " .. currentHelperPort .. "."
	)
	setStatusSection("registration", "info", "Waiting", "The plugin is waiting for the helper registration response.")
	setStatusSection("remote", "warning", "Inactive", "Remote MCP will be checked after local helper registration succeeds.")

	local ok, clientOrError, instanceId = pcall(function()
		return connectToHelper(currentHelperPort)
	end)
	if not ok then
		local errorMessage = tostring(clientOrError)
		logWarn(errorMessage)
		stopStatusPolling()
		setStatusSection("helper", "error", "Error", errorMessage)
		setStatusSection("registration", "warning", "Inactive", "The plugin could not register because the helper connection failed.")
		setStatusSection("remote", "warning", "Inactive", "Remote MCP is unavailable until the helper connection succeeds.")
		return false
	end

	currentClient = clientOrError
	setStatusSection("registration", "info", "Registering", "Registered helper instance " .. tostring(instanceId) .. " for PlaceId " .. tostring(game.PlaceId) .. ".")
	startStatusPolling()
	refreshStatusFromHelper()
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
	currentAutoConnect = true
	helperPortInput.Text = currentHelperPort
	saveConnectionConfig(currentHelperPort, currentAutoConnect)
	if currentClient == nil then
		resetStatusSections("Reset helper settings to the default local port and enabled auto connect.")
	else
		setStatusSection(
			"helper",
			statusSections.helper.kind,
			statusSections.helper.message,
			"Saved default helper settings. Reconnect if you want to switch the live connection to port " .. currentHelperPort .. "."
		)
	end
end)

connectButton.MouseButton1Click:Connect(function()
	if currentClient == nil then
		connectCurrentClient()
	else
		disconnectCurrentClient()
	end
end)

resetStatusSections(currentStatusDetail)

if currentAutoConnect then
	task.defer(function()
		local connected = connectCurrentClient()
		if connected then
			widget.Enabled = false
		end
	end)
end

toggleButton.Click:Connect(function()
	widget.Enabled = not widget.Enabled
	updateStatusViews()
end)

plugin.Unloading:Connect(function()
	disconnectCurrentClient()
end)
