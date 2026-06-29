package main

import (
	"context"
	"errors"
	"fmt"
	"log/slog"
	"os"
	"path/filepath"
	"regexp"
	"strings"
	"sync"

	"github.com/clock-p/clockbridge/pkg/clockbridge"
)

const defaultPublicDomainSuffix = "dev.clock-p.com"

var publicIdentityValuePattern = regexp.MustCompile(`^[a-z0-9-]+$`)

type publicExposureConfig struct {
	Enabled       bool
	DryRun        bool
	MachineName   string
	UserName      string
	DomainSuffix  string
	TokenFile     string
	IdentityDir   string
	ListenAddr    string
	ForwardRunner func(context.Context, clockbridge.RemoteForwardConfig) error
}

type publicExposureStatus struct {
	Enabled        bool   `json:"enabled"`
	DryRun         bool   `json:"dry_run"`
	State          string `json:"state"`
	PublicURL      string `json:"public_url,omitempty"`
	PublicHost     string `json:"public_host,omitempty"`
	BridgeIdentity string `json:"bridge_identity,omitempty"`
	LocalTargetURL string `json:"local_target_url,omitempty"`
	RegisterHost   string `json:"register_host,omitempty"`
	LastError      string `json:"last_error,omitempty"`
}

type publicExposureManager struct {
	config         publicExposureConfig
	logger         *slog.Logger
	mu             sync.Mutex
	cancel         context.CancelFunc
	generation     int64
	state          string
	lastError      string
	publicHost     string
	publicURL      string
	bridgeIdentity string
	localTargetURL string
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
	return &publicExposureManager{
		config:         resolved,
		logger:         logger,
		state:          "configured",
		publicHost:     publicExposureHost(resolved),
		publicURL:      publicExposureURL(resolved),
		bridgeIdentity: publicExposureBridgeIdentity(resolved),
		localTargetURL: publicExposureLocalTargetURL(resolved.ListenAddr),
	}, nil
}

func resolvePublicExposureConfig(config publicExposureConfig) (publicExposureConfig, error) {
	identity, err := resolveClientIdentity(config.IdentityDir)
	if err != nil {
		return config, err
	}
	config.IdentityDir = identity.Dir
	config.MachineName = identity.MachineName
	config.UserName = identity.UserName
	config.TokenFile = identity.TokenFile
	config.DomainSuffix = strings.Trim(strings.TrimSpace(config.DomainSuffix), ".")
	if config.DomainSuffix == "" {
		config.DomainSuffix = defaultPublicDomainSuffix
	}
	config.ListenAddr = strings.TrimSpace(config.ListenAddr)
	if config.ListenAddr == "" {
		return config, errors.New("helper listen addr is required for public exposure")
	}
	if _, err := listenPort(config.ListenAddr); err != nil {
		return config, fmt.Errorf("helper listen addr is invalid for public exposure: %w", err)
	}
	return config, nil
}

type clientIdentity struct {
	Dir         string
	MachineName string
	UserName    string
	TokenFile   string
}

func resolveClientIdentity(explicitDir string) (clientIdentity, error) {
	candidates := clientIdentityDirs(explicitDir)
	var checked []string
	for _, dir := range candidates {
		if dir == "" {
			continue
		}
		checked = append(checked, dir)
		identity, err := readClientIdentityDir(dir)
		if err == nil {
			return identity, nil
		}
		if explicitDir != "" {
			return clientIdentity{}, err
		}
	}
	return clientIdentity{}, fmt.Errorf("cannot resolve helper2 client identity files machine_name, feishu-user_name, feishu-token; checked %s", strings.Join(checked, ", "))
}

func clientIdentityDirs(explicitDir string) []string {
	if strings.TrimSpace(explicitDir) != "" {
		return []string{strings.TrimSpace(explicitDir)}
	}
	var candidates []string
	if appData := os.Getenv("APPDATA"); appData != "" {
		candidates = append(candidates, filepath.Join(appData, "dev.clock-p.com"))
	}
	if userProfile := os.Getenv("USERPROFILE"); userProfile != "" {
		candidates = append(candidates, filepath.Join(userProfile, ".dev.clock-p.com"))
	}
	if home := os.Getenv("HOME"); home != "" {
		candidates = append(candidates, filepath.Join(home, ".dev.clock-p.com"))
	}
	return candidates
}

func readClientIdentityDir(dir string) (clientIdentity, error) {
	machineName, err := readIdentityValue(filepath.Join(dir, "machine_name"), "machine_name")
	if err != nil {
		return clientIdentity{}, err
	}
	userName, err := readIdentityValue(filepath.Join(dir, "feishu-user_name"), "feishu-user_name")
	if err != nil {
		return clientIdentity{}, err
	}
	tokenFile := filepath.Join(dir, "feishu-token")
	token, err := os.ReadFile(tokenFile)
	if err != nil {
		return clientIdentity{}, fmt.Errorf("cannot read feishu-token from %s: %w", dir, err)
	}
	if strings.TrimSpace(string(token)) == "" {
		return clientIdentity{}, fmt.Errorf("feishu-token is empty in %s", dir)
	}
	return clientIdentity{
		Dir:         dir,
		MachineName: machineName,
		UserName:    userName,
		TokenFile:   tokenFile,
	}, nil
}

func readIdentityValue(path string, name string) (string, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return "", fmt.Errorf("cannot read %s: %w", path, err)
	}
	value := strings.TrimSpace(string(body))
	if value == "" {
		return "", fmt.Errorf("%s is empty: %s", name, path)
	}
	if !publicIdentityValuePattern.MatchString(value) {
		return "", fmt.Errorf("%s must match [a-z0-9-]+: %s", name, value)
	}
	return value, nil
}

func publicExposureHost(config publicExposureConfig) string {
	return fmt.Sprintf("roblox-helper-%s-%s-user.%s", config.MachineName, config.UserName, config.DomainSuffix)
}

func publicExposureURL(config publicExposureConfig) string {
	return "https://" + publicExposureHost(config)
}

func publicExposureBridgeIdentity(config publicExposureConfig) string {
	return fmt.Sprintf("roblox-helper-%s-%s-user@%s", config.MachineName, config.UserName, publicExposureRegisterHost(config))
}

func publicExposureLocalTargetURL(listenAddr string) string {
	port, err := listenPort(listenAddr)
	if err != nil {
		return ""
	}
	return fmt.Sprintf("http://127.0.0.1:%d/", port)
}

func publicExposureRegisterHost(config publicExposureConfig) string {
	domainSuffix := strings.Trim(strings.TrimSpace(config.DomainSuffix), ".")
	if domainSuffix == "" {
		domainSuffix = defaultPublicDomainSuffix
	}
	return "register-https-proxy." + domainSuffix
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
	if m.cancel != nil {
		m.mu.Unlock()
		return nil
	}
	token, err := readPublicBearerToken(m.config.TokenFile)
	if err != nil {
		m.state = "error"
		m.lastError = err.Error()
		m.mu.Unlock()
		return err
	}
	forwardConfig := clockbridge.RemoteForwardConfig{
		TargetURL:           m.localTargetURL,
		UUID:                publicExposureHostPrefix(m.config),
		RegisterHost:        publicExposureRegisterHost(m.config),
		ProxyMode:           clockbridge.ProxyModeLegacyFramed,
		RegisterBearerToken: token,
	}
	runner := m.config.ForwardRunner
	if runner == nil {
		runner = runEmbeddedPublicForward
	}
	forwardCtx, cancel := context.WithCancel(ctx)
	m.cancel = cancel
	m.generation++
	generation := m.generation
	m.state = "starting"
	m.lastError = ""
	m.mu.Unlock()

	m.mu.Lock()
	m.state = "running"
	m.mu.Unlock()
	m.logger.Info("helper2 public exposure started", "public_url", m.publicURL, "target", m.localTargetURL, "identity", m.bridgeIdentity)

	go func() {
		err := runner(forwardCtx, forwardConfig)
		m.mu.Lock()
		defer m.mu.Unlock()
		if m.generation != generation {
			return
		}
		m.cancel = nil
		if err != nil && forwardCtx.Err() == nil {
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
	cancel := m.cancel
	m.cancel = nil
	if m.config.Enabled && !m.config.DryRun && cancel != nil {
		m.state = "stopping"
	} else if m.config.Enabled && !m.config.DryRun && cancel == nil {
		switch m.state {
		case "starting", "running", "stopping", "configured":
			m.state = "stopped"
		}
	}
	m.mu.Unlock()
	if cancel != nil {
		cancel()
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
		RegisterHost:   publicExposureRegisterHost(m.config),
		LastError:      m.lastError,
	}
	return status
}

func publicExposureHostPrefix(config publicExposureConfig) string {
	return fmt.Sprintf("roblox-helper-%s-%s-user", config.MachineName, config.UserName)
}

func runEmbeddedPublicForward(ctx context.Context, config clockbridge.RemoteForwardConfig) error {
	forward, err := clockbridge.NewRemoteForward(config)
	if err != nil {
		return err
	}
	return forward.Run(ctx)
}
