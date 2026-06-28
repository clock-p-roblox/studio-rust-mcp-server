package publicroute

import (
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
)

const DefaultDomainSuffix = "dev.clock-p.com"

func HelperBaseURL(machineName string, userName string, domainSuffix string) (string, error) {
	host, err := HelperHost(machineName, userName, domainSuffix)
	if err != nil {
		return "", err
	}
	return "https://" + host, nil
}

func HelperBridgeIdentity(machineName string, userName string, domainSuffix string) (string, error) {
	host, err := HelperHost(machineName, userName, domainSuffix)
	if err != nil {
		return "", err
	}
	return host + "@register-https-proxy." + normalizeDomainSuffix(domainSuffix), nil
}

func HelperHost(machineName string, userName string, domainSuffix string) (string, error) {
	machineName = strings.TrimSpace(machineName)
	userName = strings.TrimSpace(userName)
	if machineName == "" {
		return "", errors.New("machine_name is required for public helper URL")
	}
	if userName == "" {
		return "", errors.New("user_name is required for public helper URL")
	}
	return fmt.Sprintf("roblox-helper-%s-%s-user.%s", machineName, userName, normalizeDomainSuffix(domainSuffix)), nil
}

func RojoBaseURL(placeID string, taskID string, userName string, domainSuffix string) (string, error) {
	host, err := RojoHost(placeID, taskID, userName, domainSuffix)
	if err != nil {
		return "", err
	}
	return "https://" + host, nil
}

func RojoBridgeIdentity(placeID string, taskID string, userName string, domainSuffix string) (string, error) {
	host, err := RojoHost(placeID, taskID, userName, domainSuffix)
	if err != nil {
		return "", err
	}
	return host + "@register-https-proxy." + normalizeDomainSuffix(domainSuffix), nil
}

func RojoHost(placeID string, taskID string, userName string, domainSuffix string) (string, error) {
	placeID = strings.TrimSpace(placeID)
	taskID = strings.TrimSpace(taskID)
	userName = strings.TrimSpace(userName)
	if placeID == "" {
		return "", errors.New("place_id is required for public Rojo URL")
	}
	if taskID == "" {
		return "", errors.New("task_id is required for public Rojo URL")
	}
	if userName == "" {
		return "", errors.New("user_name is required for public Rojo URL")
	}
	return fmt.Sprintf("%s-%s-rojo-%s-user.%s", placeID, taskID, userName, normalizeDomainSuffix(domainSuffix)), nil
}

func ResolveUserName(explicit string) (string, error) {
	if value := strings.TrimSpace(explicit); value != "" {
		return value, nil
	}
	for _, candidate := range devClockPCandidates("feishu-user_name") {
		body, err := os.ReadFile(candidate)
		if err == nil {
			if value := strings.TrimSpace(string(body)); value != "" {
				return value, nil
			}
		}
	}
	return "", errors.New("--user is required for public mode when feishu-user_name cannot be resolved")
}

func ResolveTokenFile(explicit string) (string, error) {
	if value := strings.TrimSpace(explicit); value != "" {
		return value, nil
	}
	for _, candidate := range devClockPCandidates("feishu-token") {
		if stat, err := os.Stat(candidate); err == nil && !stat.IsDir() {
			return candidate, nil
		}
	}
	return "", errors.New("--clockbridge-token-file is required for public mode when feishu-token cannot be resolved")
}

func normalizeDomainSuffix(domainSuffix string) string {
	if value := strings.Trim(strings.TrimSpace(domainSuffix), "."); value != "" {
		return value
	}
	return DefaultDomainSuffix
}

func devClockPCandidates(fileName string) []string {
	var candidates []string
	if appData := os.Getenv("APPDATA"); appData != "" {
		candidates = append(candidates, filepath.Join(appData, "dev.clock-p.com", fileName))
	}
	if userProfile := os.Getenv("USERPROFILE"); userProfile != "" {
		candidates = append(candidates, filepath.Join(userProfile, ".dev.clock-p.com", fileName))
	}
	if home := os.Getenv("HOME"); home != "" {
		candidates = append(candidates, filepath.Join(home, ".dev.clock-p.com", fileName))
	}
	return candidates
}
