package taskagent

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"time"
)

var ErrDescriptorIdentityMismatch = errors.New("descriptor identity mismatch")

type Descriptor struct {
	TaskID               string      `json:"task_id"`
	Environment          string      `json:"environment"`
	MachineName          string      `json:"machine_name"`
	PlaceID              string      `json:"place_id"`
	TaskAgentPID         int         `json:"task_agent_pid"`
	TaskAgentStartedAtMS int64       `json:"task_agent_started_at_ms"`
	TaskAgentStatusURL   string      `json:"task_agent_status_url"`
	Helper               HelperRoute `json:"helper"`
	Rojo                 RojoRoute   `json:"rojo"`
}

type HelperRoute struct {
	BaseURL   string `json:"base_url"`
	PublicURL string `json:"public_url,omitempty"`
}

type RojoRoute struct {
	LocalURL    string `json:"local_url"`
	UpstreamURL string `json:"upstream_url"`
}

func SessionPath(workspace string) string {
	return filepath.Join(workspace, ".clock-p", "session.json")
}

func LoadDescriptor(workspace string) (Descriptor, error) {
	body, err := os.ReadFile(SessionPath(workspace))
	if err != nil {
		return Descriptor{}, err
	}
	var descriptor Descriptor
	if err := json.Unmarshal(body, &descriptor); err != nil {
		return Descriptor{}, err
	}
	return descriptor, nil
}

func SaveDescriptor(workspace string, descriptor Descriptor) error {
	path := SessionPath(workspace)
	if err := os.MkdirAll(filepath.Dir(path), 0o755); err != nil {
		return err
	}
	body, err := json.MarshalIndent(descriptor, "", "  ")
	if err != nil {
		return err
	}
	body = append(body, '\n')
	return os.WriteFile(path, body, 0o644)
}

func RemoveVolatileDescriptor(workspace string) error {
	path := SessionPath(workspace)
	if err := os.Remove(path); err != nil && !errors.Is(err, os.ErrNotExist) {
		return err
	}
	return nil
}

func RequestExistingShutdown(client *http.Client, descriptor Descriptor, timeout time.Duration) (bool, error) {
	if descriptor.TaskID == "" || descriptor.TaskAgentStatusURL == "" {
		return false, nil
	}
	status, err := FetchStatus(client, descriptor.TaskAgentStatusURL, timeout)
	if err != nil {
		return false, err
	}
	if status.TaskID != descriptor.TaskID {
		return false, fmt.Errorf("%w: recorded status URL belongs to task %s, expected %s", ErrDescriptorIdentityMismatch, status.TaskID, descriptor.TaskID)
	}
	if status.TaskAgentPID != descriptor.TaskAgentPID {
		return false, fmt.Errorf("%w: recorded status URL reports pid %d, expected %d", ErrDescriptorIdentityMismatch, status.TaskAgentPID, descriptor.TaskAgentPID)
	}
	if status.TaskAgentStartedAtMS != descriptor.TaskAgentStartedAtMS {
		return false, fmt.Errorf("%w: recorded status URL reports started_at_ms %d, expected %d", ErrDescriptorIdentityMismatch, status.TaskAgentStartedAtMS, descriptor.TaskAgentStartedAtMS)
	}
	shutdownURL, err := taskAgentShutdownURL(descriptor.TaskAgentStatusURL)
	if err != nil {
		return false, err
	}
	req, err := http.NewRequest(http.MethodPost, shutdownURL, nil)
	if err != nil {
		return false, err
	}
	resp, err := client.Do(req)
	if err != nil {
		return false, err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return false, fmt.Errorf("old task-agent shutdown failed: HTTP %s", resp.Status)
	}
	return true, nil
}

func FetchStatus(client *http.Client, baseURL string, timeout time.Duration) (StatusResponse, error) {
	if client == nil {
		client = http.DefaultClient
	}
	req, err := http.NewRequest(http.MethodGet, baseURL, nil)
	if err != nil {
		return StatusResponse{}, err
	}
	if timeout > 0 {
		ctx, cancel := context.WithTimeout(req.Context(), timeout)
		defer cancel()
		req = req.WithContext(ctx)
	}
	resp, err := client.Do(req)
	if err != nil {
		return StatusResponse{}, err
	}
	defer resp.Body.Close()
	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return StatusResponse{}, fmt.Errorf("task-agent status failed: HTTP %s", resp.Status)
	}
	var status StatusResponse
	if err := json.NewDecoder(resp.Body).Decode(&status); err != nil {
		return StatusResponse{}, err
	}
	return status, nil
}

func taskAgentShutdownURL(statusURL string) (string, error) {
	parsed, err := url.Parse(statusURL)
	if err != nil {
		return "", err
	}
	if parsed.Path != "/status" {
		return "", fmt.Errorf("task_agent_status_url must end with /status, got %q", statusURL)
	}
	parsed.Path = "/shutdown"
	parsed.RawQuery = ""
	parsed.Fragment = ""
	return parsed.String(), nil
}

func UnixMillis(t time.Time) int64 {
	return t.UnixNano() / int64(time.Millisecond)
}

type RouteConfig struct {
	Environment   string
	MachineName   string
	UserName      string
	HelperBaseURL string
}

func ResolveHelperBaseURL(config RouteConfig) (string, string, error) {
	environment := strings.TrimSpace(config.Environment)
	if environment == "" {
		environment = "local"
	}
	switch environment {
	case "local":
		if strings.TrimSpace(config.HelperBaseURL) == "" {
			return "", "", errors.New("--helper-base-url is required for local task-agent start")
		}
		return strings.TrimRight(strings.TrimSpace(config.HelperBaseURL), "/"), "", nil
	case "public":
		userName, err := ResolveUserName(config.UserName)
		if err != nil {
			return "", "", err
		}
		machineName := strings.TrimSpace(config.MachineName)
		if machineName == "" {
			return "", "", errors.New("machine_name is required for public helper URL")
		}
		baseURL := fmt.Sprintf("https://roblox-helper-%s-%s-user.dev.clock-p.com", machineName, userName)
		return baseURL, baseURL, nil
	default:
		return "", "", fmt.Errorf("--environment must be local or public, got %q", environment)
	}
}

func ResolveUserName(explicit string) (string, error) {
	if value := strings.TrimSpace(explicit); value != "" {
		return value, nil
	}
	for _, candidate := range userNameCandidates() {
		body, err := os.ReadFile(candidate)
		if err == nil {
			if value := strings.TrimSpace(string(body)); value != "" {
				return value, nil
			}
		}
	}
	return "", errors.New("--user is required for public task-agent start when feishu-user_name cannot be resolved")
}

func userNameCandidates() []string {
	var candidates []string
	if appData := os.Getenv("APPDATA"); appData != "" {
		candidates = append(candidates, filepath.Join(appData, "dev.clock-p.com", "feishu-user_name"))
	}
	if userProfile := os.Getenv("USERPROFILE"); userProfile != "" {
		candidates = append(candidates, filepath.Join(userProfile, ".dev.clock-p.com", "feishu-user_name"))
	}
	if home := os.Getenv("HOME"); home != "" {
		candidates = append(candidates, filepath.Join(home, ".dev.clock-p.com", "feishu-user_name"))
	}
	return candidates
}
