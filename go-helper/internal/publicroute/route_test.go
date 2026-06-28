package publicroute

import "testing"

func TestHelperPublicRouteMatchesClockbridgeIdentity(t *testing.T) {
	baseURL, err := HelperBaseURL("win-a", "tester", "")
	if err != nil {
		t.Fatalf("helper base URL failed: %v", err)
	}
	if baseURL != "https://roblox-helper-win-a-tester-user.dev.clock-p.com" {
		t.Fatalf("unexpected helper base URL: %s", baseURL)
	}
	identity, err := HelperBridgeIdentity("win-a", "tester", "")
	if err != nil {
		t.Fatalf("helper identity failed: %v", err)
	}
	if identity != "roblox-helper-win-a-tester-user.dev.clock-p.com@register-https-proxy.dev.clock-p.com" {
		t.Fatalf("unexpected helper identity: %s", identity)
	}
}

func TestRojoPublicRouteUsesTaskID(t *testing.T) {
	baseURL, err := RojoBaseURL("123456", "tdemo", "tester", "")
	if err != nil {
		t.Fatalf("Rojo base URL failed: %v", err)
	}
	if baseURL != "https://123456-tdemo-rojo-tester-user.dev.clock-p.com" {
		t.Fatalf("unexpected Rojo base URL: %s", baseURL)
	}
	identity, err := RojoBridgeIdentity("123456", "tdemo", "tester", "")
	if err != nil {
		t.Fatalf("Rojo identity failed: %v", err)
	}
	if identity != "123456-tdemo-rojo-tester-user.dev.clock-p.com@register-https-proxy.dev.clock-p.com" {
		t.Fatalf("unexpected Rojo identity: %s", identity)
	}
}
