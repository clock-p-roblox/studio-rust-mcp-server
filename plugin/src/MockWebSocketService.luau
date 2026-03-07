local HttpService = game:GetService("HttpService")

local MockWebSocketClient = {}
MockWebSocketClient.__index = MockWebSocketClient

local EnumWebSocketState = {
	Connecting = "Connecting",
	Open = "Open",
	Closing = "Closing",
	Closed = "Closed",
}

local ConnectionHealth = {
	Info = "info",
	Connected = "connected",
	Warning = "warning",
	Error = "error",
}

local CloseableStates = {
	[EnumWebSocketState.Connecting] = true,
	[EnumWebSocketState.Open] = true,
}

local POLL_WAIT_TIME = 1

export type MockWebSocketClient = {
	Send: (self: MockWebSocketClient, data: any) -> (),
	Close: (self: MockWebSocketClient) -> (),
	SetReceiveEndpoint: (self: MockWebSocketClient, endpoint: string) -> (),
	SetSendEndpoint: (self: MockWebSocketClient, endpoint: string) -> (),
	SetHeaders: (self: MockWebSocketClient, headers: { [string]: string }) -> (),
	ConnectionStatusChanged: RBXScriptSignal,
	Opened: RBXScriptSignal,
	Closed: RBXScriptSignal,
	MessageReceived: RBXScriptSignal,
	ConnectionState: typeof(EnumWebSocketState.Connecting),
}

type MockWebSocketClientPrivate = MockWebSocketClient & {
	new: (uri: string) -> MockWebSocketClient,
	_OpenImpl: (self: MockWebSocketClient) -> (),
	_EmitStatus: (self: MockWebSocketClientPrivate, kind: string, message: string) -> (),
	_uri: string,
	_receiveEndpoint: string,
	_sendEndpoint: string,
	_headers: { [string]: string },
	_pollTask: thread?,
	_lastHealthy: boolean,
	_OpenedEvent: BindableEvent,
	_ClosedEvent: BindableEvent,
	_MessageReceivedEvent: BindableEvent,
	_ConnectionStatusChangedEvent: BindableEvent,
}

local function shortBody(body: string?): string
	if body == nil or body == "" then
		return ""
	end

	if #body <= 180 then
		return body
	end

	return string.sub(body, 1, 180) .. "..."
end

function MockWebSocketClient.new(uri: string): MockWebSocketClient
	local self: MockWebSocketClientPrivate = setmetatable({}, MockWebSocketClient) :: any

	self._uri = uri
	self._receiveEndpoint = ""
	self._sendEndpoint = ""
	self._headers = {}
	self._pollTask = nil :: thread?
	self._lastHealthy = false

	self._OpenedEvent = Instance.new("BindableEvent")
	self.Opened = self._OpenedEvent.Event

	self._ClosedEvent = Instance.new("BindableEvent")
	self.Closed = self._ClosedEvent.Event

	self._MessageReceivedEvent = Instance.new("BindableEvent")
	self.MessageReceived = self._MessageReceivedEvent.Event

	self._ConnectionStatusChangedEvent = Instance.new("BindableEvent")
	self.ConnectionStatusChanged = self._ConnectionStatusChangedEvent.Event

	self.ConnectionState = EnumWebSocketState.Connecting

	task.defer(self._OpenImpl, self)

	return self
end

local function doRequest(url: string, method: "GET" | "POST", body: any, headers: { [string]: string }?)
	local requestHeaders = {}

	if headers then
		for key, value in headers do
			requestHeaders[key] = value
		end
	end

	if body ~= nil then
		requestHeaders["Content-Type"] = "application/json"
	end

	local ok, response = pcall(function()
		return HttpService:RequestAsync({
			Url = url,
			Method = method,
			Headers = requestHeaders,
			Body = if body then HttpService:JSONEncode(body) else nil,
			Compress = Enum.HttpCompression.None,
		})
	end)

	if ok then
		return response, nil
	end

	return nil, tostring(response)
end

function MockWebSocketClient._EmitStatus(self: MockWebSocketClientPrivate, kind: string, message: string)
	self._ConnectionStatusChangedEvent:Fire(kind, message)
end

function MockWebSocketClient._OpenImpl(self: MockWebSocketClientPrivate)
	assert(self.ConnectionState == EnumWebSocketState.Connecting, "WebSocket is not in the Connecting state")

	self.ConnectionState = EnumWebSocketState.Open
	self._OpenedEvent:Fire()
	self:_EmitStatus(ConnectionHealth.Info, "Polling " .. self._uri .. self._receiveEndpoint)

	self._pollTask = task.spawn(function()
		while self.ConnectionState == EnumWebSocketState.Open do
			local requestUrl = self._uri .. self._receiveEndpoint
			local response, requestError = doRequest(requestUrl, "GET", nil, self._headers)

			if response then
				if response.StatusCode == 200 and response.Success then
					if not self._lastHealthy then
						self._lastHealthy = true
						self:_EmitStatus(ConnectionHealth.Connected, "Connected to " .. self._uri)
					end
					self:_EmitStatus(ConnectionHealth.Info, "Received tool request from " .. self._receiveEndpoint)
					self._MessageReceivedEvent:Fire(response.Body)
					continue
				elseif response.StatusCode == 423 then
					if not self._lastHealthy then
						self._lastHealthy = true
						self:_EmitStatus(ConnectionHealth.Connected, "Connected to " .. self._uri)
					end
					continue
				end

				self._lastHealthy = false
				self:_EmitStatus(
					ConnectionHealth.Error,
					string.format(
						"%s %s returned %d (%s)",
						"GET",
						self._receiveEndpoint,
						response.StatusCode,
						shortBody(response.Body)
					)
				)
			else
				self._lastHealthy = false
				self:_EmitStatus(ConnectionHealth.Error, "Request to " .. requestUrl .. " failed: " .. tostring(requestError))
			end

			task.wait(POLL_WAIT_TIME)
		end
	end)
end

function MockWebSocketClient.Send(self: MockWebSocketClientPrivate, data: any)
	local requestUrl = self._uri .. self._sendEndpoint
	local response, requestError = doRequest(requestUrl, "POST", data, self._headers)
	if response and response.Success and response.StatusCode >= 200 and response.StatusCode < 300 then
		self:_EmitStatus(ConnectionHealth.Info, "Sent tool response to " .. self._sendEndpoint)
		return
	end

	if response then
		self:_EmitStatus(
			ConnectionHealth.Error,
			string.format(
				"POST %s returned %d (%s)",
				self._sendEndpoint,
				response.StatusCode,
				shortBody(response.Body)
			)
		)
		return
	end

	self:_EmitStatus(ConnectionHealth.Error, "Request to " .. requestUrl .. " failed: " .. tostring(requestError))
end

function MockWebSocketClient.Close(self: MockWebSocketClientPrivate)
	if CloseableStates[self.ConnectionState] then
		self.ConnectionState = EnumWebSocketState.Closing

		if self._pollTask then
			task.cancel(self._pollTask)
			self._pollTask = nil :: any
		end

		self.ConnectionState = EnumWebSocketState.Closed
		self:_EmitStatus(ConnectionHealth.Warning, "Disconnected from " .. self._uri)
		self._ClosedEvent:Fire()
	end
end

-- START DEVIATION: These methods are not present in the real WebSocketClient instance
function MockWebSocketClient.SetReceiveEndpoint(self: MockWebSocketClientPrivate, endpoint: string)
	self._receiveEndpoint = endpoint
end

function MockWebSocketClient.SetSendEndpoint(self: MockWebSocketClientPrivate, endpoint: string)
	self._sendEndpoint = endpoint
end

function MockWebSocketClient.SetHeaders(self: MockWebSocketClientPrivate, headers: { [string]: string })
	self._headers = headers
end
-- END DEVIATION

local MockWebSocketService = {}
MockWebSocketService.__index = MockWebSocketService

type MockWebSocketService = {
	CreateClient: (self: MockWebSocketService, uri: string) -> MockWebSocketClient,
}

function MockWebSocketService.CreateClient(_: MockWebSocketService, uri: string)
	return MockWebSocketClient.new(uri)
end

return MockWebSocketService
