package taskagent

import (
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"strings"
)

const workspaceConfigFileName = "clock-p.workspace.json"

type WorkspaceConfig struct {
	PlaceID        string `json:"place_id"`
	CodeSyncConfig string `json:"code_sync_config,omitempty"`
}

func LoadWorkspaceConfig(workspace string) (WorkspaceConfig, error) {
	workspaceAbs, err := filepath.Abs(workspace)
	if err != nil {
		return WorkspaceConfig{}, err
	}
	path := filepath.Join(workspaceAbs, workspaceConfigFileName)
	body, err := os.ReadFile(path)
	if err != nil {
		return WorkspaceConfig{}, fmt.Errorf("read %s: %w", path, err)
	}
	var config WorkspaceConfig
	if err := json.Unmarshal(body, &config); err != nil {
		return WorkspaceConfig{}, fmt.Errorf("parse %s: %w", path, err)
	}
	config.PlaceID = strings.TrimSpace(config.PlaceID)
	if config.PlaceID == "" {
		return WorkspaceConfig{}, fmt.Errorf("%s place_id is required", path)
	}
	if !regexp.MustCompile(`^[0-9]+$`).MatchString(config.PlaceID) {
		return WorkspaceConfig{}, fmt.Errorf("%s place_id must contain digits only", path)
	}
	config.CodeSyncConfig = normalizeWorkspaceConfigPath(config.CodeSyncConfig, "code-sync.tree.json")
	return config, nil
}

func ValidateWorkspaceBindingFiles(workspace string, config WorkspaceConfig) error {
	workspaceAbs, err := filepath.Abs(workspace)
	if err != nil {
		return err
	}
	configPath := filepath.Join(workspaceAbs, filepath.FromSlash(config.CodeSyncConfig))
	if _, err := os.Stat(configPath); err != nil {
		if errors.Is(err, os.ErrNotExist) {
			return fmt.Errorf(
				"missing %s; clock-p.workspace.json code_sync_config points here. This file declares the Studio node tree managed by code-sync",
				configPath,
			)
		}
		return fmt.Errorf("stat %s: %w", configPath, err)
	}
	return nil
}

func normalizeWorkspaceConfigPath(value string, defaultValue string) string {
	trimmed := strings.TrimSpace(value)
	if trimmed == "" {
		return defaultValue
	}
	return filepath.ToSlash(filepath.Clean(trimmed))
}
