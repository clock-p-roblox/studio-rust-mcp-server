package main

import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/clock-p/clockbridge/pkg/clockbridge"
)

func TestPublicExposureUsesHelper2MachineDomain(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:    true,
		DryRun:     true,
		ListenAddr: "127.0.0.1:44750",
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	status := manager.Status()
	if status.PublicURL != "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com" {
		t.Fatalf("public URL = %q", status.PublicURL)
	}
	if status.BridgeIdentity != "roblox-helper-win-a-sunjun-user@register-https-proxy.dev.clock-p.com" {
		t.Fatalf("bridge identity = %q", status.BridgeIdentity)
	}
	if status.LocalTargetURL != "http://127.0.0.1:44750/" {
		t.Fatalf("local target URL = %q", status.LocalTargetURL)
	}
	if status.RegisterHost != "register-https-proxy.dev.clock-p.com" {
		t.Fatalf("register host = %q", status.RegisterHost)
	}
}

func TestPublicExposureDryRunDoesNotStartProcess(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:    true,
		DryRun:     true,
		ListenAddr: "127.0.0.1:44750",
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	if err := manager.Start(context.Background()); err != nil {
		t.Fatalf("dry-run start failed: %v", err)
	}
	status := manager.Status()
	if status.State != "dry_run" {
		t.Fatalf("unexpected dry-run status: %+v", status)
	}
}

func TestPublicExposureStartStopCanRepeat(t *testing.T) {
	runner := newBlockingForwardRunner()
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:       true,
		ListenAddr:    "127.0.0.1:44750",
		ForwardRunner: runner.Run,
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	t.Cleanup(manager.Stop)

	for attempt := 0; attempt < 2; attempt++ {
		if err := manager.Start(context.Background()); err != nil {
			t.Fatalf("start attempt %d failed: %v", attempt+1, err)
		}
		runner.WaitForStart(t, attempt+1)
		if status := manager.Status(); status.State != "running" {
			t.Fatalf("unexpected running status on attempt %d: %+v", attempt+1, status)
		}
		manager.Stop()
		waitForPublicExposureState(t, manager, "stopped")
	}
}

func TestPublicExposureStopPreservesExitedError(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:    true,
		ListenAddr: "127.0.0.1:44750",
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	manager.mu.Lock()
	manager.state = "exited"
	manager.lastError = "boom"
	manager.mu.Unlock()

	manager.Stop()
	status := manager.Status()
	if status.State != "exited" || status.LastError != "boom" {
		t.Fatalf("stop should preserve exited error, got %+v", status)
	}
}

func TestPublicExposureStartsEmbeddedForwardWithSystemToken(t *testing.T) {
	runner := newBlockingForwardRunner()
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:       true,
		ListenAddr:    "127.0.0.1:44750",
		ForwardRunner: runner.Run,
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	t.Cleanup(manager.Stop)

	if err := manager.Start(context.Background()); err != nil {
		t.Fatalf("start failed: %v", err)
	}
	got := runner.WaitForStart(t, 1)
	if got.TargetURL != "http://127.0.0.1:44750/" {
		t.Fatalf("target URL = %q", got.TargetURL)
	}
	if got.UUID != "roblox-helper-win-a-sunjun-user" {
		t.Fatalf("uuid = %q", got.UUID)
	}
	if got.RegisterHost != "register-https-proxy.dev.clock-p.com" {
		t.Fatalf("register host = %q", got.RegisterHost)
	}
	if got.RegisterBearerToken != "token" {
		t.Fatalf("bearer token = %q", got.RegisterBearerToken)
	}
	if got.RegisterIP != "" || got.RegisterXToken != "" {
		t.Fatalf("unexpected removed passthrough fields: %+v", got)
	}
}

func TestPublicExposureRequiresSystemIdentityFiles(t *testing.T) {
	cases := []struct {
		name    string
		files   map[string]string
		wantErr string
	}{
		{
			name: "missing machine",
			files: map[string]string{
				"feishu-user_name": "sunjun\n",
				"feishu-token":     "token\n",
			},
			wantErr: "machine_name",
		},
		{
			name: "missing user",
			files: map[string]string{
				"machine_name": "win-a\n",
				"feishu-token": "token\n",
			},
			wantErr: "feishu-user_name",
		},
		{
			name: "missing token",
			files: map[string]string{
				"machine_name":     "win-a\n",
				"feishu-user_name": "sunjun\n",
			},
			wantErr: "feishu-token",
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			dir := t.TempDir()
			for name, content := range tc.files {
				if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o600); err != nil {
					t.Fatalf("write identity file: %v", err)
				}
			}
			_, err := newPublicExposureManager(publicExposureConfig{
				Enabled:     true,
				IdentityDir: dir,
				ListenAddr:  "127.0.0.1:44750",
			}, slog.Default())
			if err == nil || !strings.Contains(err.Error(), tc.wantErr) {
				t.Fatalf("expected error containing %q, got %v", tc.wantErr, err)
			}
		})
	}
}

func publicExposureTestConfig(t *testing.T, config publicExposureConfig) publicExposureConfig {
	t.Helper()
	dir := t.TempDir()
	files := map[string]string{
		"machine_name":     "win-a\n",
		"feishu-user_name": "sunjun\n",
		"feishu-token":     "token\n",
	}
	for name, content := range files {
		if err := os.WriteFile(filepath.Join(dir, name), []byte(content), 0o600); err != nil {
			t.Fatalf("write identity file: %v", err)
		}
	}
	config.IdentityDir = dir
	return config
}

func waitForPublicExposureState(t *testing.T, manager *publicExposureManager, want string) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if status := manager.Status(); status.State == want {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	t.Fatalf("timed out waiting for public exposure state %q, last status %+v", want, manager.Status())
}

type blockingForwardRunner struct {
	mu      sync.Mutex
	started []clockbridge.RemoteForwardConfig
	notify  chan struct{}
}

func newBlockingForwardRunner() *blockingForwardRunner {
	return &blockingForwardRunner{notify: make(chan struct{})}
}

func (r *blockingForwardRunner) Run(ctx context.Context, config clockbridge.RemoteForwardConfig) error {
	r.mu.Lock()
	r.started = append(r.started, config)
	close(r.notify)
	r.notify = make(chan struct{})
	r.mu.Unlock()
	<-ctx.Done()
	return ctx.Err()
}

func (r *blockingForwardRunner) WaitForStart(t *testing.T, count int) clockbridge.RemoteForwardConfig {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for {
		r.mu.Lock()
		if len(r.started) >= count {
			got := r.started[count-1]
			r.mu.Unlock()
			return got
		}
		notify := r.notify
		r.mu.Unlock()
		if !time.Now().Before(deadline) {
			t.Fatalf("timed out waiting for forward start %d", count)
		}
		select {
		case <-notify:
		case <-time.After(50 * time.Millisecond):
		}
	}
}
