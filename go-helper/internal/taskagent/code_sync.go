package taskagent

import (
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strings"

	"github.com/zeebo/blake3"
)

const codeSyncProtocolVersion = 1
const codeSyncMappingProfile = "sync_lua_v1"

type CodeSyncBinding struct {
	ProtocolVersion    int                 `json:"protocol_version"`
	WorkspaceID        string              `json:"workspace_id"`
	PlaceID            string              `json:"place_id"`
	MachineName        string              `json:"machine_name"`
	ProjectID          string              `json:"project_id"`
	MappingProfile     string              `json:"mapping_profile"`
	CodeSyncConfigHash string              `json:"code_sync_config_hash"`
	RootsAuthorityHash string              `json:"roots_authority_hash"`
	ConfigPath         string              `json:"config_path"`
	ProjectPath        string              `json:"project_path"`
	Roots              []CodeSyncRootRoute `json:"roots"`
}

type CodeSyncRootRoute struct {
	RootID     string   `json:"root_id"`
	StudioPath []string `json:"studio_path"`
}

type codeSyncRootConfig struct {
	RootID     string
	LocalPath  string
	StudioPath []string
	Include    []string
	Exclude    []string
}

type codeSyncConfigPayload struct {
	ProjectID      string                `json:"project_id"`
	MappingProfile string                `json:"mapping_profile"`
	Roots          []codeSyncRootPayload `json:"roots"`
}

type codeSyncRootPayload struct {
	RootID     string   `json:"root_id"`
	LocalPath  string   `json:"local_path"`
	StudioPath []string `json:"studio_path"`
	Include    []string `json:"include"`
	Exclude    []string `json:"exclude"`
}

func BuildCodeSyncBinding(workspace string, configPath string, projectPath string, machineName string, placeID string) (CodeSyncBinding, error) {
	workspaceAbs, err := filepath.Abs(workspace)
	if err != nil {
		return CodeSyncBinding{}, err
	}
	configRel, configAbs := normalizeWorkspacePath(workspaceAbs, configPath, "code-sync.roots.json")
	projectRel, projectAbs := normalizeWorkspacePath(workspaceAbs, projectPath, "default.project.json")
	targets, err := loadProjectTargets(projectAbs)
	if err != nil {
		return CodeSyncBinding{}, err
	}
	projectID, mappingProfile, roots, err := loadCodeSyncConfig(configAbs, targets)
	if err != nil {
		return CodeSyncBinding{}, err
	}
	rootRoutes := make([]CodeSyncRootRoute, 0, len(roots))
	rootDicts := make([]map[string]any, 0, len(roots))
	for _, root := range roots {
		rootRoutes = append(rootRoutes, CodeSyncRootRoute{
			RootID:     root.RootID,
			StudioPath: append([]string(nil), root.StudioPath...),
		})
		rootDicts = append(rootDicts, map[string]any{
			"root_id":     root.RootID,
			"local_path":  root.LocalPath,
			"studio_path": root.StudioPath,
			"include":     root.Include,
			"exclude":     root.Exclude,
		})
	}
	return CodeSyncBinding{
		ProtocolVersion:    codeSyncProtocolVersion,
		WorkspaceID:        workspaceID(workspaceAbs),
		PlaceID:            placeID,
		MachineName:        machineName,
		ProjectID:          projectID,
		MappingProfile:     mappingProfile,
		CodeSyncConfigHash: configHash(codeSyncProtocolVersion, projectID, mappingProfile, targets, rootDicts),
		RootsAuthorityHash: rootsAuthorityHash(rootRoutes),
		ConfigPath:         configRel,
		ProjectPath:        projectRel,
		Roots:              rootRoutes,
	}, nil
}

func NewTaskSessionToken() (string, error) {
	var raw [32]byte
	if _, err := rand.Read(raw[:]); err != nil {
		return "", err
	}
	return hex.EncodeToString(raw[:]), nil
}

func normalizeWorkspacePath(workspace string, value string, fallback string) (string, string) {
	trimmed := strings.TrimSpace(value)
	if trimmed == "" {
		trimmed = fallback
	}
	cleaned := filepath.Clean(trimmed)
	if filepath.IsAbs(cleaned) {
		return filepath.ToSlash(cleaned), cleaned
	}
	return filepath.ToSlash(cleaned), filepath.Join(workspace, cleaned)
}

func workspaceID(workspace string) string {
	normalized := strings.ToLower(filepath.ToSlash(workspace))
	sum := blake3.Sum256([]byte(normalized))
	return hex.EncodeToString(sum[:])[:24]
}

func loadCodeSyncConfig(path string, targets [][]string) (string, string, []codeSyncRootConfig, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return "", "", nil, err
	}
	var payload codeSyncConfigPayload
	if err := json.Unmarshal(body, &payload); err != nil {
		return "", "", nil, err
	}
	projectID := strings.TrimSpace(payload.ProjectID)
	if projectID == "" {
		return "", "", nil, fmt.Errorf("project_id must be a non-empty string")
	}
	mappingProfile := strings.TrimSpace(payload.MappingProfile)
	if mappingProfile == "" {
		return "", "", nil, fmt.Errorf("mapping_profile must be a non-empty string")
	}
	if mappingProfile != codeSyncMappingProfile {
		return "", "", nil, fmt.Errorf("only mapping_profile=%s is supported", codeSyncMappingProfile)
	}
	if len(payload.Roots) == 0 {
		return "", "", nil, fmt.Errorf("roots must be a non-empty array")
	}
	roots := make([]codeSyncRootConfig, 0, len(payload.Roots))
	for _, item := range payload.Roots {
		root, err := parseCodeSyncRoot(item, targets)
		if err != nil {
			return "", "", nil, err
		}
		roots = append(roots, root)
	}
	if err := validateUniqueCodeSyncRoots(roots); err != nil {
		return "", "", nil, err
	}
	return projectID, mappingProfile, roots, nil
}

func parseCodeSyncRoot(item codeSyncRootPayload, targets [][]string) (codeSyncRootConfig, error) {
	rootID := strings.TrimSpace(item.RootID)
	if rootID == "" {
		return codeSyncRootConfig{}, fmt.Errorf("root_id must be a non-empty string")
	}
	localPath := strings.ReplaceAll(strings.TrimSpace(item.LocalPath), "\\", "/")
	if localPath == "" {
		return codeSyncRootConfig{}, fmt.Errorf("local_path must be a non-empty string for root_id %s", rootID)
	}
	if escapesWorkspace(localPath) {
		return codeSyncRootConfig{}, fmt.Errorf("local_path must be workspace-relative and cannot escape workspace for root_id %s", rootID)
	}
	if len(item.StudioPath) == 0 {
		return codeSyncRootConfig{}, fmt.Errorf("studio_path must be a non-empty string array for root_id %s", rootID)
	}
	studioPath := append([]string(nil), item.StudioPath...)
	for _, segment := range studioPath {
		if strings.TrimSpace(segment) == "" {
			return codeSyncRootConfig{}, fmt.Errorf("studio_path must not contain empty segments for root_id %s", rootID)
		}
	}
	if !studioPathAllowed(studioPath, targets) {
		return codeSyncRootConfig{}, fmt.Errorf("studio_path is not declared by the project target allowlist for root_id %s", rootID)
	}
	include := normalizePatterns(item.Include, []string{"**/*.lua", "**/*.luau"})
	exclude := normalizePatterns(item.Exclude, nil)
	return codeSyncRootConfig{RootID: rootID, LocalPath: localPath, StudioPath: studioPath, Include: include, Exclude: exclude}, nil
}

func escapesWorkspace(path string) bool {
	if strings.HasPrefix(path, "/") || path == ".." || strings.HasPrefix(path, "../") || strings.Contains("/"+path+"/", "/../") {
		return true
	}
	return regexp.MustCompile(`^[A-Za-z]:(/|$)`).MatchString(path)
}

func normalizePatterns(values []string, fallback []string) []string {
	if values == nil {
		values = fallback
	}
	result := make([]string, 0, len(values))
	for _, value := range values {
		result = append(result, strings.ReplaceAll(value, "\\", "/"))
	}
	return result
}

func validateUniqueCodeSyncRoots(roots []codeSyncRootConfig) error {
	seenIDs := make(map[string]struct{})
	var seenPaths [][]string
	for _, root := range roots {
		if _, ok := seenIDs[root.RootID]; ok {
			return fmt.Errorf("duplicate root_id %s", root.RootID)
		}
		seenIDs[root.RootID] = struct{}{}
		for _, existing := range seenPaths {
			if samePath(existing, root.StudioPath) || pathPrefix(existing, root.StudioPath) || pathPrefix(root.StudioPath, existing) {
				return fmt.Errorf("managed root studio_path cannot overlap: %v", root.StudioPath)
			}
		}
		seenPaths = append(seenPaths, root.StudioPath)
	}
	return nil
}

func loadProjectTargets(path string) ([][]string, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var payload struct {
		Tree map[string]any `json:"tree"`
	}
	if err := json.Unmarshal(body, &payload); err != nil {
		return nil, err
	}
	if payload.Tree == nil {
		return nil, fmt.Errorf("project missing tree object")
	}
	var targets [][]string
	for name, child := range payload.Tree {
		if strings.HasPrefix(name, "$") {
			continue
		}
		if childMap, ok := child.(map[string]any); ok {
			collectProjectTargets(childMap, []string{name}, &targets, true)
		}
	}
	return dedupePaths(targets), nil
}

func collectProjectTargets(node map[string]any, path []string, targets *[][]string, isService bool) {
	_, hasPath := node["$path"]
	var children []struct {
		name string
		node map[string]any
	}
	for name, child := range node {
		if strings.HasPrefix(name, "$") {
			continue
		}
		if childMap, ok := child.(map[string]any); ok {
			children = append(children, struct {
				name string
				node map[string]any
			}{name: name, node: childMap})
		}
	}
	if hasPath || len(children) == 0 || !isService {
		*targets = append(*targets, append([]string(nil), path...))
	}
	for _, child := range children {
		collectProjectTargets(child.node, append(append([]string(nil), path...), child.name), targets, false)
	}
}

func dedupePaths(paths [][]string) [][]string {
	seen := make(map[string]struct{})
	var result [][]string
	for _, path := range paths {
		key := strings.Join(path, "\x00")
		if _, ok := seen[key]; ok {
			continue
		}
		seen[key] = struct{}{}
		result = append(result, append([]string(nil), path...))
	}
	sort.Slice(result, func(i, j int) bool { return comparePath(result[i], result[j]) < 0 })
	return result
}

func studioPathAllowed(path []string, targets [][]string) bool {
	for _, target := range targets {
		if pathPrefix(target, path) {
			return true
		}
	}
	return false
}

func samePath(left []string, right []string) bool {
	return len(left) == len(right) && pathPrefix(left, right)
}

func pathPrefix(prefix []string, path []string) bool {
	if len(prefix) > len(path) {
		return false
	}
	for i := range prefix {
		if prefix[i] != path[i] {
			return false
		}
	}
	return true
}

func comparePath(left []string, right []string) int {
	for i := 0; i < len(left) && i < len(right); i++ {
		if left[i] < right[i] {
			return -1
		}
		if left[i] > right[i] {
			return 1
		}
	}
	if len(left) < len(right) {
		return -1
	}
	if len(left) > len(right) {
		return 1
	}
	return 0
}

func configHash(protocolVersion int, projectID string, mappingProfile string, targets [][]string, roots []map[string]any) string {
	targetsCopy := append([][]string(nil), targets...)
	sort.Slice(targetsCopy, func(i, j int) bool { return comparePath(targetsCopy[i], targetsCopy[j]) < 0 })
	var targetBytes []byte
	for _, target := range targetsCopy {
		targetBytes = append(targetBytes, canonicalString(len(target))...)
		for _, segment := range target {
			targetBytes = append(targetBytes, canonicalString(segment)...)
		}
	}
	rootsCopy := append([]map[string]any(nil), roots...)
	sort.Slice(rootsCopy, func(i, j int) bool { return fmt.Sprint(rootsCopy[i]["root_id"]) < fmt.Sprint(rootsCopy[j]["root_id"]) })
	var rootBytes []byte
	for _, root := range rootsCopy {
		studioPath := stringSlice(root["studio_path"])
		include := stringSlice(root["include"])
		exclude := stringSlice(root["exclude"])
		sort.Strings(include)
		sort.Strings(exclude)
		rootBytes = append(rootBytes, canonicalString(fmt.Sprint(root["root_id"]))...)
		rootBytes = append(rootBytes, canonicalString(strings.ReplaceAll(fmt.Sprint(root["local_path"]), "\\", "/"))...)
		rootBytes = append(rootBytes, canonicalString(len(studioPath))...)
		for _, segment := range studioPath {
			rootBytes = append(rootBytes, canonicalString(segment)...)
		}
		rootBytes = append(rootBytes, canonicalString(len(include))...)
		for _, pattern := range include {
			rootBytes = append(rootBytes, canonicalString(pattern)...)
		}
		rootBytes = append(rootBytes, canonicalString(len(exclude))...)
		for _, pattern := range exclude {
			rootBytes = append(rootBytes, canonicalString(pattern)...)
		}
	}
	return blake3Hex(joinBytes(
		canonicalString("clockp.code_sync.v1.config"),
		canonicalString(protocolVersion),
		canonicalString(projectID),
		canonicalString(mappingProfile),
		canonicalString(len(targetsCopy)),
		targetBytes,
		canonicalString(len(rootsCopy)),
		rootBytes,
	))
}

func rootsAuthorityHash(roots []CodeSyncRootRoute) string {
	rootsCopy := append([]CodeSyncRootRoute(nil), roots...)
	sort.Slice(rootsCopy, func(i, j int) bool { return rootsCopy[i].RootID < rootsCopy[j].RootID })
	var rootBytes []byte
	for _, root := range rootsCopy {
		rootBytes = append(rootBytes, canonicalString(root.RootID)...)
		rootBytes = append(rootBytes, canonicalString(len(root.StudioPath))...)
		for _, segment := range root.StudioPath {
			rootBytes = append(rootBytes, canonicalString(segment)...)
		}
	}
	return blake3Hex(joinBytes(
		canonicalString("clockp.code_sync.v1.roots_authority"),
		canonicalString(len(rootsCopy)),
		rootBytes,
	))
}

func canonicalString(value any) []byte {
	text := fmt.Sprint(value)
	return []byte(fmt.Sprintf("%d:%s", len([]byte(text)), text))
}

func joinBytes(parts ...[]byte) []byte {
	var total int
	for _, part := range parts {
		total += len(part)
	}
	result := make([]byte, 0, total)
	for _, part := range parts {
		result = append(result, part...)
	}
	return result
}

func blake3Hex(data []byte) string {
	sum := blake3.Sum256(data)
	return hex.EncodeToString(sum[:])
}

func stringSlice(value any) []string {
	switch typed := value.(type) {
	case []string:
		return append([]string(nil), typed...)
	case []any:
		result := make([]string, 0, len(typed))
		for _, item := range typed {
			result = append(result, fmt.Sprint(item))
		}
		return result
	default:
		return nil
	}
}
