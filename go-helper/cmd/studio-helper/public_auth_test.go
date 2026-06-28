package main

import (
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"testing"
)

func TestPublicBearerAuthMiddlewareOnlyProtectsPublicHost(t *testing.T) {
	tokenFile := filepath.Join(t.TempDir(), "feishu-token")
	if err := os.WriteFile(tokenFile, []byte("secret-token\n"), 0o600); err != nil {
		t.Fatalf("write token: %v", err)
	}
	handler := publicBearerAuthMiddleware(
		http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.WriteHeader(http.StatusNoContent)
		}),
		func() publicExposureStatus {
			return publicExposureStatus{
				Enabled:    true,
				State:      "running",
				PublicHost: "roblox-helper-win-a-sunjun-user.dev.clock-p.com",
			}
		},
		func() string { return tokenFile },
	)

	localRequest := httptest.NewRequest(http.MethodGet, "http://127.0.0.1:44750/status", nil)
	localResponse := httptest.NewRecorder()
	handler.ServeHTTP(localResponse, localRequest)
	if localResponse.Code != http.StatusNoContent {
		t.Fatalf("local request should not require bearer token, got %d", localResponse.Code)
	}

	publicMissing := httptest.NewRequest(http.MethodGet, "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com/status", nil)
	missingResponse := httptest.NewRecorder()
	handler.ServeHTTP(missingResponse, publicMissing)
	if missingResponse.Code != http.StatusUnauthorized {
		t.Fatalf("public request without token = %d", missingResponse.Code)
	}

	publicInvalid := httptest.NewRequest(http.MethodGet, "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com/status", nil)
	publicInvalid.Header.Set("Authorization", "Bearer wrong-token")
	invalidResponse := httptest.NewRecorder()
	handler.ServeHTTP(invalidResponse, publicInvalid)
	if invalidResponse.Code != http.StatusUnauthorized {
		t.Fatalf("public request with invalid token = %d", invalidResponse.Code)
	}

	publicValid := httptest.NewRequest(http.MethodGet, "https://roblox-helper-win-a-sunjun-user.dev.clock-p.com/status", nil)
	publicValid.Header.Set("Authorization", "Bearer secret-token")
	validResponse := httptest.NewRecorder()
	handler.ServeHTTP(validResponse, publicValid)
	if validResponse.Code != http.StatusNoContent {
		t.Fatalf("public request with valid token = %d", validResponse.Code)
	}
}

func TestPublicBearerAuthMiddlewareHonorsForwardedHost(t *testing.T) {
	tokenFile := filepath.Join(t.TempDir(), "feishu-token")
	if err := os.WriteFile(tokenFile, []byte("secret-token\n"), 0o600); err != nil {
		t.Fatalf("write token: %v", err)
	}
	handler := publicBearerAuthMiddleware(
		http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
			w.WriteHeader(http.StatusNoContent)
		}),
		func() publicExposureStatus {
			return publicExposureStatus{
				Enabled:    true,
				State:      "running",
				PublicHost: "roblox-helper-win-a-sunjun-user.dev.clock-p.com",
			}
		},
		func() string { return tokenFile },
	)
	request := httptest.NewRequest(http.MethodGet, "http://127.0.0.1:44750/status", nil)
	request.Header.Set("X-Forwarded-Host", "roblox-helper-win-a-sunjun-user.dev.clock-p.com")
	response := httptest.NewRecorder()
	handler.ServeHTTP(response, request)
	if response.Code != http.StatusUnauthorized {
		t.Fatalf("forwarded public host without token = %d", response.Code)
	}
}
