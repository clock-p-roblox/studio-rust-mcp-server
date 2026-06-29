package taskagent

import (
	"errors"
	"net/http"
	"os"
	"path/filepath"
	"strings"
)

func newHelperHTTPClient(_ string, _ string) (*http.Client, error) {
	return &http.Client{Timeout: heartbeatHTTPTimeout}, nil
}

func ResolveClockbridgeRegisterToken(workspace string) (string, error) {
	for _, candidate := range tokenCandidates(workspace) {
		body, err := os.ReadFile(candidate)
		if err == nil {
			if value := strings.TrimSpace(string(body)); value != "" {
				return value, nil
			}
		}
	}
	return "", errors.New("feishu-token is required for clockbridge registration")
}

func tokenCandidates(workspace string) []string {
	var candidates []string
	if workspace != "" {
		candidates = append(candidates, filepath.Join(workspace, ".dev.clock-p.com", "feishu-token"))
	}
	if appData := os.Getenv("APPDATA"); appData != "" {
		candidates = append(candidates, filepath.Join(appData, "dev.clock-p.com", "feishu-token"))
	}
	if userProfile := os.Getenv("USERPROFILE"); userProfile != "" {
		candidates = append(candidates, filepath.Join(userProfile, ".dev.clock-p.com", "feishu-token"))
	}
	if home := os.Getenv("HOME"); home != "" {
		candidates = append(candidates, filepath.Join(home, ".dev.clock-p.com", "feishu-token"))
	}
	return candidates
}
