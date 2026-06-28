package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"os/exec"
	"strings"
	"sync"
)

const defaultPublicDomainSuffix = "dev.clock-p.com"

type publicExposureConfig struct {
	Enabled        bool
	DryRun         bool
	ClockbridgeBin string
	MachineName    string
	UserName       string
	DomainSuffix   string
	TokenFile      string
	XToken         string
	RegisterHost   string
	RegisterIP     string
	ListenAddr     string
}

type publicExposureStatus struct {
	Enabled        bool     `json:"enabled"`
	DryRun         bool     `json:"dry_run"`
	State          string   `json:"state"`
	PublicURL      string   `json:"public_url,omitempty"`
	PublicHost     string   `json:"public_host,omitempty"`
	BridgeIdentity string   `json:"bridge_identity,omitempty"`
	LocalTargetURL string   `json:"local_target_url,omitempty"`
	RegisterHost   string   `json:"register_host,omitempty"`
	Command        []string `json:"command,omitempty"`
	PID            int      `json:"pid,omitempty"`
	LastError      string   `json:"last_error,omitempty"`
}

type publicExposureManager struct {
	config         publicExposureConfig
	logger         *slog.Logger
	mu             sync.Mutex
	cmd            *exec.Cmd
	job            *publicProcessJob
	state          string
	lastError      string
	publicHost     string
	publicURL      string
	bridgeIdentity string
	localTargetURL string
	command        []string
}

func newPublicExposureManager(config publicExposureConfig, logger *slog.Logger) (*publicExposureManager, error) {
	if logger == nil {
		logger = slog.Default()
	}
	if !config.Enabled {
		return &publicExposureManager{config: config, logger: logger, state: "disabled"}, nil
	}
	resolved, err := resolvePublicExposureConfig(config)
	if err != nil {
		return nil, err
	}
	command := publicExposureCommand(resolved)
	var job *publicProcessJob
	if !resolved.DryRun {
		job, err = newPublicProcessJob()
		if err != nil {
			return nil, err
		}
	}
	return &publicExposureManager{
		config:         resolved,
		logger:         logger,
		job:            job,
		state:          "configured",
		publicHost:     publicExposureHost(resolved),
		publicURL:      publicExposureURL(resolved),
		bridgeIdentity: publicExposureBridgeIdentity(resolved),
		localTargetURL: publicExposureLocalTargetURL(resolved.ListenAddr),
		command:        command,
	}, nil
}

func resolvePublicExposureConfig(config publicExposureConfig) (publicExposureConfig, error) {
	config.ClockbridgeBin = strings.TrimSpace(config.ClockbridgeBin)
	if config.ClockbridgeBin == "" {
		config.ClockbridgeBin = "clockbridge-cli"
	}
	config.MachineName = strings.TrimSpace(config.MachineName)
	if config.MachineName == "" {
		return config, errors.New("--public-machine-name is required when --public-exposure is enabled")
	}
	config.UserName = strings.TrimSpace(config.UserName)
	if config.UserName == "" {
		return config, errors.New("--public-user is required when --public-exposure is enabled")
	}
	config.DomainSuffix = strings.Trim(strings.TrimSpace(config.DomainSuffix), ".")
	if config.DomainSuffix == "" {
		config.DomainSuffix = defaultPublicDomainSuffix
	}
	config.TokenFile = strings.TrimSpace(config.TokenFile)
	if config.TokenFile == "" {
		return config, errors.New("--clockbridge-token-file is required when --public-exposure is enabled")
	}
	config.RegisterHost = strings.TrimSpace(config.RegisterHost)
	if config.RegisterHost == "" {
		config.RegisterHost = "register-https-proxy." + config.DomainSuffix
	}
	config.RegisterIP = strings.TrimSpace(config.RegisterIP)
	config.XToken = strings.TrimSpace(config.XToken)
	config.ListenAddr = strings.TrimSpace(config.ListenAddr)
	if config.ListenAddr == "" {
		return config, errors.New("helper listen addr is required for public exposure")
	}
	if _, err := listenPort(config.ListenAddr); err != nil {
		return config, fmt.Errorf("helper listen addr is invalid for public exposure: %w", err)
	}
	return config, nil
}

func publicExposureHost(config publicExposureConfig) string {
	return fmt.Sprintf("roblox-helper-%s-%s-user.%s", config.MachineName, config.UserName, config.DomainSuffix)
}

func publicExposureURL(config publicExposureConfig) string {
	return "https://" + publicExposureHost(config)
}

func publicExposureBridgeIdentity(config publicExposureConfig) string {
	return fmt.Sprintf("roblox-helper-%s-%s-user@%s", config.MachineName, config.UserName, config.RegisterHost)
}

func publicExposureLocalTargetURL(listenAddr string) string {
	port, err := listenPort(listenAddr)
	if err != nil {
		return ""
	}
	return fmt.Sprintf("http://127.0.0.1:%d/", port)
}

func publicExposureCommand(config publicExposureConfig) []string {
	command := []string{
		config.ClockbridgeBin,
		"-i", config.TokenFile,
		"--proxy-mode", "legacy_framed",
	}
	if config.XToken != "" {
		command = append(command, "-x-token", config.XToken)
	}
	if config.RegisterIP != "" {
		command = append(command, "--register-ip", config.RegisterIP)
	}
	command = append(command,
		"-R", publicExposureLocalTargetURL(config.ListenAddr),
		publicExposureBridgeIdentity(config),
	)
	return command
}

func redactedPublicExposureCommand(command []string) []string {
	redacted := append([]string(nil), command...)
	for index, arg := range redacted {
		if arg == "-x-token" && index+1 < len(redacted) {
			redacted[index+1] = "<redacted>"
			continue
		}
		if strings.HasPrefix(arg, "-x-token=") {
			redacted[index] = "-x-token=<redacted>"
		}
	}
	return redacted
}

func (m *publicExposureManager) Start(ctx context.Context) error {
	m.mu.Lock()
	if !m.config.Enabled {
		m.state = "disabled"
		m.mu.Unlock()
		return nil
	}
	if m.config.DryRun {
		m.state = "dry_run"
		m.mu.Unlock()
		m.logger.Info("helper2 public exposure dry-run configured", "public_url", m.publicURL, "target", m.localTargetURL, "identity", m.bridgeIdentity)
		return nil
	}
	if m.cmd != nil {
		m.mu.Unlock()
		return nil
	}
	if m.job == nil {
		job, err := newPublicProcessJob()
		if err != nil {
			m.state = "error"
			m.lastError = err.Error()
			m.mu.Unlock()
			return err
		}
		m.job = job
	}
	command := append([]string(nil), m.command...)
	cmd := exec.CommandContext(ctx, command[0], command[1:]...)
	cmd.Stdout = os.Stdout
	cmd.Stderr = os.Stderr
	m.state = "starting"

	if err := startPublicManagedCommand(cmd, m.job); err != nil {
		m.cmd = nil
		m.state = "error"
		m.lastError = err.Error()
		m.mu.Unlock()
		return err
	}

	m.cmd = cmd
	m.state = "running"
	pid := cmd.Process.Pid
	m.mu.Unlock()
	m.logger.Info("helper2 public exposure started", "pid", pid, "public_url", m.publicURL, "target", m.localTargetURL, "identity", m.bridgeIdentity)

	go func() {
		err := cmd.Wait()
		m.mu.Lock()
		defer m.mu.Unlock()
		if m.cmd != cmd {
			if m.state == "stopping" {
				m.state = "stopped"
			}
			return
		}
		m.cmd = nil
		if err != nil && ctx.Err() == nil {
			m.state = "exited"
			m.lastError = err.Error()
			m.logger.Warn("helper2 public exposure exited", "error", err)
			return
		}
		m.state = "stopped"
	}()
	return nil
}

func (m *publicExposureManager) Stop() {
	m.mu.Lock()
	cmd := m.cmd
	job := m.job
	m.cmd = nil
	m.job = nil
	if m.config.Enabled && !m.config.DryRun && cmd != nil {
		m.state = "stopping"
	} else if m.config.Enabled && !m.config.DryRun && cmd == nil {
		switch m.state {
		case "starting", "running", "stopping", "configured":
			m.state = "stopped"
		}
	}
	m.mu.Unlock()
	if cmd != nil && cmd.Process != nil {
		stopPublicManagedCommand(cmd)
	}
	if job != nil {
		if err := job.close(); err != nil {
			m.logger.Warn("failed to close public exposure job", "error", err)
		}
	}
}

func (m *publicExposureManager) Status() publicExposureStatus {
	m.mu.Lock()
	defer m.mu.Unlock()
	status := publicExposureStatus{
		Enabled:        m.config.Enabled,
		DryRun:         m.config.DryRun,
		State:          m.state,
		PublicURL:      m.publicURL,
		PublicHost:     m.publicHost,
		BridgeIdentity: m.bridgeIdentity,
		LocalTargetURL: m.localTargetURL,
		RegisterHost:   m.config.RegisterHost,
		Command:        redactedPublicExposureCommand(m.command),
		LastError:      m.lastError,
	}
	if m.cmd != nil && m.cmd.Process != nil {
		status.PID = m.cmd.Process.Pid
	}
	return status
}
