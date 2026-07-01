package main

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func writeWorkspaceConfig(t *testing.T) string {
	t.Helper()
	workspace := t.TempDir()
	body := []byte("{\"place_id\":\"123\"}\n")
	if err := os.WriteFile(filepath.Join(workspace, "clock-p.workspace.json"), body, 0o644); err != nil {
		t.Fatalf("write workspace config: %v", err)
	}
	return workspace
}

func TestStartRequiresExplicitMachineName(t *testing.T) {
	workspace := writeWorkspaceConfig(t)
	err := run([]string{"start", "--workspace", workspace})
	if err == nil || !strings.Contains(err.Error(), "--machine_name is required") {
		t.Fatalf("expected machine_name error, got %v", err)
	}
}

func TestStartRequiresWorkspaceConfigPlaceID(t *testing.T) {
	workspace := t.TempDir()
	err := run([]string{"start", "--workspace", workspace, "--machine_name", "win-a"})
	if err == nil || !strings.Contains(err.Error(), "clock-p.workspace.json") {
		t.Fatalf("expected workspace config error, got %v", err)
	}
}

func TestStartMissingCodeSyncFilesGetsHelpfulError(t *testing.T) {
	workspace := writeWorkspaceConfig(t)
	err := run([]string{"start", "--workspace", workspace, "--machine_name", "win-a"})
	if err == nil || !strings.Contains(err.Error(), "Studio node tree") {
		t.Fatalf("expected helpful code-sync config error, got %v", err)
	}
}

func TestStartLocalRequiresExplicitHelperURL(t *testing.T) {
	workspace := writeWorkspaceConfig(t)
	err := run([]string{"start", "--workspace", workspace, "--machine_name", "win-a", "--environment", "local"})
	if err == nil || !strings.Contains(err.Error(), "--helper-url is required") {
		t.Fatalf("expected helper-url error, got %v", err)
	}
}

func TestUnknownCommandFails(t *testing.T) {
	err := run([]string{"launch"})
	if err == nil || !strings.Contains(err.Error(), "unknown command") {
		t.Fatalf("expected unknown command error, got %v", err)
	}
}
