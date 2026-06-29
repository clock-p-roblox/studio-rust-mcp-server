package taskagent

import (
	"os"
	"path/filepath"
	"testing"
)

func TestResolveClockbridgeRegisterTokenUsesWorkspaceFile(t *testing.T) {
	isolateTokenSearchPaths(t)
	workspace := t.TempDir()
	dir := filepath.Join(workspace, ".dev.clock-p.com")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir token dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "feishu-token"), []byte("from-file\n"), 0o600); err != nil {
		t.Fatalf("write token: %v", err)
	}
	token, err := ResolveClockbridgeRegisterToken(workspace)
	if err != nil {
		t.Fatalf("resolve token failed: %v", err)
	}
	if token != "from-file" {
		t.Fatalf("unexpected token %q", token)
	}
}

func TestNewHelperHTTPClientDoesNotRequireBearerForPublicHelper(t *testing.T) {
	isolateTokenSearchPaths(t)
	publicClient, err := newHelperHTTPClient("https://roblox-helper-win-a-sunjun-user.dev.clock-p.com", t.TempDir())
	if err != nil {
		t.Fatalf("public helper client failed: %v", err)
	}
	if publicClient.Transport != nil {
		t.Fatalf("public helper client must not install bearer transport: %#v", publicClient.Transport)
	}

	localClient, err := newHelperHTTPClient("http://127.0.0.1:44750", t.TempDir())
	if err != nil {
		t.Fatalf("local helper client failed: %v", err)
	}
	if localClient.Transport != nil {
		t.Fatalf("local helper client must not install bearer transport: %#v", localClient.Transport)
	}
}

func isolateTokenSearchPaths(t *testing.T) {
	t.Helper()
	empty := t.TempDir()
	t.Setenv("APPDATA", empty)
	t.Setenv("USERPROFILE", empty)
	t.Setenv("HOME", empty)
}
