package taskagent

import (
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func TestSaveDescriptorDoesNotStoreWorkspace(t *testing.T) {
	workspace := t.TempDir()
	descriptor := Descriptor{
		TaskID:               "t123",
		Environment:          "local",
		MachineName:          "win-a",
		PlaceID:              "123",
		TaskAgentPID:         42,
		TaskAgentStartedAtMS: 1000,
		TaskAgentStatusURL:   "http://127.0.0.1:1/status",
		TaskSessionToken:     "token",
		HelperURL:            "http://127.0.0.1:44750",
		CodeSync:             testCodeSyncBinding(),
	}
	if err := SaveDescriptor(workspace, descriptor); err != nil {
		t.Fatalf("save descriptor failed: %v", err)
	}
	body, err := os.ReadFile(filepath.Join(workspace, ".clock-p", "session.json"))
	if err != nil {
		t.Fatalf("read descriptor failed: %v", err)
	}
	var raw map[string]any
	if err := json.Unmarshal(body, &raw); err != nil {
		t.Fatalf("descriptor is invalid JSON: %v", err)
	}
	if _, ok := raw["workspace"]; ok {
		t.Fatalf("descriptor must not store workspace: %s", string(body))
	}
	loaded, err := LoadDescriptor(workspace)
	if err != nil {
		t.Fatalf("load descriptor failed: %v", err)
	}
	if loaded.TaskID != descriptor.TaskID || loaded.HelperURL != descriptor.HelperURL {
		t.Fatalf("loaded descriptor mismatch: %+v", loaded)
	}
}

func TestResolveHelperURLLocalRequiresExplicitURL(t *testing.T) {
	_, err := ResolveHelperURL(RouteConfig{
		Environment: "local",
		MachineName: "win-a",
	})
	if err == nil || !strings.Contains(err.Error(), "--helper-url is required") {
		t.Fatalf("expected local helper URL error, got %v", err)
	}
}

func TestCodeSyncHashesUseBlake3Fixtures(t *testing.T) {
	scriptHash := blake3Hex(joinBytes(
		canonicalString("clockp.code_sync.v1.script"),
		canonicalString("ModuleScript"),
		canonicalString("Main"),
		canonicalString("return 1\n"),
		canonicalString(0),
	))
	if scriptHash != "6520c8981971c534640f33cd4664b94b77ae9d2b9e6c3d3e7da744f13639a209" {
		t.Fatalf("script hash = %s", scriptHash)
	}
	folderHash := blake3Hex(joinBytes(
		canonicalString("clockp.code_sync.v1.folder"),
		canonicalString("Root"),
		canonicalString(1),
		joinBytes(
			canonicalString("Main"),
			canonicalString("ModuleScript"),
			canonicalString(scriptHash),
		),
	))
	if folderHash != "0fa8f8896db5050d170a1682aff52745b71d44c1dd0b2779e7842e3062873ad4" {
		t.Fatalf("folder hash = %s", folderHash)
	}
}

func TestResolveHelperURLPublicDerivesFromMachineAndUser(t *testing.T) {
	helperURL, err := ResolveHelperURL(RouteConfig{
		Environment: "public",
		MachineName: "win-a",
		UserName:    "sunjun",
	})
	if err != nil {
		t.Fatalf("public helper URL failed: %v", err)
	}
	want := "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com"
	if helperURL != want {
		t.Fatalf("unexpected helper url: %q", helperURL)
	}
}

func TestResolveHelperURLPublicHonorsDomainSuffix(t *testing.T) {
	helperURL, err := ResolveHelperURL(RouteConfig{
		Environment:  "public",
		MachineName:  "win-a",
		UserName:     "sunjun",
		DomainSuffix: "example.test",
	})
	if err != nil {
		t.Fatalf("public helper URL failed: %v", err)
	}
	want := "https://roblox-helper-win-a-sunjun-user.example.test"
	if helperURL != want {
		t.Fatalf("unexpected helper url: %q", helperURL)
	}
}

func TestResolveHelperURLPublicRequiresExplicitIdentity(t *testing.T) {
	emptyHome := t.TempDir()
	t.Setenv("APPDATA", emptyHome)
	t.Setenv("USERPROFILE", emptyHome)
	t.Setenv("HOME", emptyHome)

	_, missingUserErr := ResolveHelperURL(RouteConfig{Environment: "public", MachineName: "win-a"})
	if missingUserErr == nil || !strings.Contains(missingUserErr.Error(), "--user is required") {
		t.Fatalf("expected public user error, got %v", missingUserErr)
	}

	_, missingMachineErr := ResolveHelperURL(RouteConfig{Environment: "public", UserName: "sunjun"})
	if missingMachineErr == nil || !strings.Contains(missingMachineErr.Error(), "machine_name is required") {
		t.Fatalf("expected public machine error, got %v", missingMachineErr)
	}
}

func TestResolveHelperURLPublicNeverReadsMachineNameFile(t *testing.T) {
	dir := t.TempDir()
	identityDir := filepath.Join(dir, "dev.clock-p.com")
	if err := os.MkdirAll(identityDir, 0o755); err != nil {
		t.Fatalf("mkdir identity dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(identityDir, "machine_name"), []byte("file-machine\n"), 0o600); err != nil {
		t.Fatalf("write machine_name: %v", err)
	}
	if err := os.WriteFile(filepath.Join(identityDir, "feishu-user_name"), []byte("sunjun\n"), 0o600); err != nil {
		t.Fatalf("write user: %v", err)
	}
	t.Setenv("APPDATA", dir)
	t.Setenv("USERPROFILE", t.TempDir())
	t.Setenv("HOME", t.TempDir())

	_, err := ResolveHelperURL(RouteConfig{Environment: "public"})
	if err == nil || !strings.Contains(err.Error(), "machine_name is required") {
		t.Fatalf("expected explicit machine_name error, got %v", err)
	}
}

func TestLoadWorkspaceConfig(t *testing.T) {
	workspace := t.TempDir()
	body := []byte("{\"place_id\":\"123\",\"code_sync_config\":\"custom/roots.json\"}\n")
	if err := os.WriteFile(filepath.Join(workspace, workspaceConfigFileName), body, 0o644); err != nil {
		t.Fatalf("write workspace config: %v", err)
	}
	config, err := LoadWorkspaceConfig(workspace)
	if err != nil {
		t.Fatalf("load workspace config failed: %v", err)
	}
	if config.PlaceID != "123" || config.CodeSyncConfig != "custom/roots.json" {
		t.Fatalf("unexpected workspace config: %+v", config)
	}
}

func TestValidateWorkspaceBindingFilesExplainsConfigRole(t *testing.T) {
	workspace := t.TempDir()
	err := ValidateWorkspaceBindingFiles(workspace, WorkspaceConfig{
		PlaceID:        "123",
		CodeSyncConfig: "code-sync.roots.json",
	})
	if err == nil || !strings.Contains(err.Error(), "which local directories code-sync manages") {
		t.Fatalf("expected helpful config message, got %v", err)
	}
}

func TestRequestExistingShutdownUsesStatusURLAndTaskID(t *testing.T) {
	shutdownCalled := false
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch r.URL.Path {
		case "/status":
			_ = json.NewEncoder(w).Encode(StatusResponse{
				OK:                   true,
				TaskID:               "t123",
				TaskAgentPID:         42,
				TaskAgentStartedAtMS: 1000,
			})
		case "/shutdown":
			shutdownCalled = true
			w.WriteHeader(http.StatusOK)
		default:
			w.WriteHeader(http.StatusNotFound)
		}
	}))
	defer server.Close()

	stopped, err := RequestExistingShutdown(server.Client(), Descriptor{
		TaskID:               "t123",
		TaskAgentPID:         42,
		TaskAgentStartedAtMS: 1000,
		TaskAgentStatusURL:   server.URL + "/status",
	}, time.Second)
	if err != nil {
		t.Fatalf("shutdown request failed: %v", err)
	}
	if !stopped || !shutdownCalled {
		t.Fatalf("expected shutdown to be requested, stopped=%v called=%v", stopped, shutdownCalled)
	}
}

func testCodeSyncBinding() CodeSyncBinding {
	return CodeSyncBinding{
		ProtocolVersion:    2,
		WorkspaceID:        "workspace",
		PlaceID:            "123",
		MachineName:        "win-a",
		MappingProfile:     "sync_lua_v1",
		CodeSyncConfigHash: "config",
		RootsAuthorityHash: "roots",
		ConfigPath:         "code-sync.roots.json",
		Roots: []CodeSyncRootRoute{
			{RootID: "root", StudioPath: []string{"Workspace", "ClockPTest"}},
		},
	}
}

func TestRequestExistingShutdownRejectsMismatchedStatusTask(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(StatusResponse{
			OK:                   true,
			TaskID:               "t_other",
			TaskAgentPID:         42,
			TaskAgentStartedAtMS: 1000,
		})
	}))
	defer server.Close()

	stopped, err := RequestExistingShutdown(server.Client(), Descriptor{
		TaskID:               "t123",
		TaskAgentPID:         42,
		TaskAgentStartedAtMS: 1000,
		TaskAgentStatusURL:   server.URL + "/status",
	}, time.Second)
	if err == nil {
		t.Fatal("expected mismatched task to fail")
	}
	if stopped {
		t.Fatal("mismatched task should not be stopped")
	}
}

func TestRequestExistingShutdownRejectsMismatchedProcessIdentity(t *testing.T) {
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		_ = json.NewEncoder(w).Encode(StatusResponse{
			OK:                   true,
			TaskID:               "t123",
			TaskAgentPID:         99,
			TaskAgentStartedAtMS: 1000,
		})
	}))
	defer server.Close()

	stopped, err := RequestExistingShutdown(server.Client(), Descriptor{
		TaskID:               "t123",
		TaskAgentPID:         42,
		TaskAgentStartedAtMS: 1000,
		TaskAgentStatusURL:   server.URL + "/status",
	}, time.Second)
	if err == nil {
		t.Fatal("expected mismatched process identity to fail")
	}
	if stopped {
		t.Fatal("mismatched process identity should not be stopped")
	}
}
