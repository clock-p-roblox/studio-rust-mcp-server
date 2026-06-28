package main

import (
	"context"
	"testing"
	"time"
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

func TestTaskBrokerRegistryIsolatesTaskQueues(t *testing.T) {
	registry := newMCP2CommandBrokerRegistry()
	taskA := registry.forTask("task-a")
	taskB := registry.forTask("task-b")

	initializeBroker(t, taskA, "edit", 11, 101)
	initializeBroker(t, taskB, "edit", 22, 202)
	commandA, err := taskA.enqueueStudioPlay("111")
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
	commandA, err := taskA.enqueueStudioPlay("111")
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

	commandA, err := taskA.enqueueStudioPlay("111")
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
	waitingCommand, err := broker.enqueueStudioPlay("111")
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

func TestPingDoesNotSwitchLifecycleOrClearQueue(t *testing.T) {
	broker := newMCP2CommandBroker()
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioPlay("111")
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

func TestCleanupPreventsLateResponseFromResurrectingCommand(t *testing.T) {
	registry := newMCP2CommandBrokerRegistry()
	broker := registry.forTask("task-a")
	initializeBroker(t, broker, "edit", 11, 101)
	command, err := broker.enqueueStudioPlay("111")
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
