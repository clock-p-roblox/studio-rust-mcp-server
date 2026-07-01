package taskagent

import (
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
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

var allowedManagedServiceNodes = map[string]struct{}{
	"ReplicatedStorage":   {},
	"ServerScriptService": {},
}

var reservedServerScriptServiceChildren = map[string]struct{}{
	"MCP2Runtime":            {},
	"MCPStudioSessionControl": {},
}

var allowedCodeSyncNodeMetaKeys = map[string]struct{}{
	"$local_path": {},
	"$kind":       {},
	"$include":    {},
	"$exclude":    {},
}

type CodeSyncBinding struct {
	ProtocolVersion     int                   `json:"protocol_version"`
	WorkspaceID         string                `json:"workspace_id"`
	PlaceID             string                `json:"place_id"`
	MachineName         string                `json:"machine_name"`
	MappingProfile      string                `json:"mapping_profile"`
	CodeSyncConfigHash  string                `json:"code_sync_config_hash"`
	TargetAuthorityHash string                `json:"target_authority_hash"`
	ConfigPath          string                `json:"config_path"`
	Targets             []CodeSyncTargetRoute `json:"targets"`
}

type CodeSyncTargetRoute struct {
	StudioPath []string `json:"studio_path"`
}

type codeSyncNodeConfig struct {
	LocalPath  string
	StudioPath []string
	Kind       string
	Include    []string
	Exclude    []string
}

func BuildCodeSyncBinding(workspace string, configPath string, machineName string, placeID string) (CodeSyncBinding, error) {
	workspaceAbs, err := filepath.Abs(workspace)
	if err != nil {
		return CodeSyncBinding{}, err
	}
	configRel, configAbs := normalizeWorkspacePath(workspaceAbs, configPath, "code-sync.tree.json")
	nodes, err := loadCodeSyncConfig(configAbs)
	if err != nil {
		return CodeSyncBinding{}, err
	}
	targets := topLevelTargets(nodes)
	return CodeSyncBinding{
		ProtocolVersion:     codeSyncProtocolVersion,
		WorkspaceID:         workspaceID(workspaceAbs),
		PlaceID:             placeID,
		MachineName:         machineName,
		MappingProfile:      codeSyncMappingProfile,
		CodeSyncConfigHash:  configHash(codeSyncProtocolVersion, codeSyncMappingProfile, nodes),
		TargetAuthorityHash: targetAuthorityHash(targets),
		ConfigPath:          configRel,
		Targets:             targets,
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

func loadCodeSyncConfig(path string) ([]codeSyncNodeConfig, error) {
	body, err := os.ReadFile(path)
	if err != nil {
		return nil, err
	}
	if err := rejectDuplicateJSONKeys(body); err != nil {
		return nil, err
	}
	var payload map[string]any
	if err := json.Unmarshal(body, &payload); err != nil {
		return nil, err
	}
	if _, ok := payload["roots"]; ok {
		return nil, fmt.Errorf("old roots format is not supported; use tree")
	}
	rawTree, ok := payload["tree"].(map[string]any)
	if !ok || len(rawTree) == 0 {
		return nil, fmt.Errorf("tree must be a non-empty object")
	}
	nodes := []codeSyncNodeConfig{}
	for service, rawServiceTree := range rawTree {
		if _, ok := allowedStudioServices[service]; !ok {
			return nil, fmt.Errorf("tree key must be a supported DataModel service: %s", service)
		}
		serviceTree, ok := rawServiceTree.(map[string]any)
		if !ok {
			return nil, fmt.Errorf("Studio service entry must be an object: %s", service)
		}
		if err := parseCodeSyncTreeNode(serviceTree, []string{service}, true, &nodes); err != nil {
			return nil, err
		}
	}
	if len(nodes) == 0 {
		return nil, fmt.Errorf("tree must contain at least one node with $local_path")
	}
	if err := validateUniqueCodeSyncNodes(nodes); err != nil {
		return nil, err
	}
	return nodes, nil
}

func rejectDuplicateJSONKeys(body []byte) error {
	decoder := json.NewDecoder(bytes.NewReader(body))
	return rejectDuplicateJSONKeysValue(decoder)
}

func rejectDuplicateJSONKeysValue(decoder *json.Decoder) error {
	token, err := decoder.Token()
	if err != nil {
		if err == io.EOF {
			return nil
		}
		return err
	}
	if delimiter, ok := token.(json.Delim); ok {
		switch delimiter {
		case '{':
			seen := map[string]struct{}{}
			for decoder.More() {
				keyToken, err := decoder.Token()
				if err != nil {
					return err
				}
				key, ok := keyToken.(string)
				if !ok {
					return fmt.Errorf("object key must be a string")
				}
				if _, exists := seen[key]; exists {
					return fmt.Errorf("duplicate JSON key: %s", key)
				}
				seen[key] = struct{}{}
				if err := rejectDuplicateJSONKeysValue(decoder); err != nil {
					return err
				}
			}
			_, err := decoder.Token()
			return err
		case '[':
			for decoder.More() {
				if err := rejectDuplicateJSONKeysValue(decoder); err != nil {
					return err
				}
			}
			_, err := decoder.Token()
			return err
		}
	}
	return nil
}

func parseCodeSyncTreeNode(value map[string]any, studioPath []string, isService bool, nodes *[]codeSyncNodeConfig) error {
	metaKeys := []string{}
	for key := range value {
		if strings.HasPrefix(key, "$") {
			metaKeys = append(metaKeys, key)
		}
	}
	_, isNode := value["$local_path"]
	if isService && len(metaKeys) > 0 && !isNode {
		return fmt.Errorf("Studio service entries cannot declare metadata unless they are managed nodes: %v", studioPath)
	}
	if !isNode && len(metaKeys) > 0 {
		return fmt.Errorf("only nodes with $local_path can declare metadata: %v", studioPath)
	}
	for _, key := range metaKeys {
		if _, ok := allowedCodeSyncNodeMetaKeys[key]; !ok {
			return fmt.Errorf("unknown code-sync node metadata %q for %v", key, studioPath)
		}
	}
	if isService && isNode {
		if _, ok := allowedManagedServiceNodes[studioPath[0]]; !ok {
			return fmt.Errorf("this Studio service cannot be a managed code-sync node: %v", studioPath)
		}
	}
	if isService {
		if _, ok := value["$kind"]; ok {
			return fmt.Errorf("managed Studio service nodes cannot declare $kind: %v", studioPath)
		}
	}
	if hasReservedServerScriptServiceSegment(studioPath) {
		return fmt.Errorf("code-sync path uses a reserved ServerScriptService child: %v", studioPath)
	}
	if isNode {
		node, err := parseCodeSyncNode(value, studioPath)
		if err != nil {
			return err
		}
		*nodes = append(*nodes, node)
	}
	for childName, rawChild := range value {
		if strings.HasPrefix(childName, "$") {
			continue
		}
		childTree, ok := rawChild.(map[string]any)
		if !ok {
			return fmt.Errorf("Studio child entry must be an object: %v", appendPath(studioPath, childName))
		}
		if err := parseCodeSyncTreeNode(childTree, appendPath(studioPath, childName), false, nodes); err != nil {
			return err
		}
	}
	return nil
}

func hasReservedServerScriptServiceSegment(studioPath []string) bool {
	if len(studioPath) < 2 || studioPath[0] != "ServerScriptService" {
		return false
	}
	for _, segment := range studioPath[1:] {
		if _, ok := reservedServerScriptServiceChildren[segment]; ok {
			return true
		}
	}
	return false
}

func parseCodeSyncNode(value map[string]any, studioPath []string) (codeSyncNodeConfig, error) {
	localPath, ok := value["$local_path"].(string)
	localPath = strings.ReplaceAll(strings.TrimSpace(localPath), "\\", "/")
	if !ok || localPath == "" {
		return codeSyncNodeConfig{}, fmt.Errorf("$local_path must be a non-empty string for %v", studioPath)
	}
	if escapesWorkspace(localPath) {
		return codeSyncNodeConfig{}, fmt.Errorf("$local_path must be workspace-relative and cannot escape workspace for %v", studioPath)
	}
	kind := ""
	if rawKind, ok := value["$kind"]; ok {
		kind, ok = rawKind.(string)
		if !ok || !validCodeSyncKind(kind) {
			return codeSyncNodeConfig{}, fmt.Errorf("$kind must be Folder, ModuleScript, Script or LocalScript for %v", studioPath)
		}
	}
	include := normalizePatterns(anyStringSlice(value["$include"]), []string{"**/*.lua", "**/*.luau"})
	exclude := normalizePatterns(anyStringSlice(value["$exclude"]), nil)
	return codeSyncNodeConfig{LocalPath: localPath, StudioPath: append([]string(nil), studioPath...), Kind: kind, Include: include, Exclude: exclude}, nil
}

func validCodeSyncKind(kind string) bool {
	return kind == "Folder" || kind == "ModuleScript" || kind == "Script" || kind == "LocalScript"
}

func anyStringSlice(value any) []string {
	if value == nil {
		return nil
	}
	items, ok := value.([]any)
	if !ok {
		return []string{"\x00-invalid"}
	}
	result := make([]string, 0, len(items))
	for _, item := range items {
		text, ok := item.(string)
		if !ok {
			return []string{"\x00-invalid"}
		}
		result = append(result, text)
	}
	return result
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
		if value == "\x00-invalid" {
			return []string{value}
		}
		result = append(result, strings.ReplaceAll(value, "\\", "/"))
	}
	return result
}

func validateUniqueCodeSyncNodes(nodes []codeSyncNodeConfig) error {
	seenPaths := make(map[string]struct{})
	for _, node := range nodes {
		key := canonicalStudioPath(node.StudioPath)
		if _, ok := seenPaths[key]; ok {
			return fmt.Errorf("duplicate studio_path: %v", node.StudioPath)
		}
		seenPaths[key] = struct{}{}
		for _, pattern := range append(append([]string{}, node.Include...), node.Exclude...) {
			if pattern == "\x00-invalid" {
				return fmt.Errorf("$include and $exclude must be string arrays for %v", node.StudioPath)
			}
		}
	}
	return nil
}

func topLevelTargets(nodes []codeSyncNodeConfig) []CodeSyncTargetRoute {
	result := []CodeSyncTargetRoute{}
	for _, node := range nodes {
		if nearestParentNode(node.StudioPath, nodes) != nil {
			continue
		}
		result = append(result, CodeSyncTargetRoute{StudioPath: append([]string(nil), node.StudioPath...)})
	}
	sort.Slice(result, func(i, j int) bool {
		return canonicalStudioPath(result[i].StudioPath) < canonicalStudioPath(result[j].StudioPath)
	})
	return result
}

func nearestParentNode(path []string, nodes []codeSyncNodeConfig) []string {
	var best []string
	for _, node := range nodes {
		candidate := node.StudioPath
		if len(candidate) >= len(path) || !pathPrefix(candidate, path) {
			continue
		}
		if best == nil || len(candidate) > len(best) {
			best = candidate
		}
	}
	return best
}

func appendPath(path []string, segment string) []string {
	next := append([]string(nil), path...)
	return append(next, segment)
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

func configHash(protocolVersion int, mappingProfile string, nodes []codeSyncNodeConfig) string {
	nodesCopy := append([]codeSyncNodeConfig(nil), nodes...)
	sort.Slice(nodesCopy, func(i, j int) bool {
		return canonicalStudioPath(nodesCopy[i].StudioPath) < canonicalStudioPath(nodesCopy[j].StudioPath)
	})
	var nodeBytes []byte
	for _, node := range nodesCopy {
		include := append([]string(nil), node.Include...)
		exclude := append([]string(nil), node.Exclude...)
		sort.Strings(include)
		sort.Strings(exclude)
		nodeBytes = append(nodeBytes, canonicalString(canonicalStudioPath(node.StudioPath))...)
		nodeBytes = append(nodeBytes, canonicalString(node.Kind)...)
		nodeBytes = append(nodeBytes, canonicalString(strings.ReplaceAll(node.LocalPath, "\\", "/"))...)
		nodeBytes = append(nodeBytes, canonicalString(len(node.StudioPath))...)
		for _, segment := range node.StudioPath {
			nodeBytes = append(nodeBytes, canonicalString(segment)...)
		}
		nodeBytes = append(nodeBytes, canonicalString(len(include))...)
		for _, pattern := range include {
			nodeBytes = append(nodeBytes, canonicalString(pattern)...)
		}
		nodeBytes = append(nodeBytes, canonicalString(len(exclude))...)
		for _, pattern := range exclude {
			nodeBytes = append(nodeBytes, canonicalString(pattern)...)
		}
	}
	return blake3Hex(joinBytes(
		canonicalString("clockp.code_sync.v2.config"),
		canonicalString(protocolVersion),
		canonicalString(mappingProfile),
		canonicalString(len(nodesCopy)),
		nodeBytes,
	))
}

func targetAuthorityHash(targets []CodeSyncTargetRoute) string {
	targetIDs := make([]string, 0, len(targets))
	for _, target := range targets {
		targetIDs = append(targetIDs, canonicalStudioPath(target.StudioPath))
	}
	sort.Strings(targetIDs)
	var targetBytes []byte
	for _, targetID := range targetIDs {
		targetBytes = append(targetBytes, canonicalString(targetID)...)
	}
	return blake3Hex(joinBytes(
		canonicalString("clockp.code_sync.v2.target_authority"),
		canonicalString(len(targetIDs)),
		targetBytes,
	))
}

func canonicalStudioPath(studioPath []string) string {
	var builder strings.Builder
	builder.WriteString("studio-path-v1:")
	for _, segment := range studioPath {
		builder.WriteString(fmt.Sprintf("%d:%s", len([]byte(segment)), segment))
	}
	return builder.String()
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
