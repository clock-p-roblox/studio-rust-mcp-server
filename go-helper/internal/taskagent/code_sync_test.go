package taskagent

import (
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestBuildCodeSyncBindingUsesTreeConfigOnly(t *testing.T) {
	workspace := t.TempDir()
	body := `{
  "tree": {
    "ReplicatedStorage": {
      "ClockPTest": {
        "$local_path": "src",
        "Nested": {
          "$local_path": "lib"
        }
      }
    }
  }
}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	binding, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err != nil {
		t.Fatalf("BuildCodeSyncBinding failed: %v", err)
	}
	if binding.ProtocolVersion != 2 {
		t.Fatalf("protocol_version = %d, want 2", binding.ProtocolVersion)
	}
	if binding.MappingProfile != codeSyncMappingProfile {
		t.Fatalf("mapping_profile = %q, want %q", binding.MappingProfile, codeSyncMappingProfile)
	}
	if binding.ConfigPath != "code-sync.tree.json" {
		t.Fatalf("config_path = %q", binding.ConfigPath)
	}
	if len(binding.Targets) != 1 || strings.Join(binding.Targets[0].StudioPath, "/") != "ReplicatedStorage/ClockPTest" {
		t.Fatalf("unexpected targets: %+v", binding.Targets)
	}
	if binding.TargetAuthorityHash == "" || binding.CodeSyncConfigHash == "" {
		t.Fatalf("missing hashes: %+v", binding)
	}
}

func TestBuildCodeSyncBindingRejectsUnknownStudioService(t *testing.T) {
	workspace := t.TempDir()
	body := `{"tree":{"NotAService":{"ClockPTest":{"$local_path":"src"}}}}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "supported DataModel service") {
		t.Fatalf("expected supported DataModel service error, got %v", err)
	}
}

func TestBuildCodeSyncBindingAllowsManagedServiceNode(t *testing.T) {
	workspace := t.TempDir()
	body := `{"tree":{"ServerScriptService":{"$local_path":".ts-out/server"}}}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	binding, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err != nil {
		t.Fatalf("BuildCodeSyncBinding failed: %v", err)
	}
	if len(binding.Targets) != 1 || strings.Join(binding.Targets[0].StudioPath, "/") != "ServerScriptService" {
		t.Fatalf("unexpected targets: %+v", binding.Targets)
	}
}

func TestBuildCodeSyncBindingRejectsManagedServiceKind(t *testing.T) {
	workspace := t.TempDir()
	body := `{"tree":{"ServerScriptService":{"$local_path":".ts-out/server","$kind":"Folder"}}}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "managed Studio service nodes cannot declare $kind") {
		t.Fatalf("expected managed service kind error, got %v", err)
	}
}

func TestBuildCodeSyncBindingRejectsUnsupportedManagedServiceNode(t *testing.T) {
	workspace := t.TempDir()
	body := `{"tree":{"Workspace":{"$local_path":"src"}}}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "this Studio service cannot be a managed code-sync node") {
		t.Fatalf("expected unsupported managed service error, got %v", err)
	}
}

func TestBuildCodeSyncBindingRejectsOldRootsConfig(t *testing.T) {
	workspace := t.TempDir()
	body := `{"roots":[]}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "old roots format") {
		t.Fatalf("expected old roots error, got %v", err)
	}
}

func TestBuildCodeSyncBindingRejectsDuplicateJSONKeys(t *testing.T) {
	workspace := t.TempDir()
	body := `{"tree":{"Workspace":{"A":{"$local_path":"a"},"A":{"$local_path":"b"}}}}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "duplicate JSON key") {
		t.Fatalf("expected duplicate key error, got %v", err)
	}
}

func TestBuildCodeSyncBindingRejectsUnknownNodeMetadata(t *testing.T) {
	workspace := t.TempDir()
	body := `{"tree":{"Workspace":{"A":{"$local_path":"a","$incldue":["**/*.lua"]}}}}`
	if err := os.WriteFile(filepath.Join(workspace, "code-sync.tree.json"), []byte(body), 0o644); err != nil {
		t.Fatalf("write code-sync config failed: %v", err)
	}
	_, err := BuildCodeSyncBinding(workspace, "code-sync.tree.json", "win-a", "123")
	if err == nil || !strings.Contains(err.Error(), "unknown code-sync node metadata") {
		t.Fatalf("expected unknown metadata error, got %v", err)
	}
}

func TestTargetAuthorityHashFixture(t *testing.T) {
	hash := targetAuthorityHash([]CodeSyncTargetRoute{
		{StudioPath: []string{"Workspace", "B"}},
		{StudioPath: []string{"ReplicatedStorage", "A"}},
	})
	if hash != "1c92a941b27b5f7cecf24f16cb8b3212d47da2cfc50b91b9aa1064c4523c299f" {
		t.Fatalf("targetAuthorityHash = %q", hash)
	}
}
