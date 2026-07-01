package taskagent

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestBuildCodeSyncBindingUsesRootsConfigOnly(t *testing.T) {
	workspace := t.TempDir()
	body := `{
  "roots": [
    {
      "root_id": "app",
      "local_path": "src",
      "studio_path": ["ReplicatedStorage", "ClockPTest"]
    }
  ]
}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.roots.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	binding, err := BuildCodeSyncBinding(workspace, "code-sync.roots.json", "win-a", "123")
	if err != nil {
		t.Fatalf("BuildCodeSyncBinding failed: %v", err)
	}
	if binding.ProtocolVersion != 2 {
		t.Fatalf("protocol_version = %d, want 2", binding.ProtocolVersion)
	}
	if binding.MappingProfile != codeSyncMappingProfile {
		t.Fatalf("mapping_profile = %q, want %q", binding.MappingProfile, codeSyncMappingProfile)
	}
	if binding.ConfigPath != "code-sync.roots.json" {
		t.Fatalf("config_path = %q", binding.ConfigPath)
	}
	if len(binding.Roots) != 1 || binding.Roots[0].RootID != "app" {
		t.Fatalf("unexpected roots: %+v", binding.Roots)
	}
}

func TestBuildCodeSyncBindingRejectsUnknownStudioService(t *testing.T) {
	workspace := t.TempDir()
	body := `{
  "roots": [
    {
      "root_id": "app",
      "local_path": "src",
      "studio_path": ["NotAService", "ClockPTest"]
    }
  ]
}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.roots.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.roots.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "supported DataModel service") {
		t.Fatalf("expected supported DataModel service error, got %v", err)
	}
}
