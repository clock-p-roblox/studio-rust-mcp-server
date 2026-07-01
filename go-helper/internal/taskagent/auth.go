package taskagent

import (
	"errors"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
)

type bearerTransport struct {
	token string
	base  http.RoundTripper
}

func (transport bearerTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	base := transport.base
	if base == nil {
		base = http.DefaultTransport
	}
	next := req.Clone(req.Context())
	next.Header.Set("Authorization", "Bearer "+transport.token)
	return base.RoundTrip(next)
}

func newHelperHTTPClient(helperURL string, workspace string) (*http.Client, error) {
	client := &http.Client{Timeout: heartbeatHTTPTimeout}
	if helperURLRequiresBearer(helperURL) {
		token, err := ResolveClockbridgeRegisterToken(workspace)
		if err != nil {
			return nil, err
		}
		client.Transport = bearerTransport{token: token}
	}
	return client, nil
}

func helperURLRequiresBearer(helperURL string) bool {
	parsed, err := url.Parse(strings.TrimSpace(helperURL))
	if err != nil {
		return false
	}
	host := strings.ToLower(parsed.Hostname())
	return parsed.Scheme == "https" && strings.HasPrefix(host, "roblox-helper-") && strings.HasSuffix(host, ".dev.clock-p.com")
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
