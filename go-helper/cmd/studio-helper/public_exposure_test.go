package main

import (
	"context"
	"log/slog"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"testing"
	"time"
)

func TestPublicExposureCommandUsesHelper2MachineDomain(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:        true,
		DryRun:         true,
		ClockbridgeBin: "clockbridge-cli",
		ListenAddr:     "127.0.0.1:44750",
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
	joined := strings.Join(status.Command, " ")
	for _, want := range []string{
		"clockbridge-cli",
		"-i " + filepath.Join(statusIdentityDir(t, manager), "feishu-token"),
		"--proxy-mode legacy_framed",
		"-R http://127.0.0.1:44750/",
		"roblox-helper-win-a-sunjun-user@register-https-proxy.dev.clock-p.com",
	} {
		if !strings.Contains(joined, want) {
			t.Fatalf("command %q missing %q", joined, want)
		}
	}
}

func statusIdentityDir(t *testing.T, manager *publicExposureManager) string {
	t.Helper()
	manager.mu.Lock()
	defer manager.mu.Unlock()
	return manager.config.IdentityDir
}

func TestPublicExposureStatusRedactsXToken(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:        true,
		DryRun:         true,
		ClockbridgeBin: "clockbridge-cli",
		XToken:         "secret-x-token",
		ListenAddr:     "127.0.0.1:44750",
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	joined := strings.Join(manager.Status().Command, " ")
	if strings.Contains(joined, "secret-x-token") {
		t.Fatalf("status command leaked x-token: %q", joined)
	}
	if !strings.Contains(joined, "-x-token <redacted>") {
		t.Fatalf("status command did not show redacted x-token: %q", joined)
	}
}

func TestPublicExposureDryRunDoesNotStartProcess(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:        true,
		DryRun:         true,
		ClockbridgeBin: "definitely-not-a-real-clockbridge-binary",
		ListenAddr:     "127.0.0.1:44750",
	})
	manager, err := newPublicExposureManager(config, slog.Default())
	if err != nil {
		t.Fatalf("public exposure manager failed: %v", err)
	}
	if err := manager.Start(context.Background()); err != nil {
		t.Fatalf("dry-run start failed: %v", err)
	}
	status := manager.Status()
	if status.State != "dry_run" || status.PID != 0 {
		t.Fatalf("unexpected dry-run status: %+v", status)
	}
}

func TestPublicExposureStartStopCanRepeat(t *testing.T) {
	script := writeSleepScript(t)
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:        true,
		ClockbridgeBin: script,
		ListenAddr:     "127.0.0.1:44750",
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
		if status := manager.Status(); status.State != "running" || status.PID == 0 {
			t.Fatalf("unexpected running status on attempt %d: %+v", attempt+1, status)
		}
		manager.Stop()
		waitForPublicExposureState(t, manager, "stopped")
	}
}

func TestPublicExposureStopPreservesExitedError(t *testing.T) {
	config := publicExposureTestConfig(t, publicExposureConfig{
		Enabled:        true,
		ClockbridgeBin: "clockbridge-cli",
		ListenAddr:     "127.0.0.1:44750",
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

func writeSleepScript(t *testing.T) string {
	t.Helper()
	dir := t.TempDir()
	if runtime.GOOS == "windows" {
		path := filepath.Join(dir, "sleep.cmd")
		content := "@echo off\r\nping -n 60 127.0.0.1 >NUL\r\n"
		if err := os.WriteFile(path, []byte(content), 0o755); err != nil {
			t.Fatalf("write sleep script: %v", err)
		}
		return path
	}
	path := filepath.Join(dir, "sleep.sh")
	content := "#!/bin/sh\nsleep 60\n"
	if err := os.WriteFile(path, []byte(content), 0o755); err != nil {
		t.Fatalf("write sleep script: %v", err)
	}
	return path
}
