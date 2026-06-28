package main

import (
	"errors"
	"net"
	"net/http"
	"os"
	"strings"
)

func publicBearerAuthMiddleware(
	next http.Handler,
	statusFn func() publicExposureStatus,
	tokenFileFn func() string,
) http.Handler {
	return http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		status := statusFn()
		if !requestTargetsPublicHost(r, status.PublicHost) {
			next.ServeHTTP(w, r)
			return
		}
		expectedToken, err := readPublicBearerToken(tokenFileFn())
		if err != nil {
			writeJSON(w, http.StatusServiceUnavailable, map[string]any{
				"ok":      false,
				"code":    "public_auth_unavailable",
				"message": "public helper2 bearer token is unavailable",
			})
			return
		}
		if bearerTokenFromRequest(r) != expectedToken {
			writeJSON(w, http.StatusUnauthorized, map[string]any{
				"ok":      false,
				"code":    "unauthorized",
				"message": "public helper2 bearer token is required",
			})
			return
		}
		next.ServeHTTP(w, r)
	})
}

func requestTargetsPublicHost(r *http.Request, publicHost string) bool {
	publicHost = strings.ToLower(strings.TrimSpace(publicHost))
	if publicHost == "" {
		return false
	}
	for _, candidate := range []string{r.Host, r.Header.Get("X-Forwarded-Host")} {
		if strings.ToLower(hostWithoutPort(candidate)) == publicHost {
			return true
		}
	}
	return false
}

func hostWithoutPort(raw string) string {
	host := strings.TrimSpace(raw)
	if host == "" {
		return ""
	}
	if splitHost, _, err := net.SplitHostPort(host); err == nil {
		return splitHost
	}
	return host
}

func readPublicBearerToken(tokenFile string) (string, error) {
	body, err := os.ReadFile(strings.TrimSpace(tokenFile))
	if err != nil {
		return "", err
	}
	token := strings.TrimSpace(string(body))
	if token == "" {
		return "", errors.New("public bearer token file is empty")
	}
	return token, nil
}

func bearerTokenFromRequest(r *http.Request) string {
	header := strings.TrimSpace(r.Header.Get("Authorization"))
	if len(header) < len("Bearer ") || !strings.EqualFold(header[:len("Bearer ")], "Bearer ") {
		return ""
	}
	return strings.TrimSpace(header[len("Bearer "):])
}
