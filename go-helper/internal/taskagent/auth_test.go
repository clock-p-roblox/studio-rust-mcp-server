package taskagent

import (
	"io"
	"net/http"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

type captureTransport struct {
	header http.Header
}

func (t *captureTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	t.header = req.Header.Clone()
	return &http.Response{
		StatusCode: http.StatusOK,
		Body:       io.NopCloser(strings.NewReader("")),
		Header:     make(http.Header),
	}, nil
}

func TestPublicHelperBaseURLDetection(t *testing.T) {
	if !isPublicHelperBaseURL("https://roblox-helper-win-a-sunjun-user.dev.clock-p.com") {
		t.Fatal("expected dev.clock-p.com helper URL to be public")
	}
	if isPublicHelperBaseURL("http://127.0.0.1:44750") {
		t.Fatal("local helper URL must not be treated as public")
	}
	if isPublicHelperBaseURL("https://example.com") {
		t.Fatal("non clock-p host must not be treated as public")
	}
}

func TestResolveBearerTokenPrefersEnvironment(t *testing.T) {
	t.Setenv(helperBearerTokenEnv, "from-env")
	token, err := ResolveBearerToken(t.TempDir())
	if err != nil {
		t.Fatalf("resolve token failed: %v", err)
	}
	if token != "from-env" {
		t.Fatalf("unexpected token %q", token)
	}
}

func TestResolveBearerTokenFallsBackToWorkspaceFile(t *testing.T) {
	t.Setenv(helperBearerTokenEnv, "")
	isolateTokenSearchPaths(t)
	workspace := t.TempDir()
	dir := filepath.Join(workspace, ".dev.clock-p.com")
	if err := os.MkdirAll(dir, 0o755); err != nil {
		t.Fatalf("mkdir token dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(dir, "feishu-token"), []byte("from-file\n"), 0o600); err != nil {
		t.Fatalf("write token: %v", err)
	}
	token, err := ResolveBearerToken(workspace)
	if err != nil {
		t.Fatalf("resolve token failed: %v", err)
	}
	if token != "from-file" {
		t.Fatalf("unexpected token %q", token)
	}
}

func TestNewHelperHTTPClientInjectsBearerOnlyForPublicHelper(t *testing.T) {
	isolateTokenSearchPaths(t)
	t.Setenv(helperBearerTokenEnv, "")
	publicClient, err := newHelperHTTPClient("https://roblox-helper-win-a-sunjun-user.dev.clock-p.com", t.TempDir())
	if err == nil {
		t.Fatal("expected public helper client to require token")
	}
	t.Setenv(helperBearerTokenEnv, "secret")
	publicClient, err = newHelperHTTPClient("https://roblox-helper-win-a-sunjun-user.dev.clock-p.com", t.TempDir())
	if err != nil {
		t.Fatalf("public helper client failed: %v", err)
	}
	capture := &captureTransport{}
	publicClient.Transport = bearerTransport{base: capture, token: "secret"}
	req, err := http.NewRequest(http.MethodGet, "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com/status", nil)
	if err != nil {
		t.Fatalf("new request: %v", err)
	}
	resp, err := publicClient.Do(req)
	if err != nil {
		t.Fatalf("public request failed: %v", err)
	}
	resp.Body.Close()
	if got := capture.header.Get("Authorization"); got != "Bearer secret" {
		t.Fatalf("authorization header = %q", got)
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
