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

const codeSyncProtocolVersion = 2
const codeSyncMappingProfile = "sync_lua_v1"

var allowedStudioServices = map[string]struct{}{
	"Lighting":            {},
	"ReplicatedFirst":     {},
	"ReplicatedStorage":   {},
	"ServerScriptService": {},
	"ServerStorage":       {},
	"SoundService":        {},
	"StarterGui":          {},
	"StarterPack":         {},
	"StarterPlayer":       {},
	"Workspace":           {},
}

type CodeSyncBinding struct {
	ProtocolVersion    int                 `json:"protocol_version"`
	WorkspaceID        string              `json:"workspace_id"`
	PlaceID            string              `json:"place_id"`
	MachineName        string              `json:"machine_name"`
	MappingProfile     string              `json:"mapping_profile"`
	CodeSyncConfigHash string              `json:"code_sync_config_hash"`
	RootsAuthorityHash string              `json:"roots_authority_hash"`
	ConfigPath         string              `json:"config_path"`
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
	Roots []codeSyncRootPayload `json:"roots"`
}

type codeSyncRootPayload struct {
	RootID     string   `json:"root_id"`
	LocalPath  string   `json:"local_path"`
	StudioPath []string `json:"studio_path"`
	Include    []string `json:"include"`
	Exclude    []string `json:"exclude"`
}

func BuildCodeSyncBinding(workspace string, configPath string, machineName string, placeID string) (CodeSyncBinding, error) {
	workspaceAbs, err := filepath.Abs(workspace)
	if err != nil {
		return CodeSyncBinding{}, err
	}
	configRel, configAbs := normalizeWorkspacePath(workspaceAbs, configPath, "code-sync.roots.json")
	roots, err := loadCodeSyncConfig(configAbs)
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
		MappingProfile:     codeSyncMappingProfile,
		CodeSyncConfigHash: configHash(codeSyncProtocolVersion, codeSyncMappingProfile, rootDicts),
		RootsAuthorityHash: rootsAuthorityHash(rootRoutes),
		ConfigPath:         configRel,
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

func normalizeWorkspacePath(workspace string, value string, defaultValue string) (string, string) {
	trimmed := strings.TrimSpace(value)
	if trimmed == "" {
		trimmed = defaultValue
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

func loadCodeSyncConfig(path string) ([]codeSyncRootConfig, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	var payload codeSyncConfigPayload
	if err := json.Unmarshal(body, &payload); err != nil {
		return nil, err
	}
	if len(payload.Roots) == 0 {
		return nil, fmt.Errorf("roots must be a non-empty array")
	}
	roots := make([]codeSyncRootConfig, 0, len(payload.Roots))
	for _, item := range payload.Roots {
		root, err := parseCodeSyncRoot(item)
		if err != nil {
			return nil, err
		}
		roots = append(roots, root)
	}
	if err := validateUniqueCodeSyncRoots(roots); err != nil {
		return nil, err
	}
	return roots, nil
}

func parseCodeSyncRoot(item codeSyncRootPayload) (codeSyncRootConfig, error) {
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
	if _, ok := allowedStudioServices[studioPath[0]]; !ok {
		return codeSyncRootConfig{}, fmt.Errorf("studio_path must start from a supported DataModel service for root_id %s", rootID)
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

func normalizePatterns(values []string, defaultValues []string) []string {
	if values == nil {
		values = defaultValues
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

func configHash(protocolVersion int, mappingProfile string, roots []map[string]any) string {
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
		canonicalString(mappingProfile),
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
