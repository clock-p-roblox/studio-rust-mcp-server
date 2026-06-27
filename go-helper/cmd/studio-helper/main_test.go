package main

import "testing"

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
}
