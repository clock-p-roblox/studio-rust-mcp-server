package taskagent

import (
	"context"
	"encoding/json"
	"log/slog"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/clock-p/clockbridge/pkg/clockbridge"
)

func TestAgentRegistersRojoPublicDomain(t *testing.T) {
	workspace := t.TempDir()
	tokenDir := filepath.Join(workspace, ".dev.clock-p.com")
	if err := os.MkdirAll(tokenDir, 0o755); err != nil {
		t.Fatalf("mkdir token dir: %v", err)
	}
	if err := os.WriteFile(filepath.Join(tokenDir, "feishu-token"), []byte("token\n"), 0o600); err != nil {
		t.Fatalf("write token: %v", err)
	}
	rojoBin := writeBlockingScript(t)
	forward := newTaskAgentForwardRecorder()

	heartbeatCh := make(chan map[string]any, 4)
	helper := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		switch {
		case r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, "/heartbeat"):
			var payload map[string]any
			if err := json.NewDecoder(r.Body).Decode(&payload); err != nil {
				t.Fatalf("decode heartbeat: %v", err)
			}
			heartbeatCh <- payload
			_ = json.NewEncoder(w).Encode(map[string]any{"ok": true})
		case r.Method == http.MethodPost && strings.HasSuffix(r.URL.Path, "/release"):
			_ = json.NewEncoder(w).Encode(map[string]any{"ok": true})
		default:
			http.NotFound(w, r)
		}
	}))
	defer helper.Close()

	agent, err := New(Config{
		Workspace:         workspace,
		Environment:       "local",
		MachineName:       "sunjun2",
		UserName:          "sunjun",
		PlaceID:           "113577273791190",
		HelperBaseURL:     helper.URL,
		RegisterDomain:    true,
		DomainSuffix:      "dev.clock-p.com",
		RojoBin:           rojoBin,
		ProjectPath:       workspace,
		RojoForwardRunner: forward.Run,
	}, slog.Default())
	if err != nil {
		t.Fatalf("new agent failed: %v", err)
	}
	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan error, 1)
	go func() {
		done <- agent.Run(ctx)
	}()
	t.Cleanup(func() {
		cancel()
		select {
		case err := <-done:
			if err != nil {
				t.Fatalf("agent run returned error: %v", err)
			}
		case <-time.After(5 * time.Second):
			t.Fatalf("agent did not stop")
		}
	})

	gotForward := forward.WaitForStart(t)
	if gotForward.TargetURL == "" || !strings.HasPrefix(gotForward.TargetURL, "http://127.0.0.1:") {
		t.Fatalf("forward target URL = %q", gotForward.TargetURL)
	}
	if gotForward.UUID == "" || !strings.Contains(gotForward.UUID, "-rojo-sunjun-user") {
		t.Fatalf("forward UUID = %q", gotForward.UUID)
	}
	if gotForward.RegisterHost != "register-https-proxy.dev.clock-p.com" {
		t.Fatalf("register host = %q", gotForward.RegisterHost)
	}
	if gotForward.RegisterBearerToken != "token" {
		t.Fatalf("register token = %q", gotForward.RegisterBearerToken)
	}

	descriptor, err := LoadDescriptor(workspace)
	if err != nil {
		t.Fatalf("load descriptor: %v", err)
	}
	if !strings.HasPrefix(descriptor.Rojo.UpstreamURL, "http://127.0.0.1:") {
		t.Fatalf("descriptor Rojo upstream URL = %q", descriptor.Rojo.UpstreamURL)
	}
	if !strings.HasPrefix(descriptor.Rojo.PublicURL, "https://113577273791190-") || !strings.Contains(descriptor.Rojo.PublicURL, "-rojo-sunjun-user.dev.clock-p.com") {
		t.Fatalf("descriptor Rojo public URL = %q", descriptor.Rojo.PublicURL)
	}

	select {
	case heartbeat := <-heartbeatCh:
		if heartbeat["rojo_upstream_url"] != descriptor.Rojo.UpstreamURL {
			t.Fatalf("heartbeat Rojo upstream = %v, descriptor = %q", heartbeat["rojo_upstream_url"], descriptor.Rojo.UpstreamURL)
		}
	case <-time.After(10 * time.Second):
		t.Fatalf("timed out waiting for heartbeat")
	}
}

type taskAgentForwardRecorder struct {
	mu      sync.Mutex
	started *clockbridge.RemoteForwardConfig
	notify  chan struct{}
}

func newTaskAgentForwardRecorder() *taskAgentForwardRecorder {
	return &taskAgentForwardRecorder{notify: make(chan struct{})}
}

func (r *taskAgentForwardRecorder) Run(ctx context.Context, config clockbridge.RemoteForwardConfig) error {
	r.mu.Lock()
	copy := config
	r.started = &copy
	close(r.notify)
	r.notify = make(chan struct{})
	r.mu.Unlock()
	<-ctx.Done()
	return ctx.Err()
}

func (r *taskAgentForwardRecorder) WaitForStart(t *testing.T) clockbridge.RemoteForwardConfig {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		r.mu.Lock()
		if r.started != nil {
			got := *r.started
			r.mu.Unlock()
			return got
		}
		notify := r.notify
		r.mu.Unlock()
		if !time.Now().Before(deadline) {
			t.Fatalf("timed out waiting for Rojo public forward start")
		}
		select {
		case <-notify:
		case <-time.After(50 * time.Millisecond):
		}
	}
}

func writeBlockingScript(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	if runtime.GOOS == "windows" {
		path := filepath.Join(dir, "block.cmd")
		if err := os.WriteFile(path, []byte("@echo off\r\nping -n 60 127.0.0.1 >NUL\r\n"), 0o755); err != nil {
			t.Fatalf("write blocking script: %v", err)
		}
		return path
	}
	path := filepath.Join(dir, "block.sh")
	if err := os.WriteFile(path, []byte("#!/bin/sh\nsleep 60\n"), 0o755); err != nil {
		t.Fatalf("write blocking script: %v", err)
	}
	return path
}
