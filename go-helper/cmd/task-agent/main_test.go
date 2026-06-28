package main

import (
	"strings"
	"testing"
)

func TestStartRequiresExplicitMachineName(t *testing.T) {
	err := run([]string{"start", "--place_id", "123", "--helper-base-url", "http://127.0.0.1:44750"})
	if err == nil || !strings.Contains(err.Error(), "--machine_name is required") {
		t.Fatalf("expected machine_name error, got %v", err)
	}
}

func TestStartRequiresExplicitHelperBaseURL(t *testing.T) {
	err := run([]string{"start", "--machine_name", "win-a", "--place_id", "123"})
	if err == nil || !strings.Contains(err.Error(), "--helper-base-url is required") {
		t.Fatalf("expected helper-base-url error, got %v", err)
	}
}

func TestUnknownCommandFails(t *testing.T) {
	err := run([]string{"launch"})
	if err == nil || !strings.Contains(err.Error(), "unknown command") {
		t.Fatalf("expected unknown command error, got %v", err)
	}
}
