package taskagent

import (
	"errors"
	"net/http"
	"net/url"
	"os"
	"path/filepath"
	"strings"
)

const helperBearerTokenEnv = "ROBLOX_HELPER2_BEARER_TOKEN"

type bearerTransport struct {
	base  http.RoundTripper
	token string
}

func (t bearerTransport) RoundTrip(req *http.Request) (*http.Response, error) {
	cloned := req.Clone(req.Context())
	cloned.Header.Set("Authorization", "Bearer "+t.token)
	return t.base.RoundTrip(cloned)
}

func newHelperHTTPClient(helperBaseURL string, workspace string) (*http.Client, error) {
	client := &http.Client{Timeout: heartbeatHTTPTimeout}
	if !isPublicHelperBaseURL(helperBaseURL) {
		return client, nil
	}
	token, err := ResolveBearerToken(workspace)
	if err != nil {
		return nil, err
	}
	client.Transport = bearerTransport{base: http.DefaultTransport, token: token}
	return client, nil
}

func isPublicHelperBaseURL(baseURL string) bool {
	parsed, err := url.Parse(strings.TrimSpace(baseURL))
	if err != nil || parsed.Scheme != "https" {
		return false
	}
	host := strings.ToLower(parsed.Hostname())
	return host != "" && strings.HasSuffix(host, ".dev.clock-p.com")
}

func ResolveBearerToken(workspace string) (string, error) {
	if value := strings.TrimSpace(os.Getenv(helperBearerTokenEnv)); value != "" {
		return value, nil
	}
	for _, candidate := range tokenCandidates(workspace) {
		body, err := os.ReadFile(candidate)
		if err == nil {
			if value := strings.TrimSpace(string(body)); value != "" {
				return value, nil
			}
		}
	}
	return "", errors.New("ROBLOX_HELPER2_BEARER_TOKEN or feishu-token is required for public helper2 requests")
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
