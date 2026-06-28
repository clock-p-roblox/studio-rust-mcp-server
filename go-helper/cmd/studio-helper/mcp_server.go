package main

import (
	"context"
	"encoding/json"
	"fmt"
	"log/slog"
	"net/http"
	"strconv"
	"strings"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/runtimelog"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/screenshot"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/tasksession"
)

type mcpJSONRPCRequest struct {
	JSONRPC string          `json:"jsonrpc"`
	ID      any             `json:"id,omitempty"`
	Method  string          `json:"method"`
	Params  json.RawMessage `json:"params,omitempty"`
}

type mcpJSONRPCResponse struct {
	JSONRPC string       `json:"jsonrpc"`
	ID      any          `json:"id,omitempty"`
	Result  any          `json:"result,omitempty"`
	Error   *mcpRPCError `json:"error,omitempty"`
}

type mcpRPCError struct {
	Code    int    `json:"code"`
	Message string `json:"message"`
}

type mcpToolCallParams struct {
	Name      string         `json:"name"`
	Arguments map[string]any `json:"arguments"`
}

type mcpRuntime struct {
	taskSessions         *tasksession.Registry
	studioManager        *studio.Manager
	commandBroker        *mcp2CommandBrokerRegistry
	runtimeLogs          *runtimelog.Store
	logger               *slog.Logger
	publicExposureStatus func() publicExposureStatus
}

func registerMCPHandlers(
	mux *http.ServeMux,
	taskSessions *tasksession.Registry,
	studioManager *studio.Manager,
	commandBrokers *mcp2CommandBrokerRegistry,
	runtimeLogs *runtimelog.Store,
	logger *slog.Logger,
	publicExposureStatus func() publicExposureStatus,
) {
	runtime := &mcpRuntime{
		taskSessions:         taskSessions,
		studioManager:        studioManager,
		commandBroker:        commandBrokers,
		runtimeLogs:          runtimeLogs,
		logger:               logger,
		publicExposureStatus: publicExposureStatus,
	}
	mux.HandleFunc("GET /status", runtime.handleMCPStatus)
	mux.HandleFunc("POST /mcp", runtime.handleMCP)
}

func (m *mcpRuntime) handleMCPStatus(w http.ResponseWriter, r *http.Request) {
	taskID := strings.TrimSpace(r.URL.Query().Get("task_id"))
	if taskID == "" {
		payload := map[string]any{
			"ok":      true,
			"service": "studio-helper2-mcp",
			"tools":   mcpToolNames(),
		}
		if m.publicExposureStatus != nil {
			payload["public_exposure"] = m.publicExposureStatus()
		}
		writeJSON(w, http.StatusOK, payload)
		return
	}
	payload, statusCode := m.taskStatusPayload(taskID)
	writeJSON(w, statusCode, payload)
}

func (m *mcpRuntime) handleMCP(w http.ResponseWriter, r *http.Request) {
	var request mcpJSONRPCRequest
	if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
		writeMCPResponse(w, mcpJSONRPCResponse{
			JSONRPC: "2.0",
			Error:   &mcpRPCError{Code: -32700, Message: err.Error()},
		})
		return
	}
	switch request.Method {
	case "initialize":
		w.Header().Set("Mcp-Session-Id", fmt.Sprintf("helper2-%d", time.Now().UnixNano()))
		writeMCPResponse(w, mcpJSONRPCResponse{
			JSONRPC: "2.0",
			ID:      request.ID,
			Result: map[string]any{
				"protocolVersion": "2025-11-25",
				"capabilities": map[string]any{
					"tools": map[string]any{"listChanged": false},
				},
				"serverInfo": map[string]any{
					"name":    "studio-helper2",
					"version": "helper2-hubless",
				},
			},
		})
	case "notifications/initialized":
		w.WriteHeader(http.StatusAccepted)
	case "tools/list":
		writeMCPResponse(w, mcpJSONRPCResponse{
			JSONRPC: "2.0",
			ID:      request.ID,
			Result:  map[string]any{"tools": mcpTools()},
		})
	case "tools/call":
		result, err := m.handleToolCall(r.Context(), request.Params)
		if err != nil {
			writeMCPResponse(w, mcpJSONRPCResponse{
				JSONRPC: "2.0",
				ID:      request.ID,
				Error:   &mcpRPCError{Code: -32602, Message: err.Error()},
			})
			return
		}
		writeMCPResponse(w, mcpJSONRPCResponse{
			JSONRPC: "2.0",
			ID:      request.ID,
			Result:  result,
		})
	default:
		writeMCPResponse(w, mcpJSONRPCResponse{
			JSONRPC: "2.0",
			ID:      request.ID,
			Error:   &mcpRPCError{Code: -32601, Message: "method not found: " + request.Method},
		})
	}
}

func (m *mcpRuntime) handleToolCall(ctx context.Context, raw json.RawMessage) (map[string]any, error) {
	var params mcpToolCallParams
	if err := json.Unmarshal(raw, &params); err != nil {
		return nil, err
	}
	if params.Arguments == nil {
		params.Arguments = map[string]any{}
	}
	payload, err := m.runTool(ctx, params.Name, params.Arguments)
	if err != nil {
		return mcpToolError(err.Error()), nil
	}
	return mcpToolResult(payload), nil
}

func (m *mcpRuntime) runTool(ctx context.Context, name string, args map[string]any) (map[string]any, error) {
	taskID, err := requiredStringArg(args, "task_id")
	if err != nil {
		return nil, err
	}
	switch name {
	case "helper2_status":
		payload, statusCode := m.taskStatusPayload(taskID)
		if statusCode >= 400 {
			return nil, fmt.Errorf("%v", payload["message"])
		}
		return payload, nil
	case "helper2_studio_mode":
		return m.runStudioMode(ctx, taskID), nil
	case "helper2_studio_play":
		mode, _ := optionalStringArg(args, "mode")
		if mode != "" && mode != "start_play" {
			return nil, fmt.Errorf("helper2 MCP play supports start_play only in this phase, got %s", mode)
		}
		return m.runStudioCommand(ctx, taskID, mcp2CommandStudioPlay)
	case "helper2_studio_stop":
		mode, _ := optionalStringArg(args, "mode")
		if mode != "" && mode != "stop" {
			return nil, fmt.Errorf("helper2 MCP stop supports stop only in this phase, got %s", mode)
		}
		return m.runStudioCommand(ctx, taskID, mcp2CommandStudioStop)
	case "helper2_studio_screenshot":
		return m.runScreenshot(ctx, taskID)
	case "helper2_runtime_log":
		cursor, _ := optionalStringArg(args, "cursor")
		limit := optionalIntArg(args, "limit", runtimelog.DefaultReadLimit)
		return m.readRuntimeLog(taskID, cursor, limit)
	default:
		return nil, fmt.Errorf("unknown helper2 MCP tool: %s", name)
	}
}

func (m *mcpRuntime) taskStatusPayload(taskID string) (map[string]any, int) {
	status := m.taskSessions.Status(taskID)
	if !status.OK {
		return map[string]any{
			"ok":      false,
			"code":    "task_not_registered",
			"message": fmt.Sprintf("helper2 has no registered session for task_id %s", taskID),
			"action":  "restart_task_agent",
			"task_id": taskID,
			"state":   status.State,
		}, http.StatusNotFound
	}
	studioSummary := m.studioManager.Summary()
	desired := make([]studio.DesiredStudio, 0)
	for _, target := range studioSummary.Desired {
		if target.OwnerKind == "task" && target.OwnerID == taskID {
			desired = append(desired, target)
		}
	}
	studios := make([]studio.ManagedProcess, 0)
	for _, process := range studioSummary.Studios {
		if process.OwnerKind == "task" && process.OwnerID == taskID {
			studios = append(studios, process)
		}
	}
	rojoUpstreamURL := ""
	if status.Contract != nil {
		rojoUpstreamURL = status.Contract.RojoUpstreamURL
	}
	return map[string]any{
		"ok":                status.OK,
		"service":           "studio-helper2-mcp",
		"task_id":           status.TaskID,
		"state":             status.State,
		"contract":          status.Contract,
		"last_heartbeat_at": status.LastHeartbeatAt,
		"lease_age_ms":      status.LeaseAgeMS,
		"lease_timeout_ms":  status.LeaseTimeoutMS,
		"rojo_upstream_url": rojoUpstreamURL,
		"desired_studio":    desired,
		"studios":           studios,
		"mcp2_channel":      m.commandBroker.summaryForTask(taskID),
		"recent_commands":   m.commandBroker.recentTerminalsForTask(taskID, 20),
	}, http.StatusOK
}

func (m *mcpRuntime) runStudioMode(ctx context.Context, taskID string) map[string]any {
	status := m.taskSessions.Status(taskID)
	if !status.OK || status.Contract == nil || status.State != "live" {
		return map[string]any{
			"ok":        true,
			"task_id":   taskID,
			"available": false,
			"mode":      "unknown",
			"reason":    "task_session_not_live",
			"state":     status.State,
		}
	}
	if _, err := m.studioManager.ManagedProcessForTask(taskID); err != nil {
		return map[string]any{
			"ok":        true,
			"task_id":   taskID,
			"available": false,
			"mode":      "unknown",
			"reason":    "studio_not_available",
		}
	}
	broker := m.commandBroker.forTask(taskID)
	command, err := broker.enqueueStudioModeQuery(status.Contract.PlaceID)
	if err != nil {
		return map[string]any{
			"ok":        true,
			"task_id":   taskID,
			"available": false,
			"mode":      "unknown",
			"reason":    err.Error(),
		}
	}
	waitCtx, cancel := context.WithTimeout(ctx, 5*time.Second)
	defer cancel()
	terminal, found := broker.waitForTerminal(waitCtx, command.CommandID)
	if !found {
		broker.cancelCommandWithReason(command.CommandID, "mode_query_timeout")
		return map[string]any{
			"ok":         true,
			"task_id":    taskID,
			"available":  false,
			"mode":       "unknown",
			"command_id": command.CommandID,
			"reason":     "mode_query_timeout",
		}
	}
	return taskStudioModeTerminalPayload(taskID, command, terminal)
}

func (m *mcpRuntime) runStudioCommand(ctx context.Context, taskID string, kind mcp2CommandKind) (map[string]any, error) {
	status := m.taskSessions.Status(taskID)
	if !status.OK || status.Contract == nil {
		return nil, fmt.Errorf("helper2 has no registered session for task_id %s", taskID)
	}
	if status.State != "live" {
		return nil, fmt.Errorf("task session is not live: %s", status.State)
	}
	if _, err := m.studioManager.ManagedProcessForTask(taskID); err != nil {
		return nil, fmt.Errorf("studio_not_available: %w", err)
	}
	broker := m.commandBroker.forTask(taskID)
	var command mcp2Command
	var err error
	switch kind {
	case mcp2CommandStudioPlay:
		command, err = broker.enqueueStudioPlay(status.Contract.PlaceID)
	case mcp2CommandStudioStop:
		command, err = broker.enqueueStudioStop(status.Contract.PlaceID)
	default:
		return nil, fmt.Errorf("unsupported studio command kind: %s", kind)
	}
	if err != nil {
		return nil, err
	}
	waitCtx, cancel := context.WithTimeout(ctx, taskStudioCommandWaitTimeout)
	defer cancel()
	terminal, found := broker.waitForTerminal(waitCtx, command.CommandID)
	if !found {
		broker.cancelCommandWithReason(command.CommandID, "command_timeout")
		return nil, fmt.Errorf("mcp2 command did not complete before timeout")
	}
	payload, ok := taskStudioCommandTerminalPayload(taskID, command, terminal)
	if !ok {
		return nil, fmt.Errorf("mcp2 command ended before a response was received: %s", terminal.Reason)
	}
	return payload, nil
}

func (m *mcpRuntime) runScreenshot(ctx context.Context, taskID string) (map[string]any, error) {
	status := m.taskSessions.Status(taskID)
	if !status.OK || status.Contract == nil {
		return nil, fmt.Errorf("helper2 has no registered session for task_id %s", taskID)
	}
	if status.State != "live" {
		return nil, fmt.Errorf("task session is not live: %s", status.State)
	}
	managedStudio, err := m.studioManager.ManagedProcessForTask(taskID)
	if err != nil {
		return nil, fmt.Errorf("studio_not_available: %w", err)
	}
	result, err := screenshot.CaptureStudioScreenshotForExactPID(ctx, managedStudio.PID, "", "task-"+taskID)
	if err != nil {
		m.logger.Warn("failed to capture task Roblox Studio screenshot through MCP", "task_id", taskID, "studio_pid", managedStudio.PID, "error", err)
		return nil, err
	}
	return map[string]any{
		"ok":         true,
		"task_id":    taskID,
		"studio_pid": managedStudio.PID,
		"screenshot": result,
	}, nil
}

func (m *mcpRuntime) readRuntimeLog(taskID string, cursor string, limit int) (map[string]any, error) {
	status := m.taskSessions.Status(taskID)
	if !status.OK || status.Contract == nil {
		return nil, fmt.Errorf("helper2 has no registered session for task_id %s", taskID)
	}
	entries, nextCursor, err := m.runtimeLogs.Read(taskID, cursor, limit)
	if err != nil {
		return nil, err
	}
	return map[string]any{
		"ok":          true,
		"task_id":     taskID,
		"entries":     entries,
		"next_cursor": nextCursor,
	}, nil
}

func taskStudioCommandTerminalPayload(taskID string, command mcp2Command, terminal mcp2CommandTerminal) (map[string]any, bool) {
	if terminal.Result == nil {
		return map[string]any{
			"ok":         false,
			"task_id":    taskID,
			"code":       terminal.Reason,
			"message":    "mcp2 command ended before a response was received",
			"action":     "retry",
			"command_id": command.CommandID,
			"accepted":   false,
			"terminal":   terminal,
		}, false
	}
	return map[string]any{
		"ok":                  terminal.Result.OK,
		"task_id":             taskID,
		"command_id":          command.CommandID,
		"accepted":            terminal.Result.OK,
		"final_mode_verified": false,
		"next_action":         "poll_studio_mode",
		"command_result":      terminal.Result,
		"terminal":            terminal,
	}, terminal.Result.OK
}

func taskStudioModeTerminalPayload(taskID string, command mcp2Command, terminal mcp2CommandTerminal) map[string]any {
	if terminal.Result == nil {
		return map[string]any{
			"ok":         true,
			"task_id":    taskID,
			"available":  false,
			"mode":       "unknown",
			"command_id": command.CommandID,
			"reason":     terminal.Reason,
			"terminal":   terminal,
		}
	}
	mode := terminal.Result.Mode
	if mode == "" && terminal.Result.Result != nil {
		if value, ok := terminal.Result.Result["mode"].(string); ok {
			mode = value
		}
	}
	if mode == "" {
		mode = "unknown"
	}
	response := map[string]any{
		"ok":             true,
		"task_id":        taskID,
		"available":      terminal.Result.OK,
		"mode":           mode,
		"mode_seq":       terminal.Result.ModeSeq,
		"command_id":     command.CommandID,
		"command_result": terminal.Result,
		"terminal":       terminal,
	}
	if terminal.Result.Result != nil {
		if runService, ok := terminal.Result.Result["run_service"]; ok {
			response["run_service"] = runService
		} else if runService, ok := terminal.Result.Result["run_service_flags"]; ok {
			response["run_service"] = runService
		}
	}
	return response
}

func writeMCPResponse(w http.ResponseWriter, response mcpJSONRPCResponse) {
	body, err := json.Marshal(response)
	if err != nil {
		slog.Error("failed to encode mcp response", "error", err)
		w.WriteHeader(http.StatusInternalServerError)
		return
	}
	body = append(body, '\n')
	w.Header().Set("Content-Type", "application/json")
	w.Header().Set("Content-Length", strconv.Itoa(len(body)))
	if _, err := w.Write(body); err != nil {
		slog.Error("failed to write mcp response", "error", err)
	}
}

func mcpToolResult(payload map[string]any) map[string]any {
	body, err := json.Marshal(payload)
	if err != nil {
		body = []byte(fmt.Sprintf(`{"ok":false,"message":%q}`, err.Error()))
	}
	return map[string]any{
		"content": []map[string]any{{"type": "text", "text": string(body)}},
		"isError": false,
	}
}

func mcpToolError(message string) map[string]any {
	return map[string]any{
		"content": []map[string]any{{"type": "text", "text": message}},
		"isError": true,
	}
}

func requiredStringArg(args map[string]any, key string) (string, error) {
	value, ok := args[key]
	if !ok {
		return "", fmt.Errorf("%s is required", key)
	}
	text, ok := value.(string)
	if !ok || strings.TrimSpace(text) == "" {
		return "", fmt.Errorf("%s must be a non-empty string", key)
	}
	return strings.TrimSpace(text), nil
}

func optionalStringArg(args map[string]any, key string) (string, bool) {
	value, ok := args[key]
	if !ok {
		return "", false
	}
	text, ok := value.(string)
	if !ok {
		return "", false
	}
	return strings.TrimSpace(text), true
}

func optionalIntArg(args map[string]any, key string, defaultValue int) int {
	value, ok := args[key]
	if !ok {
		return defaultValue
	}
	switch typed := value.(type) {
	case float64:
		return int(typed)
	case int:
		return typed
	case string:
		parsed, err := strconv.Atoi(strings.TrimSpace(typed))
		if err == nil {
			return parsed
		}
	}
	return defaultValue
}

func mcpToolNames() []string {
	names := make([]string, 0, len(mcpTools()))
	for _, tool := range mcpTools() {
		if name, ok := tool["name"].(string); ok {
			names = append(names, name)
		}
	}
	return names
}

func mcpTools() []map[string]any {
	return []map[string]any{
		mcpTool("helper2_status", "Read helper2 task status.", map[string]any{"task_id": stringSchema()}),
		mcpTool("helper2_studio_play", "Request Studio play through the task-bound mcp2 channel.", map[string]any{"task_id": stringSchema()}),
		mcpTool("helper2_studio_stop", "Request Studio stop through the task-bound mcp2 channel.", map[string]any{"task_id": stringSchema()}),
		mcpTool("helper2_studio_mode", "Read Studio mode through the task-bound mcp2 channel.", map[string]any{"task_id": stringSchema()}),
		mcpTool("helper2_studio_screenshot", "Capture a screenshot from the task-bound Studio process.", map[string]any{"task_id": stringSchema()}),
		mcpTool("helper2_runtime_log", "Read helper2 runtime logs for a task.", map[string]any{
			"task_id": stringSchema(),
			"cursor":  stringSchema(),
			"limit":   map[string]any{"type": "integer"},
		}),
	}
}

func mcpTool(name string, description string, properties map[string]any) map[string]any {
	required := []string{"task_id"}
	return map[string]any{
		"name":        name,
		"description": description,
		"inputSchema": map[string]any{
			"type":                 "object",
			"properties":           properties,
			"required":             required,
			"additionalProperties": true,
		},
	}
}

func stringSchema() map[string]any {
	return map[string]any{"type": "string"}
}
