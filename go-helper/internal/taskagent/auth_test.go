package taskagent

import (
	"net/http"
	"net/http/httptest"
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

func TestNewHelperHTTPClientAddsBearerForPublicHelper(t *testing.T) {
	isolateTokenSearchPaths(t)
	workspace := t.TempDir()
	dir := filepath.Join(workspace, ".dev.clock-p.com")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir token dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "feishu-token"), []byte("secret-token\n"), 0o600); err != nil {
		t.Fatalf("write token: %v", err)
	}
	publicClient, err := newHelperHTTPClient("https://roblox-helper-win-a-sunjun-user.dev.clock-p.com", workspace)
	if err != nil {
		t.Fatalf("public helper client failed: %v", err)
	}
	if publicClient.Transport == nil {
		t.Fatal("public helper client did not install bearer transport")
	}
	server := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if got := r.Header.Get("Authorization"); got != "Bearer secret-token" {
			t.Fatalf("Authorization = %q", got)
		}
		w.WriteHeader(http.StatusNoContent)
	}))
	defer server.Close()
	req, err := http.NewRequest(http.MethodPost, server.URL, nil)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}
	resp, err := publicClient.Transport.RoundTrip(req)
	if err != nil {
		t.Fatalf("round trip: %v", err)
	}
	_ = resp.Body.Close()
}

func TestNewHelperHTTPClientKeepsLocalHelperUnauthenticated(t *testing.T) {
	isolateTokenSearchPaths(t)
	localClient, err := newHelperHTTPClient("http://127.0.0.1:44750", t.TempDir())
	if err != nil {
		t.Fatalf("local helper client failed: %v", err)
	}
	if localClient.Transport != nil {
		t.Fatalf("local helper client must not install bearer transport: %#v", localClient.Transport)
	}
}

func TestNewHelperHTTPClientRequiresTokenForPublicHelper(t *testing.T) {
	isolateTokenSearchPaths(t)
	_, err := newHelperHTTPClient("https://roblox-helper-win-a-sunjun-user.dev.clock-p.com", t.TempDir())
	if err != nil {
		return
	}
	t.Fatal("expected missing token error")
}

func TestNewHelperHTTPClientDoesNotTreatOtherHTTPSAsHelper(t *testing.T) {
	isolateTokenSearchPaths(t)
	client, err := newHelperHTTPClient("https://example.com", t.TempDir())
	if err != nil {
		t.Fatalf("other HTTPS client failed: %v", err)
	}
	if client.Transport != nil {
		t.Fatalf("other HTTPS client must not install bearer transport: %#v", client.Transport)
	}
}

func isolateTokenSearchPaths(t *testing.T) {
	t.Helper()
	empty := t.TempDir()
	t.Setenv("APPDATA", empty)
	t.Setenv("USERPROFILE", empty)
	t.Setenv("HOME", empty)
}
