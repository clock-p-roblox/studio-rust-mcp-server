package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/runtimelog"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/tasksession"
)

func initializeBroker(t *testing.T, broker *mcp2CommandBroker, mode string, modeSeq int64, studioPID int) {
	t.Helper()
	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	response, closeConnection := broker.pull(ctx, mode, modeSeq, studioPID, time.Second)
	if closeConnection || response.Type != mcp2MessageTypeShouldRestartPull || response.Reason != "request_context_closed" {
		t.Fatalf("initialize broker response = %+v close=%v, want request_context_closed", response, closeConnection)
	}
}

func testPlayArgs() *studioPlayArgs {
	return &studioPlayArgs{
		LaunchID: 101,
		Data:     map[string]any{"kind": "test"},
	}
}

func TestRojoForwardTargetHTTPURLPreservesPathAndQuery(t *testing.T) {
	target, err := rojoForwardTargetHTTPURL("http://127.0.0.1:5000/base", "api/rojo", "cursor=next&limit=1")
	if err != nil {
		t.Fatalf("target URL failed: %v", err)
	}
	if target != "http://127.0.0.1:5000/base/api/rojo?cursor=next&limit=1" {
		t.Fatalf("target URL = %q", target)
	}

	root, err := rojoForwardTargetHTTPURL("http://127.0.0.1:5000", "", "")
	if err != nil {
		t.Fatalf("root target URL failed: %v", err)
	}
	if root != "http://127.0.0.1:5000/" {
		t.Fatalf("root target URL = %q", root)
	}
}

func TestRojoForwardTargetWSURLUsesWebSocketScheme(t *testing.T) {
	target, err := rojoForwardTargetWSURL("https://example.test/rojo", "api/socket/0", "cursor=next")
	if err != nil {
		t.Fatalf("target WS URL failed: %v", err)
	}
	if target != "wss://example.test/rojo/api/socket/0?cursor=next" {
		t.Fatalf("target WS URL = %q", target)
	}
}

func TestLocalRojoForwardBaseURLUsesHelperPortAndTaskPath(t *testing.T) {
	baseURL, err := localRojoForwardBaseURL("127.0.0.1:44750", "", "1818", "task-a")
	if err != nil {
		t.Fatalf("local base URL failed: %v", err)
	}
	if baseURL != "http://127.0.0.1:44750/rojo-forward/1818/task/task-a" {
		t.Fatalf("local base URL = %q", baseURL)
	}
}

func TestProxyRojoHTTPRequestForcesIdentityEncoding(t *testing.T) {
	msgpackBody := []byte{0x81, 0xaf, 'p', 'r', 'o', 't', 'o', 'c', 'o', 'l', 'V', 'e', 'r', 's', 'i', 'o', 'n', 0x05}
	upstream := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Accept-Encoding"); got != "identity" {
			t.Fatalf("upstream Accept-Encoding = %q, want identity", got)
		}
		w.Header().Set("Content-Type", "application/msgpack")
		_, _ = w.Write(msgpackBody)
	}))
	defer upstream.Close()

	request := httptest.NewRequest(http.MethodGet, "/rojo-forward/113/task/task-a/api/rojo", nil)
	request.Header.Set("Accept-Encoding", "gzip")
	recorder := httptest.NewRecorder()
	status, written, err := proxyRojoHTTPRequest(context.Background(), recorder, request, upstream.URL+"/api/rojo", "")
	if err != nil {
		t.Fatalf("proxy request failed: %v", err)
	}
	if status != http.StatusOK || recorder.Code != http.StatusOK {
		t.Fatalf("status = %d recorder=%d, want 200", status, recorder.Code)
	}
	if written != int64(len(msgpackBody)) {
		t.Fatalf("written = %d, want %d", written, len(msgpackBody))
	}
	if !bytes.Equal(recorder.Body.Bytes(), msgpackBody) {
		t.Fatalf("proxied body = % X, want % X", recorder.Body.Bytes(), msgpackBody)
	}
}

func TestPublicBearerHeaderForRojoUpstream(t *testing.T) {
	tokenFile := filepath.Join(t.TempDir(), "feishu-token")
	if err := os.WriteFile(tokenFile, []byte("secret-token\n"), 0o600); err != nil {
		t.Fatalf("write token file: %v", err)
	}

	localTarget, err := url.Parse("http://127.0.0.1:34872/api/rojo")
	if err != nil {
		t.Fatalf("parse local target: %v", err)
	}
	localHeader, err := publicBearerHeaderForRojoUpstream(localTarget, tokenFile)
	if err != nil {
		t.Fatalf("local target header failed: %v", err)
	}
	if localHeader != "" {
		t.Fatalf("local target header = %q, want empty", localHeader)
	}

	publicTarget, err := url.Parse("https://demo.dev.clock-p.com/api/rojo")
	if err != nil {
		t.Fatalf("parse public target: %v", err)
	}
	publicHeader, err := publicBearerHeaderForRojoUpstream(publicTarget, tokenFile)
	if err != nil {
		t.Fatalf("public target header failed: %v", err)
	}
	if publicHeader != "Bearer secret-token" {
		t.Fatalf("public target header = %q, want bearer token", publicHeader)
	}
}

func TestRojoBindingRejectsCrossTaskOrPlaceRequests(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	registerTestTask(t, runtime, "task-a", "111")
	registerTestTask(t, runtime, "task-b", "111")
	managedStudio := studio.ManagedProcess{
		PlaceID:   "111",
		Source:    studio.LaunchSourceTask,
		OwnerKind: "task",
		OwnerID:   "task-a",
		PID:       101,
	}

	taskMismatch := httptest.NewRecorder()
	if _, ok := resolveRojoPluginBindingForManagedStudio(taskMismatch, managedStudio, runtime.taskSessions, "111", "task-b"); ok {
		t.Fatal("expected cross-task Rojo binding to be rejected")
	}
	if taskMismatch.Code != http.StatusForbidden {
		t.Fatalf("cross-task status = %d, want 403", taskMismatch.Code)
	}
	if payload := decodeJSONMap(t, taskMismatch.Body.Bytes()); payload["code"] != "task_binding_mismatch" {
		t.Fatalf("cross-task payload = %+v, want task_binding_mismatch", payload)
	}

	placeMismatch := httptest.NewRecorder()
	if _, ok := resolveRojoPluginBindingForManagedStudio(placeMismatch, managedStudio, runtime.taskSessions, "222", "task-a"); ok {
		t.Fatal("expected cross-place Rojo binding to be rejected")
	}
	if placeMismatch.Code != http.StatusForbidden {
		t.Fatalf("cross-place status = %d, want 403", placeMismatch.Code)
	}
	if payload := decodeJSONMap(t, placeMismatch.Body.Bytes()); payload["code"] != "place_binding_mismatch" {
		t.Fatalf("cross-place payload = %+v, want place_binding_mismatch", payload)
	}

	success := httptest.NewRecorder()
	binding, ok := resolveRojoPluginBindingForManagedStudio(success, managedStudio, runtime.taskSessions, "111", "task-a")
	if !ok {
		t.Fatalf("expected matching Rojo binding to succeed, response=%s", success.Body.String())
	}
	if binding.TaskID != "task-a" || binding.PlaceID != "111" || binding.StudioPID != 101 {
		t.Fatalf("binding = %+v, want task-a place 111 pid 101", binding)
	}
}

func TestRecordIgnoresInvalidOrStaleResponseResults(t *testing.T) {
	broker := newMCP2CommandBroker()
	modeSeq := int64(101)
	studioPID := 202
	broker.activeMode = "edit"
	broker.activeModeSeq = &modeSeq
	broker.activeStudioPID = &studioPID
	broker.waitingResponseCommand = &mcp2Command{
		CommandID: 1,
		Type:      mcp2MessageTypeCommand,
		Kind:      mcp2CommandStudioMode,
	}

	cases := []struct {
		name      string
		result    mcp2ResponseResult
		studioPID int
		reason    string
	}{
		{
			name:      "invalid source",
			result:    mcp2ResponseResult{CommandID: 1, ModeSeq: modeSeq, OK: true},
			studioPID: 0,
			reason:    "invalid_lifecycle_source",
		},
		{
			name:      "wrong studio pid",
			result:    mcp2ResponseResult{CommandID: 1, ModeSeq: modeSeq, OK: true},
			studioPID: 303,
			reason:    "studio_pid_mismatch",
		},
		{
			name:      "wrong mode seq",
			result:    mcp2ResponseResult{CommandID: 1, ModeSeq: 999, OK: true},
			studioPID: studioPID,
			reason:    "mode_seq_mismatch",
		},
		{
			name:      "not waiting for command",
			result:    mcp2ResponseResult{CommandID: 2, ModeSeq: modeSeq, OK: true},
			studioPID: studioPID,
			reason:    "not_waiting_for_command",
		},
	}

	for _, tc := range cases {
		recorded, reason := broker.record(tc.result, tc.studioPID)
		if recorded {
			t.Fatalf("%s: expected ignored result", tc.name)
		}
		if reason != tc.reason {
			t.Fatalf("%s: expected reason %q, got %q", tc.name, tc.reason, reason)
		}
		if broker.waitingResponseCommand == nil || broker.waitingResponseCommand.CommandID != 1 {
			t.Fatalf("%s: waiting response command was cleared", tc.name)
		}
		if len(broker.results) != 0 {
			t.Fatalf("%s: ignored result was recorded", tc.name)
		}
	}

	recorded, reason := broker.record(mcp2ResponseResult{CommandID: 1, ModeSeq: modeSeq, OK: true}, studioPID)
	if !recorded {
		t.Fatalf("expected valid result to be recorded, reason=%q", reason)
	}
	if broker.waitingResponseCommand != nil {
		t.Fatalf("valid result did not clear waiting response command")
	}
	if len(broker.results) != 1 {
		t.Fatalf("valid result count = %d, want 1", len(broker.results))
	}
	if _, ok := broker.terminals[1]; !ok {
		t.Fatalf("valid result did not complete terminal callback")
	}

	recorded, reason = broker.record(mcp2ResponseResult{CommandID: 1, ModeSeq: modeSeq, OK: true}, studioPID)
	if recorded || reason != "not_waiting_for_command" {
		t.Fatalf("late result should be ignored once command is terminal, recorded=%v reason=%q", recorded, reason)
	}
}

func TestModePayloadAcceptsRunServiceFlagsAlias(t *testing.T) {
	command := mcp2Command{CommandID: 7, Type: mcp2MessageTypeCommand, Kind: mcp2CommandStudioMode}
	terminal := mcp2CommandTerminal{
		CommandID:   7,
		CompletedAt: time.Now(),
		Reason:      "response",
		Result: &mcp2ResponseResult{
			CommandID: 7,
			Mode:      "edit",
			ModeSeq:   123,
			OK:        true,
			Result: map[string]any{
				"run_service_flags": map[string]any{
					"IsEdit": true,
				},
			},
		},
	}
	payload := taskStudioModeTerminalPayload("task-a", command, terminal)
	runService, ok := payload["run_service"].(map[string]any)
	if !ok || runService["IsEdit"] != true {
		t.Fatalf("run_service alias missing from payload: %+v", payload)
	}
}

func TestQueuedTimeoutDoesNotCancelDeliveredCommand(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue command failed: %v", err)
	}
	pulled, closePull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePull || pulled.CommandID != command.CommandID {
		t.Fatalf("pulled command = %+v close=%v, want %d", pulled, closePull, command.CommandID)
	}

	if broker.cancelPendingCommandWithReason(command.CommandID, "queued_timeout") {
		t.Fatalf("queued timeout canceled command after delivery")
	}
	recorded, reason := broker.record(mcp2ResponseResult{CommandID: command.CommandID, ModeSeq: 11, OK: true}, 101)
	if !recorded {
		t.Fatalf("delivered command response was not recorded, reason=%q", reason)
	}
	terminal, ok := broker.waitForTerminal(context.Background(), command.CommandID)
	if !ok || terminal.Reason != "response" || terminal.Result == nil {
		t.Fatalf("terminal = %+v ok=%v, want response", terminal, ok)
	}
}

func TestResponseTimeoutOnlyCancelsWaitingResponseCommand(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue command failed: %v", err)
	}
	pulled, closePull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePull || pulled.CommandID != command.CommandID {
		t.Fatalf("pulled command = %+v close=%v, want %d", pulled, closePull, command.CommandID)
	}

	if !broker.cancelWaitingResponseCommandWithReason(command.CommandID, "response_timeout") {
		t.Fatalf("response timeout did not cancel waiting response command")
	}
	terminal, ok := broker.waitForTerminal(context.Background(), command.CommandID)
	if !ok || terminal.Reason != "response_timeout" || terminal.Result != nil {
		t.Fatalf("terminal = %+v ok=%v, want response_timeout", terminal, ok)
	}
	recorded, reason := broker.record(mcp2ResponseResult{CommandID: command.CommandID, ModeSeq: 11, OK: true}, 101)
	if recorded || reason != "not_waiting_for_command" {
		t.Fatalf("late response recorded after response timeout: recorded=%v reason=%q", recorded, reason)
	}
	if broker.cancelWaitingResponseCommandWithReason(command.CommandID, "response_timeout") {
		t.Fatalf("response timeout canceled terminal command twice")
	}
}

func TestTaskBrokerRegistryIsolatesTaskQueues(t *testing.T) {
	registry := newMCP2CommandBrokerRegistry()
	taskA := registry.forTask("task-a")
	taskB := registry.forTask("task-b")

	initializeBroker(t, taskA, "edit", 11, 101)
	initializeBroker(t, taskB, "edit", 22, 202)
	commandA, err := taskA.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue task A failed: %v", err)
	}
	commandB, err := taskB.enqueueStudioStop("222")
	if err != nil {
		t.Fatalf("enqueue task B failed: %v", err)
	}

	pulledB, closeB := taskB.pull(context.Background(), "edit", 22, 202, time.Second)
	if closeB || pulledB.CommandID != commandB.CommandID {
		t.Fatalf("task B pulled wrong command: response=%+v close=%v want=%d", pulledB, closeB, commandB.CommandID)
	}
	pulledA, closeA := taskA.pull(context.Background(), "edit", 11, 101, time.Second)
	if closeA || pulledA.CommandID != commandA.CommandID {
		t.Fatalf("task A pulled wrong command: response=%+v close=%v want=%d", pulledA, closeA, commandA.CommandID)
	}
}

func TestTaskBrokerCleanupCompletesOnlyThatTask(t *testing.T) {
	registry := newMCP2CommandBrokerRegistry()
	taskA := registry.forTask("task-a")
	taskB := registry.forTask("task-b")

	initializeBroker(t, taskA, "edit", 11, 101)
	initializeBroker(t, taskB, "edit", 22, 202)
	commandA, err := taskA.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue task A failed: %v", err)
	}
	commandB, err := taskB.enqueueStudioStop("222")
	if err != nil {
		t.Fatalf("enqueue task B failed: %v", err)
	}

	registry.removeTask("task-a", "task_released")
	terminal, ok := taskA.waitForTerminal(context.Background(), commandA.CommandID)
	if !ok || terminal.Reason != "task_released" {
		t.Fatalf("task A terminal = %+v ok=%v, want task_released", terminal, ok)
	}
	if summary := taskB.summary(); summary.QueuedCommandCount != 1 {
		t.Fatalf("task B queue was affected by task A cleanup: %+v", summary)
	}
	pulledB, closeB := taskB.pull(context.Background(), "edit", 22, 202, time.Second)
	if closeB || pulledB.CommandID != commandB.CommandID {
		t.Fatalf("task B command was not preserved: response=%+v close=%v", pulledB, closeB)
	}
}

func TestTaskBrokerStaleCompletesOnlyStaleTask(t *testing.T) {
	registry := newMCP2CommandBrokerRegistry()
	taskA := registry.forTask("task-a")
	taskB := registry.forTask("task-b")

	initializeBroker(t, taskA, "edit", 11, 101)
	initializeBroker(t, taskB, "edit", 22, 202)
	staleAt := time.Now().Add(-2 * time.Minute)
	taskA.mu.Lock()
	taskA.lastPullAt = &staleAt
	taskA.mu.Unlock()

	commandA, err := taskA.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue task A failed: %v", err)
	}
	commandB, err := taskB.enqueueStudioStop("222")
	if err != nil {
		t.Fatalf("enqueue task B failed: %v", err)
	}

	stale := registry.markStaleIfNeeded(time.Second)
	if len(stale) != 1 || stale[0].TaskID != "task-a" || stale[0].StudioPID != 101 {
		t.Fatalf("stale channels = %+v, want task-a pid 101 only", stale)
	}
	terminal, ok := taskA.waitForTerminal(context.Background(), commandA.CommandID)
	if !ok || terminal.Reason != "stale" {
		t.Fatalf("task A terminal = %+v ok=%v, want stale", terminal, ok)
	}
	if summary := taskB.summary(); summary.QueuedCommandCount != 1 {
		t.Fatalf("task B queue was affected by task A stale cleanup: %+v", summary)
	}
	pulledB, closeB := taskB.pull(context.Background(), "edit", 22, 202, time.Second)
	if closeB || pulledB.CommandID != commandB.CommandID {
		t.Fatalf("task B command was not preserved: response=%+v close=%v", pulledB, closeB)
	}
}

func TestModeSeqChangeCompletesPendingAndWaitingCommands(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)
	waitingCommand, err := broker.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue waiting command failed: %v", err)
	}
	pulled, closePull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePull || pulled.CommandID != waitingCommand.CommandID {
		t.Fatalf("pulled command = %+v close=%v, want %d", pulled, closePull, waitingCommand.CommandID)
	}
	pendingCommand, err := broker.enqueueStudioStop("111")
	if err != nil {
		t.Fatalf("enqueue pending command failed: %v", err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	response, closeConnection := broker.pull(ctx, "edit", 12, 101, time.Second)
	if closeConnection || response.Type != mcp2MessageTypeShouldRestartPull || response.Reason != "request_context_closed" {
		t.Fatalf("mode seq switch pull response = %+v close=%v, want request_context_closed", response, closeConnection)
	}

	waitingTerminal, ok := broker.waitForTerminal(context.Background(), waitingCommand.CommandID)
	if !ok || waitingTerminal.Reason != "mode_seq_changed" {
		t.Fatalf("waiting terminal = %+v ok=%v, want mode_seq_changed", waitingTerminal, ok)
	}
	pendingTerminal, ok := broker.waitForTerminal(context.Background(), pendingCommand.CommandID)
	if !ok || pendingTerminal.Reason != "mode_seq_changed" {
		t.Fatalf("pending terminal = %+v ok=%v, want mode_seq_changed", pendingTerminal, ok)
	}
	if summary := broker.summary(); summary.QueuedCommandCount != 0 || summary.WaitingResponseCommand != nil {
		t.Fatalf("old commands still active after mode seq change: %+v", summary)
	}
}

func TestPlayServerRejectsStaleEditLifecycleUntilStopRequested(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)

	playCommand, err := broker.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue play failed: %v", err)
	}
	pulledPlay, closePlayPull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePlayPull || pulledPlay.CommandID != playCommand.CommandID {
		t.Fatalf("pulled play command = %+v close=%v, want %d", pulledPlay, closePlayPull, playCommand.CommandID)
	}
	recorded, reason := broker.record(mcp2ResponseResult{
		CommandID: playCommand.CommandID,
		ModeSeq:   11,
		OK:        true,
		Result: map[string]any{
			"status": "play_requested",
		},
	}, 101)
	if !recorded {
		t.Fatalf("play result was not recorded, reason=%q", reason)
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	response, closeConnection := broker.pull(ctx, "play_server", 22, 101, time.Second)
	if closeConnection || response.Type != mcp2MessageTypeShouldRestartPull || response.Reason != "request_context_closed" {
		t.Fatalf("play_server lifecycle response = %+v close=%v, want request_context_closed", response, closeConnection)
	}
	if summary := broker.summary(); summary.ActiveMode != "play_server" || summary.ActiveModeSeq == nil || *summary.ActiveModeSeq != 22 {
		t.Fatalf("play_server lifecycle was not accepted: %+v", summary)
	}

	stopCommand, err := broker.enqueueStudioStop("111")
	if err != nil {
		t.Fatalf("enqueue stop failed: %v", err)
	}
	staleEditResponse, staleEditClosed := broker.pull(ctx, "edit", 11, 101, time.Second)
	if staleEditClosed || staleEditResponse.Type != mcp2MessageTypeShouldRestartPull || staleEditResponse.Reason != "invalid_lifecycle_source" {
		t.Fatalf("stale edit response = %+v close=%v, want invalid_lifecycle_source", staleEditResponse, staleEditClosed)
	}
	if summary := broker.summary(); summary.ActiveMode != "play_server" || summary.ActiveModeSeq == nil || *summary.ActiveModeSeq != 22 || summary.QueuedCommandCount != 1 {
		t.Fatalf("stale edit changed play_server state: %+v", summary)
	}
	pulledStop, closeStopPull := broker.pull(context.Background(), "play_server", 22, 101, time.Second)
	if closeStopPull || pulledStop.CommandID != stopCommand.CommandID {
		t.Fatalf("play_server stop pull = %+v close=%v, want %d", pulledStop, closeStopPull, stopCommand.CommandID)
	}
}

func TestStopRequestedAllowsEditLifecycleToBecomeActive(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "play_server", 22, 101)

	stopCommand, err := broker.enqueueStudioStop("111")
	if err != nil {
		t.Fatalf("enqueue stop failed: %v", err)
	}
	pulledStop, closeStopPull := broker.pull(context.Background(), "play_server", 22, 101, time.Second)
	if closeStopPull || pulledStop.CommandID != stopCommand.CommandID {
		t.Fatalf("pulled stop command = %+v close=%v, want %d", pulledStop, closeStopPull, stopCommand.CommandID)
	}
	recorded, reason := broker.record(mcp2ResponseResult{
		CommandID: stopCommand.CommandID,
		ModeSeq:   22,
		OK:        true,
		Result: map[string]any{
			"status": "stop_requested",
		},
	}, 101)
	if !recorded {
		t.Fatalf("stop result was not recorded, reason=%q", reason)
	}

	ctx, cancel := context.WithCancel(context.Background())
	cancel()
	response, closeConnection := broker.pull(ctx, "edit", 33, 101, time.Second)
	if closeConnection || response.Type != mcp2MessageTypeShouldRestartPull || response.Reason != "request_context_closed" {
		t.Fatalf("edit lifecycle response = %+v close=%v, want request_context_closed", response, closeConnection)
	}
	if summary := broker.summary(); summary.ActiveMode != "edit" || summary.ActiveModeSeq == nil || *summary.ActiveModeSeq != 33 || summary.ExpectedNextMode != "" {
		t.Fatalf("edit lifecycle was not accepted cleanly: %+v", summary)
	}

	stalePlayResponse, stalePlayClosed := broker.pull(ctx, "play_server", 22, 101, time.Second)
	if stalePlayClosed || stalePlayResponse.Type != mcp2MessageTypeShouldRestartPull || stalePlayResponse.Reason != "invalid_lifecycle_source" {
		t.Fatalf("stale play_server response = %+v close=%v, want invalid_lifecycle_source", stalePlayResponse, stalePlayClosed)
	}
	if summary := broker.summary(); summary.ActiveMode != "edit" || summary.ActiveModeSeq == nil || *summary.ActiveModeSeq != 33 {
		t.Fatalf("stale play_server changed edit state: %+v", summary)
	}
}

func TestPingDoesNotSwitchLifecycleOrClearQueue(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue command failed: %v", err)
	}

	response, accepted := broker.ping("edit", 12, 101)
	if accepted || response.Reason != "wrong_lifecycle" {
		t.Fatalf("wrong lifecycle ping accepted: response=%+v accepted=%v", response, accepted)
	}
	if summary := broker.summary(); summary.ActiveModeSeq == nil || *summary.ActiveModeSeq != 11 || summary.QueuedCommandCount != 1 {
		t.Fatalf("wrong lifecycle ping changed broker state: %+v", summary)
	}
	pulled, closePull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePull || pulled.CommandID != command.CommandID {
		t.Fatalf("queued command was not preserved after wrong lifecycle ping: response=%+v close=%v", pulled, closePull)
	}
}

func TestStudioRunCodeRequiresEditMode(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "play_server", 11, 101)
	if _, err := broker.enqueueStudioRunCode("111", "print('no')"); err == nil {
		t.Fatalf("enqueueStudioRunCode succeeded in play_server mode")
	}
}

func TestStudioRunCodeRoutesTaskScopedCommand(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioRunCode("111", "print('hello')")
	if err != nil {
		t.Fatalf("enqueueStudioRunCode failed: %v", err)
	}
	pulled, closePull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePull {
		t.Fatalf("pull unexpectedly closed")
	}
	if pulled.CommandID != command.CommandID || pulled.Kind != mcp2CommandStudioRunCode {
		t.Fatalf("pulled command = %+v, want studio_run_code command_id %d", pulled, command.CommandID)
	}
	args, ok := pulled.Args.(studioRunCodeCommandArgs)
	if !ok {
		t.Fatalf("pulled args type = %T, want studioRunCodeCommandArgs", pulled.Args)
	}
	if args.PlaceID != "111" || args.Code != "print('hello')" {
		t.Fatalf("run code args = %+v", args)
	}
}

func TestCleanupPreventsLateResponseFromResurrectingCommand(t *testing.T) {
	registry := newMCP2CommandBrokerRegistry()
	broker := registry.forTask("task-a")
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioPlay("111", testPlayArgs())
	if err != nil {
		t.Fatalf("enqueue command failed: %v", err)
	}
	pulled, closePull := broker.pull(context.Background(), "edit", 11, 101, time.Second)
	if closePull || pulled.CommandID != command.CommandID {
		t.Fatalf("pulled command = %+v close=%v, want %d", pulled, closePull, command.CommandID)
	}

	registry.removeTask("task-a", "task_released")
	if _, ok := registry.getTask("task-a"); ok {
		t.Fatalf("task broker still registered after release")
	}
	terminal, ok := broker.waitForTerminal(context.Background(), command.CommandID)
	if !ok || terminal.Reason != "task_released" {
		t.Fatalf("terminal = %+v ok=%v, want task_released", terminal, ok)
	}
	recorded, reason := broker.record(mcp2ResponseResult{CommandID: command.CommandID, ModeSeq: 11, OK: true}, 101)
	if recorded || reason != "studio_pid_mismatch" {
		t.Fatalf("late response recorded after cleanup: recorded=%v reason=%q", recorded, reason)
	}
	terminalAfterLateResponse, ok := broker.waitForTerminal(context.Background(), command.CommandID)
	if !ok || terminalAfterLateResponse.Reason != "task_released" || terminalAfterLateResponse.Result != nil {
		t.Fatalf("late response changed terminal: %+v ok=%v", terminalAfterLateResponse, ok)
	}
}

func TestMCPInitializeAndToolsList(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	response := postMCP(t, runtime, map[string]any{
		"jsonrpc": "2.0",
		"id":      1,
		"method":  "initialize",
		"params": map[string]any{
			"protocolVersion": "2025-11-25",
			"capabilities":    map[string]any{},
			"clientInfo":      map[string]any{"name": "test", "version": "1"},
		},
	})
	if response.Code != http.StatusOK {
		t.Fatalf("initialize status = %d", response.Code)
	}
	if response.Header().Get("Mcp-Session-Id") == "" {
		t.Fatalf("initialize did not return Mcp-Session-Id")
	}
	payload := decodeJSONMap(t, response.Body.Bytes())
	if payload["error"] != nil {
		t.Fatalf("initialize returned error: %+v", payload["error"])
	}

	response = postMCP(t, runtime, map[string]any{
		"jsonrpc": "2.0",
		"id":      2,
		"method":  "tools/list",
	})
	payload = decodeJSONMap(t, response.Body.Bytes())
	result, ok := payload["result"].(map[string]any)
	if !ok {
		t.Fatalf("tools/list result missing: %+v", payload)
	}
	tools, ok := result["tools"].([]any)
	if !ok || len(tools) == 0 {
		t.Fatalf("tools/list tools missing: %+v", result)
	}
	if !mcpToolListContains(tools, "helper2_status") || !mcpToolListContains(tools, "helper2_studio_play") || !mcpToolListContains(tools, "helper2_studio_run_code") {
		t.Fatalf("tools/list missing helper2 tools: %+v", tools)
	}
	if !mcpToolListContains(tools, "helper2_official_generate_mesh") || !mcpToolListContains(tools, "helper2_official_wait_job") || !mcpToolListContains(tools, "helper2_official_search_creator_store") {
		t.Fatalf("tools/list missing helper2 official tools: %+v", tools)
	}
	if mcpToolListContains(tools, "launch_studio_session") || mcpToolListContains(tools, "start_stop_play") || mcpToolListContains(tools, "take_screenshot") {
		t.Fatalf("tools/list should not expose legacy helper tool aliases: %+v", tools)
	}
	runCodeTool := mcpToolByName(tools, "helper2_studio_run_code")
	inputSchema, ok := runCodeTool["inputSchema"].(map[string]any)
	if !ok {
		t.Fatalf("run code input schema missing: %+v", runCodeTool)
	}
	required, ok := inputSchema["required"].([]any)
	if !ok || !stringListContainsAny(required, "task_id") || !stringListContainsAny(required, "code") {
		t.Fatalf("run code required schema = %+v, want task_id and code", inputSchema["required"])
	}
	generateMeshTool := mcpToolByName(tools, "helper2_official_generate_mesh")
	generateMeshSchema, ok := generateMeshTool["inputSchema"].(map[string]any)
	if !ok {
		t.Fatalf("generate mesh input schema missing: %+v", generateMeshTool)
	}
	generateMeshRequired, ok := generateMeshSchema["required"].([]any)
	if !ok || !stringListContainsAny(generateMeshRequired, "task_id") || !stringListContainsAny(generateMeshRequired, "text_prompt") {
		t.Fatalf("generate mesh required schema = %+v, want task_id and text_prompt", generateMeshSchema["required"])
	}
}

func TestMCPStatusAndRuntimeLogToolsRequireExplicitTaskID(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	registerTestTask(t, runtime, "task-a", "123")
	if _, err := runtime.runtimeLogs.Append("task-a", runtimelog.Upload{
		RuntimeID:   "server",
		Mode:        "play_server",
		TimestampMS: time.Now().UnixMilli(),
		Level:       "info",
		Message:     "hello",
		Fields:      map[string]any{"session_id": "sess"},
	}); err != nil {
		t.Fatalf("append runtime log failed: %v", err)
	}

	statusResult := callMCPTool(t, runtime, "helper2_status", map[string]any{"task_id": "task-a"})
	statusPayload := decodeToolText(t, statusResult)
	if statusPayload["task_id"] != "task-a" || statusPayload["state"] != "live" {
		t.Fatalf("unexpected status payload: %+v", statusPayload)
	}

	logResult := callMCPTool(t, runtime, "helper2_runtime_log", map[string]any{"task_id": "task-a"})
	logPayload := decodeToolText(t, logResult)
	entries, ok := logPayload["entries"].([]any)
	if !ok || len(entries) != 1 {
		t.Fatalf("unexpected runtime log payload: %+v", logPayload)
	}

	missingTaskResult := callMCPTool(t, runtime, "helper2_status", map[string]any{})
	if missingTaskResult["isError"] != true {
		t.Fatalf("missing task_id should be tool error: %+v", missingTaskResult)
	}
}

func TestMCPStudioToolsRejectWithoutTaskBoundStudio(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	registerTestTask(t, runtime, "task-a", "123")

	result := callMCPTool(t, runtime, "helper2_studio_play", map[string]any{
		"task_id": "task-a",
		"mode":    "start_play",
		"play_args": map[string]any{
			"launch_id": 101,
			"data":      map[string]any{"kind": "test"},
		},
	})
	if result["isError"] != true {
		t.Fatalf("launch without task-bound Studio should be tool error: %+v", result)
	}
	text := toolText(t, result)
	if !strings.Contains(text, "studio_not_available") {
		t.Fatalf("tool error = %q, want studio_not_available", text)
	}

	runCodeResult := callMCPTool(t, runtime, "helper2_studio_run_code", map[string]any{
		"task_id": "task-a",
		"code":    "print('hello')",
	})
	if runCodeResult["isError"] != true {
		t.Fatalf("run code without task-bound Studio should be tool error: %+v", runCodeResult)
	}
	runCodeText := toolText(t, runCodeResult)
	if !strings.Contains(runCodeText, "studio_not_available") {
		t.Fatalf("tool error = %q, want studio_not_available", runCodeText)
	}

	missingCodeResult := callMCPTool(t, runtime, "helper2_studio_run_code", map[string]any{
		"task_id": "task-a",
	})
	if missingCodeResult["isError"] != true {
		t.Fatalf("run code without code should be tool error: %+v", missingCodeResult)
	}
	missingCodeText := toolText(t, missingCodeResult)
	if !strings.Contains(missingCodeText, "code is required") {
		t.Fatalf("tool error = %q, want code is required", missingCodeText)
	}
}

func TestRunCodeTerminalPayloadTreatsCodeFailureAsFailure(t *testing.T) {
	command := mcp2Command{CommandID: 7, Type: mcp2MessageTypeCommand, Kind: mcp2CommandStudioRunCode}
	terminal := mcp2CommandTerminal{
		CommandID: 7,
		Reason:    "response",
		Result: &mcp2ResponseResult{
			CommandID: 7,
			Mode:      "edit",
			ModeSeq:   11,
			OK:        true,
			Result: map[string]any{
				"kind":    "studio_run_code",
				"success": false,
				"stage":   "runtime",
				"error":   "boom",
			},
		},
	}

	payload, ok := taskStudioRunCodeTerminalPayload("task-a", command, terminal)
	if ok {
		t.Fatalf("run code payload ok=true, want false: %+v", payload)
	}
	if payload["ok"] != false || payload["message"] != "boom" {
		t.Fatalf("run code failure payload = %+v", payload)
	}
}

func TestMCPLegacyToolAliasesAreNotAccepted(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	registerTestTask(t, runtime, "task-a", "123")

	result := callMCPTool(t, runtime, "launch_studio_session", map[string]any{
		"task_id": "task-a",
		"mode":    "start_play",
	})
	if result["isError"] != true {
		t.Fatalf("legacy alias should be rejected: %+v", result)
	}
	text := toolText(t, result)
	if !strings.Contains(text, "unknown helper2 MCP tool") {
		t.Fatalf("tool error = %q, want unknown helper2 MCP tool", text)
	}
}

func TestOfficialArgumentMapping(t *testing.T) {
	maxTriangles := 120
	meshArgs, err := officialGenerateMeshArgs(officialGenerateMeshRequest{
		TextPrompt:   "small tree",
		Size:         &officialVector3Request{X: 1, Y: 2, Z: 3},
		MaxTriangles: &maxTriangles,
		PartNames:    "trunk, leaves",
	})
	if err != nil {
		t.Fatalf("generate mesh args failed: %v", err)
	}
	if meshArgs["textPrompt"] != "small tree" || meshArgs["maxTriangles"] != 120 || meshArgs["partNames"] != "trunk, leaves" {
		t.Fatalf("mesh args = %+v", meshArgs)
	}
	size, ok := meshArgs["size"].(map[string]any)
	if !ok || size["x"] != float64(1) || size["y"] != float64(2) || size["z"] != float64(3) {
		t.Fatalf("mesh size args = %+v", meshArgs["size"])
	}

	proceduralArgs, err := officialGenerateProceduralModelArgs(officialGenerateProceduralModelRequest{
		Prompt:           "",
		AttachedImageURI: "IMAGEID_123",
	})
	if err != nil {
		t.Fatalf("procedural args failed: %v", err)
	}
	if proceduralArgs["prompt"] != "" || proceduralArgs["attachedImageUri"] != "IMAGEID_123" {
		t.Fatalf("procedural args = %+v", proceduralArgs)
	}

	searchArgs := officialSearchCreatorStoreArgs(officialSearchCreatorStoreRequest{
		Query:       "tree",
		AssetType:   "Model",
		PriceFilter: "free",
	})
	if searchArgs["scope"] != "creator_store" || searchArgs["assetType"] != "Model" || searchArgs["priceFilter"] != "free" {
		t.Fatalf("search args = %+v", searchArgs)
	}
}

func TestOfficialStudioBindingRequiresExactlyOneStudio(t *testing.T) {
	_, err := chooseSingleOfficialStudio(officialToolMCPResult{
		Content: []officialContent{{Type: "text", Text: `{"studios":[]}`}},
	})
	if err == nil || !strings.Contains(err.Error(), "did not find any connected") {
		t.Fatalf("empty studio list err = %v", err)
	}

	_, err = chooseSingleOfficialStudio(officialToolMCPResult{
		Content: []officialContent{{Type: "text", Text: `{"studios":[{"id":"a","name":"A"},{"id":"b","name":"B"}]}`}},
	})
	if err == nil || !strings.Contains(err.Error(), "multiple Roblox Studio") {
		t.Fatalf("multi studio list err = %v", err)
	}

	studioRef, err := chooseSingleOfficialStudio(officialToolMCPResult{
		Content: []officialContent{{Type: "text", Text: `{"studios":[{"id":"a","name":"A","active":false}]}`}},
	})
	if err != nil || studioRef.ID != "a" || studioRef.Name != "A" {
		t.Fatalf("single studio = %+v err=%v", studioRef, err)
	}
}

func TestOfficialWaitJobTimeoutIsNonTerminalResult(t *testing.T) {
	response := officialToolResponse{
		Tool:          officialToolWaitJobFinished,
		IsError:       true,
		ParsedContent: map[string]any{"status": "Timeout", "lastKnownStatus": "Polling"},
	}
	if !officialWaitJobTimedOut(response.ParsedContent) {
		t.Fatalf("wait job timeout was not detected")
	}
	if officialWaitJobTimedOut(map[string]any{"status": "Failed"}) {
		t.Fatalf("failed status must not be treated as timeout")
	}
	args, err := officialWaitJobArgs(officialWaitJobRequest{GenerationID: "job-a"})
	if err != nil {
		t.Fatalf("wait args failed: %v", err)
	}
	if args["timeout"] != float64(1) {
		t.Fatalf("default wait timeout = %+v, want 1", args)
	}
}

func TestOfficialStudioListNotConnectedDetection(t *testing.T) {
	if !officialStudioListNotConnected(officialToolMCPResult{
		IsError: true,
		Content: []officialContent{{Type: "text", Text: "Not connected to the WS host"}},
	}) {
		t.Fatalf("not connected list result was not detected")
	}
	if officialStudioListNotConnected(officialToolMCPResult{
		IsError: true,
		Content: []officialContent{{Type: "text", Text: "different official error"}},
	}) {
		t.Fatalf("unrelated official error was treated as not connected")
	}
	if !officialStudioListPending(officialToolMCPResult{
		Content: []officialContent{{Type: "text", Text: `{"studios":[]}`}},
	}) {
		t.Fatalf("empty studio list was not treated as pending")
	}
	if officialStudioListPending(officialToolMCPResult{
		Content: []officialContent{{Type: "text", Text: `{"studios":[{"id":"a","name":"A"}]}`}},
	}) {
		t.Fatalf("non-empty studio list was treated as pending")
	}
	if !officialVerifyPlacePending(errors.New("previously active Studio has disconnected")) {
		t.Fatalf("stale active Studio verify error was not treated as pending")
	}
	if officialVerifyPlacePending(errors.New("place_id mismatch")) {
		t.Fatalf("place mismatch must not be treated as pending")
	}
}

func TestOfficialVerifyActiveStudioPlaceRequiresMatchingPlace(t *testing.T) {
	process := &officialProcess{
		stdin:  &testWriteCloser{},
		reader: bufio.NewReader(strings.NewReader(`{"jsonrpc":"2.0","id":9,"result":{"content":[{"type":"text","text":"222"}],"isError":false}}` + "\n")),
		stderr: &bytes.Buffer{},
	}
	_, err := process.verifyActiveStudioPlace(9, "111")
	if err == nil || !strings.Contains(err.Error(), "place_id mismatch") {
		t.Fatalf("place mismatch err = %v", err)
	}
}

func TestMCPOfficialGenerateMeshRejectsPartialSize(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	registerTestTask(t, runtime, "task-a", "123")

	result := callMCPTool(t, runtime, "helper2_official_generate_mesh", map[string]any{
		"task_id":     "task-a",
		"text_prompt": "tree",
		"size_x":      1,
	})
	if result["isError"] != true {
		t.Fatalf("partial size should be tool error: %+v", result)
	}
	if text := toolText(t, result); !strings.Contains(text, "size_x, size_y and size_z") {
		t.Fatalf("tool error = %q, want partial size message", text)
	}
}

func TestOfficialTaskToolFailurePaths(t *testing.T) {
	runtime := newTestMCPRuntime(t)
	payload, statusCode := runOfficialTaskTool(context.Background(), "missing", runtime.taskSessions, runtime.studioManager, runtime.officialRunner, "", map[string]any{})
	if statusCode != http.StatusNotFound || payload["code"] != "task_not_registered" {
		t.Fatalf("missing task payload=%+v status=%d", payload, statusCode)
	}

	registerTestTask(t, runtime, "task-a", "123")
	payload, statusCode = runOfficialTaskTool(context.Background(), "task-a", runtime.taskSessions, runtime.studioManager, runtime.officialRunner, "", map[string]any{})
	if statusCode != http.StatusConflict || payload["code"] != "studio_not_available" {
		t.Fatalf("missing studio payload=%+v status=%d", payload, statusCode)
	}
	if code := officialErrorCode(officialToolBusinessError{response: officialToolResponse{IsError: true}}); code != "official_tool_error" {
		t.Fatalf("official business error code = %s, want official_tool_error", code)
	}
}

func newTestMCPRuntime(t *testing.T) *mcpRuntime {
	t.Helper()
	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{Level: slog.LevelError}))
	manager, err := studio.NewManager(logger)
	if err != nil {
		t.Fatalf("new studio manager failed: %v", err)
	}
	t.Cleanup(func() {
		if err := manager.Close(); err != nil {
			t.Fatalf("close studio manager failed: %v", err)
		}
	})
	logs, err := runtimelog.NewStore(runtimelog.DefaultMaxEntriesPerTask)
	if err != nil {
		t.Fatalf("new runtime log store failed: %v", err)
	}
	t.Cleanup(func() {
		if err := logs.Close(); err != nil {
			t.Fatalf("close runtime log store failed: %v", err)
		}
	})
	return &mcpRuntime{
		taskSessions:   tasksession.NewRegistry(31*time.Second, manager),
		studioManager:  manager,
		commandBroker:  newMCP2CommandBrokerRegistry(),
		runtimeLogs:    logs,
		officialRunner: fakeOfficialRunner{},
		logger:         logger,
	}
}

type fakeOfficialRunner struct{}

func (fakeOfficialRunner) Run(_ context.Context, managedStudio studio.ManagedProcess, toolName string, arguments map[string]any) (officialToolResponse, error) {
	return officialToolResponse{
		Tool:          toolName,
		Studio:        officialStudioRef{ID: "studio-a", Name: "Studio A"},
		IsError:       false,
		ParsedContent: map[string]any{"place_id": managedStudio.PlaceID, "arguments": arguments},
	}, nil
}

type failingOfficialRunner struct{}

func (failingOfficialRunner) Run(_ context.Context, _ studio.ManagedProcess, _ string, _ map[string]any) (officialToolResponse, error) {
	response := officialToolResponse{
		Tool:    "generate_mesh",
		IsError: true,
		Content: []officialContent{{Type: "text", Text: "official failed"}},
	}
	return response, officialToolBusinessError{response: response}
}

type testWriteCloser struct {
	bytes.Buffer
}

func (w *testWriteCloser) Close() error {
	return nil
}

func registerTestTask(t *testing.T, runtime *mcpRuntime, taskID string, placeID string) {
	t.Helper()
	_, err := runtime.taskSessions.Heartbeat(taskID, tasksession.HeartbeatRequest{
		TaskID:               taskID,
		MachineName:          "test-machine",
		PlaceID:              placeID,
		TaskAgentPID:         1234,
		TaskAgentStartedAtMS: time.Now().UnixMilli(),
		RojoUpstreamURL:      "http://127.0.0.1:34872",
	})
	if err != nil {
		t.Fatalf("heartbeat failed: %v", err)
	}
}

func postMCP(t *testing.T, runtime *mcpRuntime, payload map[string]any) *httptest.ResponseRecorder {
	t.Helper()
	body, err := json.Marshal(payload)
	if err != nil {
		t.Fatalf("marshal mcp payload failed: %v", err)
	}
	request := httptest.NewRequest(http.MethodPost, "/mcp", bytes.NewReader(body))
	response := httptest.NewRecorder()
	runtime.handleMCP(response, request)
	return response
}

func callMCPTool(t *testing.T, runtime *mcpRuntime, name string, arguments map[string]any) map[string]any {
	t.Helper()
	response := postMCP(t, runtime, map[string]any{
		"jsonrpc": "2.0",
		"id":      1,
		"method":  "tools/call",
		"params": map[string]any{
			"name":      name,
			"arguments": arguments,
		},
	})
	payload := decodeJSONMap(t, response.Body.Bytes())
	result, ok := payload["result"].(map[string]any)
	if !ok {
		t.Fatalf("tools/call result missing: %+v", payload)
	}
	return result
}

func decodeJSONMap(t *testing.T, body []byte) map[string]any {
	t.Helper()
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		t.Fatalf("decode json failed: %v body=%s", err, string(body))
	}
	return payload
}

func decodeToolText(t *testing.T, result map[string]any) map[string]any {
	t.Helper()
	return decodeJSONMap(t, []byte(toolText(t, result)))
}

func toolText(t *testing.T, result map[string]any) string {
	t.Helper()
	content, ok := result["content"].([]any)
	if !ok || len(content) == 0 {
		t.Fatalf("tool content missing: %+v", result)
	}
	first, ok := content[0].(map[string]any)
	if !ok {
		t.Fatalf("tool content entry invalid: %+v", content[0])
	}
	text, ok := first["text"].(string)
	if !ok {
		t.Fatalf("tool text missing: %+v", first)
	}
	return text
}

func mcpToolListContains(tools []any, name string) bool {
	for _, item := range tools {
		tool, ok := item.(map[string]any)
		if !ok {
			continue
		}
		if tool["name"] == name {
			return true
		}
	}
	return false
}

func mcpToolByName(tools []any, name string) map[string]any {
	for _, item := range tools {
		tool, ok := item.(map[string]any)
		if ok && tool["name"] == name {
			return tool
		}
	}
	return nil
}

func stringListContainsAny(values []any, expected string) bool {
	for _, value := range values {
		if value == expected {
			return true
		}
	}
	return false
}
