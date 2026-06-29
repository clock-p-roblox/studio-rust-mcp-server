package taskagent

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"strings"
	"sync"
	"time"

	"github.com/clock-p/clockbridge/pkg/clockbridge"
)

const heartbeatInterval = 5 * time.Second
const heartbeatHTTPTimeout = 5 * time.Second

var placeIDPattern = regexp.MustCompile(`^[0-9]+$`)

type Config struct {
	Workspace         string
	Environment       string
	MachineName       string
	UserName          string
	PlaceID           string
	HelperBaseURL     string
	HelperPublicURL   string
	RegisterDomain    bool
	DomainSuffix      string
	RojoBin           string
	ProjectPath       string
	StatusAddr        string
	RojoForwardRunner func(context.Context, clockbridge.RemoteForwardConfig) error
}

type StatusResponse struct {
	OK                       bool       `json:"ok"`
	TaskID                   string     `json:"task_id"`
	MachineName              string     `json:"machine_name"`
	PlaceID                  string     `json:"place_id"`
	Workspace                string     `json:"workspace"`
	TaskAgentPID             int        `json:"task_agent_pid"`
	TaskAgentStartedAtMS     int64      `json:"task_agent_started_at_ms"`
	TaskAgentStatusURL       string     `json:"task_agent_status_url"`
	HelperBaseURL            string     `json:"helper_base_url"`
	HelperRegistrationState  string     `json:"helper_registration_state"`
	HelperLastHeartbeatAt    *time.Time `json:"helper_last_heartbeat_at,omitempty"`
	HelperLastHeartbeatError string     `json:"helper_last_heartbeat_error,omitempty"`
	Rojo                     RojoStatus `json:"rojo"`
}

type RojoStatus struct {
	State       string     `json:"state"`
	PID         int        `json:"pid,omitempty"`
	LocalURL    string     `json:"local_url"`
	UpstreamURL string     `json:"upstream_url"`
	PublicURL   string     `json:"public_url,omitempty"`
	PublicState string     `json:"public_state,omitempty"`
	Restarts    int        `json:"restarts"`
	LastExitAt  *time.Time `json:"last_exit_at,omitempty"`
	LastError   string     `json:"last_error,omitempty"`
	PublicError string     `json:"public_error,omitempty"`
}

type Agent struct {
	config       Config
	descriptor   Descriptor
	startedAt    time.Time
	logger       *slog.Logger
	client       *http.Client
	server       *http.Server
	listener     net.Listener
	shutdownOnce sync.Once
	shutdownCh   chan struct{}
	doneCh       chan struct{}

	mu                    sync.Mutex
	rojoCmd               *exec.Cmd
	rojoJob               *processJob
	rojoForwardCancel     context.CancelFunc
	rojoForwardGeneration int64
	rojoStatus            RojoStatus
	helperState           string
	helperLastHeartbeatAt *time.Time
	helperLastError       string
}

func New(config Config, logger *slog.Logger) (*Agent, error) {
	if logger == nil {
		logger = slog.Default()
	}
	if config.Workspace == "" {
		workspace, err := os.Getwd()
		if err != nil {
			return nil, err
		}
		config.Workspace = workspace
	}
	workspace, err := filepath.Abs(config.Workspace)
	if err != nil {
		return nil, err
	}
	config.Workspace = workspace
	if config.MachineName == "" {
		return nil, errors.New("machine_name is required")
	}
	if config.PlaceID == "" {
		return nil, errors.New("place_id is required")
	}
	if !placeIDPattern.MatchString(config.PlaceID) {
		return nil, errors.New("place_id must contain digits only")
	}
	if config.HelperBaseURL == "" {
		return nil, errors.New("helper_base_url is required in local mode")
	}
	if config.Environment == "" {
		config.Environment = "local"
	}
	config.DomainSuffix = ResolveDomainSuffix(config.DomainSuffix)
	if config.RojoBin == "" {
		config.RojoBin = "rojo"
	}
	if config.ProjectPath == "" {
		config.ProjectPath = config.Workspace
	}
	if config.StatusAddr == "" {
		config.StatusAddr = "127.0.0.1:0"
	}
	client, err := newHelperHTTPClient(config.HelperBaseURL, config.Workspace)
	if err != nil {
		return nil, err
	}
	rojoJob, err := newProcessJob()
	if err != nil {
		return nil, err
	}
	return &Agent{
		config:      config,
		startedAt:   time.Now(),
		logger:      logger,
		client:      client,
		shutdownCh:  make(chan struct{}),
		doneCh:      make(chan struct{}),
		helperState: "not_registered",
		rojoJob:     rojoJob,
	}, nil
}

func (a *Agent) Run(ctx context.Context) error {
	defer close(a.doneCh)
	defer a.closeRojoJob()

	rojoListener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return err
	}
	rojoAddr := rojoListener.Addr().String()
	_ = rojoListener.Close()

	listener, err := net.Listen("tcp", a.config.StatusAddr)
	if err != nil {
		return err
	}
	a.listener = listener
	baseStatusURL := "http://" + listener.Addr().String()
	statusURL := baseStatusURL + "/status"
	taskID, err := NewTaskID()
	if err != nil {
		return err
	}
	a.descriptor = Descriptor{
		TaskID:               taskID,
		Environment:          a.config.Environment,
		MachineName:          a.config.MachineName,
		PlaceID:              a.config.PlaceID,
		TaskAgentPID:         os.Getpid(),
		TaskAgentStartedAtMS: UnixMillis(a.startedAt),
		TaskAgentStatusURL:   statusURL,
		Helper: HelperRoute{
			BaseURL:   a.config.HelperBaseURL,
			PublicURL: a.config.HelperPublicURL,
		},
		Rojo: RojoRoute{
			LocalURL:    "http://" + rojoAddr,
			UpstreamURL: "http://" + rojoAddr,
		},
	}
	if a.config.RegisterDomain {
		userName, err := ResolveUserName(a.config.UserName)
		if err != nil {
			_ = listener.Close()
			return err
		}
		publicURL := RojoPublicURL(a.config.PlaceID, taskID, userName, a.config.DomainSuffix)
		a.config.UserName = userName
		a.descriptor.Rojo.UpstreamURL = publicURL
	}
	a.rojoStatus = RojoStatus{
		State:       "starting",
		LocalURL:    a.descriptor.Rojo.LocalURL,
		UpstreamURL: a.descriptor.Rojo.UpstreamURL,
	}
	if a.config.RegisterDomain {
		a.rojoStatus.PublicURL = a.descriptor.Rojo.UpstreamURL
		a.rojoStatus.PublicState = "configured"
	}
	if err := SaveDescriptor(a.config.Workspace, a.descriptor); err != nil {
		_ = listener.Close()
		return err
	}

	mux := http.NewServeMux()
	mux.HandleFunc("GET /status", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, a.Status())
	})
	mux.HandleFunc("POST /shutdown", func(w http.ResponseWriter, r *http.Request) {
		a.Shutdown()
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":      true,
			"task_id": a.descriptor.TaskID,
		})
	})
	a.server = &http.Server{
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}
	go func() {
		if err := a.server.Serve(listener); err != nil && !errors.Is(err, http.ErrServerClosed) {
			a.logger.Error("task-agent status server stopped", "error", err)
			a.Shutdown()
		}
	}()
	go a.runRojoSupervisor()
	if a.config.RegisterDomain {
		if err := a.startRojoPublicForward(ctx); err != nil {
			_ = listener.Close()
			a.Shutdown()
			a.shutdownBounded()
			return err
		}
	}
	go a.runHeartbeatLoop()

	select {
	case <-ctx.Done():
		a.Shutdown()
	case <-a.shutdownCh:
	}
	a.shutdownBounded()
	return nil
}

func (a *Agent) Shutdown() {
	a.shutdownOnce.Do(func() {
		close(a.shutdownCh)
	})
}

func (a *Agent) Done() <-chan struct{} {
	return a.doneCh
}

func (a *Agent) Status() StatusResponse {
	a.mu.Lock()
	defer a.mu.Unlock()
	return StatusResponse{
		OK:                       true,
		TaskID:                   a.descriptor.TaskID,
		MachineName:              a.descriptor.MachineName,
		PlaceID:                  a.descriptor.PlaceID,
		Workspace:                a.config.Workspace,
		TaskAgentPID:             a.descriptor.TaskAgentPID,
		TaskAgentStartedAtMS:     a.descriptor.TaskAgentStartedAtMS,
		TaskAgentStatusURL:       a.descriptor.TaskAgentStatusURL,
		HelperBaseURL:            a.descriptor.Helper.BaseURL,
		HelperRegistrationState:  a.helperState,
		HelperLastHeartbeatAt:    a.helperLastHeartbeatAt,
		HelperLastHeartbeatError: a.helperLastError,
		Rojo:                     a.rojoStatus,
	}
}

func (a *Agent) runHeartbeatLoop() {
	a.sendHeartbeat()
	ticker := time.NewTicker(heartbeatInterval)
	defer ticker.Stop()
	for {
		select {
		case <-ticker.C:
			a.sendHeartbeat()
		case <-a.shutdownCh:
			return
		}
	}
}

func (a *Agent) sendHeartbeat() {
	a.mu.Lock()
	rojoRunning := a.rojoStatus.State == "running"
	rojoPublicReady := !a.config.RegisterDomain || a.rojoStatus.PublicState == "running"
	a.mu.Unlock()
	if !rojoRunning || !rojoPublicReady {
		a.mu.Lock()
		a.helperState = "waiting_for_rojo"
		a.mu.Unlock()
		return
	}
	body := map[string]any{
		"task_id":                  a.descriptor.TaskID,
		"machine_name":             a.descriptor.MachineName,
		"place_id":                 a.descriptor.PlaceID,
		"task_agent_pid":           a.descriptor.TaskAgentPID,
		"task_agent_started_at_ms": a.descriptor.TaskAgentStartedAtMS,
		"rojo_upstream_url":        a.descriptor.Rojo.UpstreamURL,
	}
	payload, _ := json.Marshal(body)
	url := fmt.Sprintf("%s/session/%s/heartbeat", a.descriptor.Helper.BaseURL, a.descriptor.TaskID)
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(payload))
	if err == nil {
		req.Header.Set("Content-Type", "application/json")
	}
	var status string
	var detail string
	if err != nil {
		status = "error"
		detail = err.Error()
	} else if resp, err := a.client.Do(req); err != nil {
		status = "retrying"
		detail = err.Error()
	} else {
		defer resp.Body.Close()
		if resp.StatusCode >= 200 && resp.StatusCode < 300 {
			status = "registered"
			detail = ""
		} else {
			status = "error"
			detail = "HTTP " + resp.Status
		}
	}
	now := time.Now()
	a.mu.Lock()
	a.helperState = status
	a.helperLastError = detail
	if status == "registered" {
		a.helperLastHeartbeatAt = &now
	}
	a.mu.Unlock()
}

func (a *Agent) runRojoSupervisor() {
	restarts := 0
	for {
		select {
		case <-a.shutdownCh:
			return
		default:
		}
		cmd := exec.Command(a.config.RojoBin, "serve", a.config.ProjectPath, "--port", listenPort(a.descriptor.Rojo.LocalURL))
		if err := startManagedCommand(cmd, a.rojoJob); err != nil {
			a.recordRojoExit("error", err, restarts)
			select {
			case <-time.After(time.Second):
				restarts++
				continue
			case <-a.shutdownCh:
				return
			}
		}
		a.mu.Lock()
		a.rojoCmd = cmd
		a.rojoStatus.State = "running"
		a.rojoStatus.PID = cmd.Process.Pid
		a.rojoStatus.Restarts = restarts
		a.rojoStatus.LastError = ""
		a.mu.Unlock()

		err := cmd.Wait()
		a.mu.Lock()
		if a.rojoCmd == cmd {
			a.rojoCmd = nil
		}
		a.mu.Unlock()
		select {
		case <-a.shutdownCh:
			return
		default:
		}
		restarts++
		a.recordRojoExit("restarting", err, restarts)
		select {
		case <-time.After(time.Second):
		case <-a.shutdownCh:
			return
		}
	}
}

func (a *Agent) closeRojoJob() {
	a.mu.Lock()
	job := a.rojoJob
	a.rojoJob = nil
	a.mu.Unlock()
	if job != nil {
		_ = job.close()
	}
}

func (a *Agent) recordRojoExit(state string, err error, restarts int) {
	now := time.Now()
	a.mu.Lock()
	defer a.mu.Unlock()
	a.rojoStatus.State = state
	a.rojoStatus.PID = 0
	a.rojoStatus.Restarts = restarts
	a.rojoStatus.LastExitAt = &now
	if err != nil {
		a.rojoStatus.LastError = err.Error()
	}
}

func (a *Agent) shutdownBounded() {
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	done := make(chan struct{})
	go func() {
		a.releaseHelper()
		time.Sleep(time.Second)
		a.stopRojoPublicForward()
		a.stopRojo()
		if a.server != nil {
			_ = a.server.Shutdown(context.Background())
		}
		close(done)
	}()

	select {
	case <-done:
	case <-ctx.Done():
		a.stopRojo()
	}
}

func (a *Agent) startRojoPublicForward(ctx context.Context) error {
	token, err := ResolveBearerToken(a.config.Workspace)
	if err != nil {
		a.mu.Lock()
		a.rojoStatus.PublicState = "error"
		a.rojoStatus.PublicError = err.Error()
		a.mu.Unlock()
		return err
	}
	runner := a.config.RojoForwardRunner
	if runner == nil {
		runner = runEmbeddedRojoForward
	}
	config := clockbridge.RemoteForwardConfig{
		TargetURL:           a.descriptor.Rojo.LocalURL,
		UUID:                strings.TrimSuffix(RojoPublicHost(a.config.PlaceID, a.descriptor.TaskID, a.config.UserName, a.config.DomainSuffix), "."+ResolveDomainSuffix(a.config.DomainSuffix)),
		RegisterHost:        ClockbridgeRegisterHost(a.config.DomainSuffix),
		ProxyMode:           clockbridge.ProxyModeLegacyFramed,
		RegisterBearerToken: token,
	}
	forwardCtx, cancel := context.WithCancel(ctx)
	a.mu.Lock()
	a.rojoForwardCancel = cancel
	a.rojoForwardGeneration++
	generation := a.rojoForwardGeneration
	a.rojoStatus.PublicState = "running"
	a.rojoStatus.PublicError = ""
	a.mu.Unlock()

	go func() {
		err := runner(forwardCtx, config)
		a.mu.Lock()
		defer a.mu.Unlock()
		if a.rojoForwardGeneration != generation {
			return
		}
		a.rojoForwardCancel = nil
		if err != nil && forwardCtx.Err() == nil {
			a.rojoStatus.PublicState = "exited"
			a.rojoStatus.PublicError = err.Error()
			a.logger.Warn("task-agent Rojo public exposure exited", "error", err)
			return
		}
		a.rojoStatus.PublicState = "stopped"
	}()
	return nil
}

func (a *Agent) stopRojoPublicForward() {
	a.mu.Lock()
	cancel := a.rojoForwardCancel
	a.rojoForwardCancel = nil
	if cancel != nil {
		a.rojoForwardGeneration++
		a.rojoStatus.PublicState = "stopping"
	}
	a.mu.Unlock()
	if cancel != nil {
		cancel()
	}
}

func runEmbeddedRojoForward(ctx context.Context, config clockbridge.RemoteForwardConfig) error {
	forward, err := clockbridge.NewRemoteForward(config)
	if err != nil {
		return err
	}
	return forward.Run(ctx)
}

func (a *Agent) releaseHelper() {
	if a.descriptor.TaskID == "" || a.descriptor.Helper.BaseURL == "" {
		return
	}
	url := fmt.Sprintf("%s/session/%s/release", a.descriptor.Helper.BaseURL, a.descriptor.TaskID)
	body := map[string]any{
		"task_agent_pid":           a.descriptor.TaskAgentPID,
		"task_agent_started_at_ms": a.descriptor.TaskAgentStartedAtMS,
	}
	payload, _ := json.Marshal(body)
	req, err := http.NewRequest(http.MethodPost, url, bytes.NewReader(payload))
	if err != nil {
		return
	}
	req.Header.Set("Content-Type", "application/json")
	resp, err := a.client.Do(req)
	if err != nil {
		return
	}
	_ = resp.Body.Close()
}

func (a *Agent) stopRojo() {
	a.mu.Lock()
	cmd := a.rojoCmd
	a.rojoCmd = nil
	a.rojoStatus.State = "stopped"
	a.mu.Unlock()
	if cmd == nil || cmd.Process == nil {
		return
	}
	stopManagedCommand(cmd)
}

func NewTaskID() (string, error) {
	var bytes [5]byte
	if _, err := rand.Read(bytes[:]); err != nil {
		return "", err
	}
	return "t" + hex.EncodeToString(bytes[:]), nil
}

func listenPort(url string) string {
	_, port, err := net.SplitHostPort(url[len("http://"):])
	if err != nil {
		return "0"
	}
	return port
}

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	_ = json.NewEncoder(w).Encode(value)
}
