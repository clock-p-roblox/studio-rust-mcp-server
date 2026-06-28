package taskagent

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestPublicAgentReportsPublicRojoUpstream(t *testing.T) {
	heartbeatCh := make(chan map[string]any, 1)
	helper := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, "/heartbeat"):
			var body map[string]any
			if err := json.NewDecoder(r.Body).Decode(&body); err != nil {
				t.Errorf("heartbeat body decode failed: %v", err)
				w.WriteHeader(http.StatusBadRequest)
				return
			}
			select {
			case heartbeatCh <- body:
			default:
			}
			writeJSON(w, http.StatusOK, map[string]any{"ok": true})
		case r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, "/release"):
			writeJSON(w, http.StatusOK, map[string]any{"ok": true})
		default:
			w.WriteHeader(http.StatusNotFound)
		}
	}))
	defer helper.Close()

	workspace := t.TempDir()
	fakeBin := writeSleepingProcess(t, workspace, "fake-process")
	agent, err := New(Config{
		Workspace:             workspace,
		Environment:           "public",
		MachineName:           "win-a",
		UserName:              "tester",
		PlaceID:               "123456",
		HelperBaseURL:         helper.URL,
		HelperPublicURL:       "https://roblox-helper-win-a-tester-user.dev.clock-p.com",
		RojoBin:               fakeBin,
		ClockbridgeBin:        fakeBin,
		ClockbridgeTokenFile:  filepath.Join(workspace, "token.txt"),
		ClockbridgeRegisterIP: "127.0.0.1",
	}, slog.New(slog.NewTextHandler(os.Stderr, nil)))
	if err != nil {
		t.Fatalf("new agent failed: %v", err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	done := make(chan error, 1)
	go func() {
		done <- agent.Run(ctx)
	}()
	defer func() {
		cancel()
		select {
		case err := <-done:
			if err != nil {
				t.Fatalf("agent run failed: %v", err)
			}
		case <-time.After(5 * time.Second):
			t.Fatal("timed out waiting for agent shutdown")
		}
	}()

	var descriptor Descriptor
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		descriptor, err = LoadDescriptor(workspace)
		if err == nil {
			break
		}
		time.Sleep(50 * time.Millisecond)
	}
	if err != nil {
		t.Fatalf("descriptor was not written: %v", err)
	}
	wantRojoURL := "https://123456-" + descriptor.TaskID + "-rojo-tester-user.dev.clock-p.com"
	if descriptor.Rojo.UpstreamURL != wantRojoURL {
		t.Fatalf("descriptor did not use public Rojo upstream: %+v", descriptor.Rojo)
	}
	if strings.HasPrefix(descriptor.Rojo.UpstreamURL, "http://127.0.0.1:") {
		t.Fatalf("descriptor leaked local Rojo upstream: %+v", descriptor.Rojo)
	}

	select {
	case heartbeat := <-heartbeatCh:
		if heartbeat["rojo_upstream_url"] != wantRojoURL {
			t.Fatalf("heartbeat Rojo upstream mismatch: %#v", heartbeat)
		}
	case <-time.After(6 * time.Second):
		t.Fatal("timed out waiting for helper heartbeat")
	}
}

func writeSleepingProcess(t *testing.T, dir string, name string) string {
	t.Helper()
	source := filepath.Join(dir, name+".go")
	binary := filepath.Join(dir, name)
	if runtime.GOOS == "windows" {
		binary += ".exe"
	}
	body := "package main\n\nimport \"time\"\n\nfunc main() { time.Sleep(60 * time.Second) }\n"
	if err := os.WriteFile(source, []byte(body), 0o644); err != nil {
		t.Fatalf("write fake process source failed: %v", err)
	}
	cmd := exec.Command("go", "build", "-o", binary, source)
	output, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("build fake process failed: %v\n%s", err, string(output))
	}
	return binary
}
