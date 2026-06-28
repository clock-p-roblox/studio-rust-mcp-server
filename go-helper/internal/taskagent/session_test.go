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
		Helper:               HelperRoute{BaseURL: "http://127.0.0.1:44750"},
		Rojo:                 RojoRoute{LocalURL: "http://127.0.0.1:5000", UpstreamURL: "http://127.0.0.1:5000"},
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
	if loaded.TaskID != descriptor.TaskID || loaded.Helper.BaseURL != descriptor.Helper.BaseURL {
		t.Fatalf("loaded descriptor mismatch: %+v", loaded)
	}
}

func TestResolveHelperBaseURLLocalRequiresExplicitURL(t *testing.T) {
	_, _, err := ResolveHelperBaseURL(RouteConfig{
		Environment: "local",
		MachineName: "win-a",
	})
	if err == nil || !strings.Contains(err.Error(), "--helper-base-url is required") {
		t.Fatalf("expected local helper URL error, got %v", err)
	}
}

func TestResolveHelperBaseURLPublicDerivesFromMachineAndUser(t *testing.T) {
	baseURL, publicURL, err := ResolveHelperBaseURL(RouteConfig{
		Environment: "public",
		MachineName: "win-a",
		UserName:    "sunjun",
	})
	if err != nil {
		t.Fatalf("public helper URL failed: %v", err)
	}
	want := "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com"
	if baseURL != want || publicURL != want {
		t.Fatalf("unexpected public URLs: base=%q public=%q", baseURL, publicURL)
	}
}

func TestResolveRojoPublicRouteUsesTaskID(t *testing.T) {
	baseURL, identity, err := ResolveRojoPublicRoute("123456", "tdemo", "tester")
	if err != nil {
		t.Fatalf("public Rojo route failed: %v", err)
	}
	if baseURL != "https://123456-tdemo-rojo-tester-user.dev.clock-p.com" {
		t.Fatalf("unexpected Rojo public URL: %s", baseURL)
	}
	if identity != "123456-tdemo-rojo-tester-user.dev.clock-p.com@register-https-proxy.dev.clock-p.com" {
		t.Fatalf("unexpected Rojo bridge identity: %s", identity)
	}
}

func TestClockbridgeArgsPreserveOldProxyShape(t *testing.T) {
	args := clockbridgeArgs("token.txt", "http://127.0.0.1:34872", "127.0.0.1", "identity.example")
	want := []string{"-i", "token.txt", "-R", "http://127.0.0.1:34872/", "--register-ip=127.0.0.1", "identity.example"}
	if strings.Join(args, "\n") != strings.Join(want, "\n") {
		t.Fatalf("unexpected clockbridge args: %#v", args)
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
