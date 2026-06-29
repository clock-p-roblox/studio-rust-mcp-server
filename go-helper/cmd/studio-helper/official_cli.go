package main

import (
	"bufio"
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
)

type officialToolRunner interface {
	Run(ctx context.Context, managedStudio studio.ManagedProcess, toolName string, arguments map[string]any) (officialToolResponse, error)
}

type officialCLIProcessRunner struct{}

type officialStudioRef struct {
	ID     string `json:"id"`
	Name   string `json:"name"`
	Active bool   `json:"active"`
}

type officialToolResponse struct {
	Tool          string            `json:"tool"`
	Studio        officialStudioRef `json:"studio"`
	VerifiedPlace string            `json:"verified_place_id,omitempty"`
	IsError       bool              `json:"is_error"`
	Content       []officialContent `json:"content,omitempty"`
	ParsedContent any               `json:"parsed_content,omitempty"`
	RawResult     map[string]any    `json:"raw_result,omitempty"`
	Stderr        string            `json:"stderr,omitempty"`
}

type officialContent struct {
	Type string `json:"type"`
	Text string `json:"text,omitempty"`
}

type officialJSONRPCResponse struct {
	JSONRPC string            `json:"jsonrpc"`
	ID      int               `json:"id,omitempty"`
	Result  json.RawMessage   `json:"result,omitempty"`
	Error   *officialRPCError `json:"error,omitempty"`
}

type officialRPCError struct {
	Code    int             `json:"code"`
	Message string          `json:"message"`
	Data    json.RawMessage `json:"data,omitempty"`
}

type officialToolMCPResult struct {
	Content []officialContent `json:"content"`
	IsError bool              `json:"isError"`
}

type officialStudioListPayload struct {
	Note    string              `json:"note,omitempty"`
	Studios []officialStudioRef `json:"studios"`
}

type officialProcess struct {
	cmd    *exec.Cmd
	stdin  io.WriteCloser
	reader *bufio.Reader
	stderr *bytes.Buffer
}

func (r officialCLIProcessRunner) Run(ctx context.Context, managedStudio studio.ManagedProcess, toolName string, arguments map[string]any) (officialToolResponse, error) {
	studioMCPPath, err := officialStudioMCPPath(managedStudio.StudioPath)
	if err != nil {
		return officialToolResponse{}, err
	}
	process, err := startOfficialProcess(ctx, studioMCPPath)
	if err != nil {
		return officialToolResponse{}, err
	}
	defer process.closeAndWait()

	if err := process.call(1, "initialize", officialInitializeParams(), nil); err != nil {
		return officialToolResponse{}, err
	}
	if err := process.notify("notifications/initialized", nil); err != nil {
		return officialToolResponse{}, err
	}
	studioRef, verifiedPlaceID, nextID, err := process.waitForVerifiedStudioPlace(ctx, 2, managedStudio.PlaceID, 20*time.Second)
	if err != nil {
		return officialToolResponse{}, err
	}
	stderr := strings.TrimSpace(process.stderr.String())
	if toolName == "" {
		return officialToolResponse{
			Tool:          "official_ping",
			Studio:        studioRef,
			VerifiedPlace: verifiedPlaceID,
			IsError:       false,
			ParsedContent: map[string]any{"bound": true},
			Stderr:        stderr,
		}, nil
	}

	result, err := process.callTool(nextID, toolName, arguments)
	if err != nil {
		return officialToolResponse{}, err
	}
	response := officialToolResponse{
		Tool:          toolName,
		Studio:        studioRef,
		VerifiedPlace: verifiedPlaceID,
		IsError:       result.IsError,
		Content:       result.Content,
		ParsedContent: parseOfficialContent(result.Content),
		RawResult: map[string]any{
			"isError": result.IsError,
			"content": result.Content,
		},
		Stderr: stderr,
	}
	if response.IsError {
		if toolName == officialToolWaitJobFinished && officialWaitJobTimedOut(response.ParsedContent) {
			return response, nil
		}
		return response, officialToolBusinessError{response: response}
	}
	return response, nil
}

func officialInitializeParams() map[string]any {
	return map[string]any{
		"protocolVersion": "2025-11-25",
		"capabilities":    map[string]any{},
		"clientInfo":      map[string]any{"name": "studio-helper2", "version": "official-local"},
	}
}

func officialInitializeRequest(id int) map[string]any {
	return map[string]any{
		"jsonrpc": "2.0",
		"id":      id,
		"method":  "initialize",
		"params":  officialInitializeParams(),
	}
}

func officialInitializedNotification() map[string]any {
	return map[string]any{
		"jsonrpc": "2.0",
		"method":  "notifications/initialized",
	}
}

func officialToolCallRequest(id int, name string, arguments map[string]any) map[string]any {
	return map[string]any{
		"jsonrpc": "2.0",
		"id":      id,
		"method":  "tools/call",
		"params": map[string]any{
			"name":      name,
			"arguments": arguments,
		},
	}
}

func runOfficialBatch(ctx context.Context, studioMCPPath string, requests []map[string]any) ([]officialJSONRPCResponse, string, error) {
	var input bytes.Buffer
	for _, request := range requests {
		body, err := json.Marshal(request)
		if err != nil {
			return nil, "", err
		}
		input.Write(body)
		input.WriteByte('\n')
	}
	command := fmt.Sprintf("$input | & '%s' --stdio", strings.ReplaceAll(studioMCPPath, "'", "''"))
	cmd := exec.CommandContext(ctx, "powershell", "-NoProfile", "-Command", command)
	cmd.Stdin = &input
	var stdout bytes.Buffer
	var stderr bytes.Buffer
	cmd.Stdout = &stdout
	cmd.Stderr = &stderr
	if err := cmd.Run(); err != nil {
		return nil, strings.TrimSpace(stderr.String()), fmt.Errorf("official CLI PowerShell bridge failed: %w; stderr=%s", err, strings.TrimSpace(stderr.String()))
	}
	responses := make([]officialJSONRPCResponse, 0)
	scanner := bufio.NewScanner(&stdout)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		var response officialJSONRPCResponse
		if err := json.Unmarshal([]byte(line), &response); err != nil {
			return nil, strings.TrimSpace(stderr.String()), fmt.Errorf("failed to parse official CLI response: %w; line=%s", err, line)
		}
		responses = append(responses, response)
	}
	if err := scanner.Err(); err != nil {
		return nil, strings.TrimSpace(stderr.String()), fmt.Errorf("failed to read official CLI response: %w", err)
	}
	return responses, strings.TrimSpace(stderr.String()), nil
}

func officialToolResultByID(responses []officialJSONRPCResponse, id int) (officialToolMCPResult, error) {
	for _, response := range responses {
		if response.ID != id {
			continue
		}
		if response.Error != nil {
			return officialToolMCPResult{}, fmt.Errorf("official CLI JSON-RPC error %d: %s", response.Error.Code, response.Error.Message)
		}
		var result officialToolMCPResult
		if err := json.Unmarshal(response.Result, &result); err != nil {
			return officialToolMCPResult{}, fmt.Errorf("failed to decode official CLI tool response id=%d: %w", id, err)
		}
		return result, nil
	}
	return officialToolMCPResult{}, fmt.Errorf("official CLI response id=%d was not returned", id)
}

func verifyOfficialPlaceResult(result officialToolMCPResult, expectedPlaceID string) (string, error) {
	if result.IsError {
		return "", fmt.Errorf("official CLI could not verify active Studio place_id: %s", firstOfficialContentText(result.Content))
	}
	actualPlaceID := strings.TrimSpace(firstOfficialContentText(result.Content))
	if actualPlaceID == "" {
		return "", errors.New("official CLI returned empty active Studio place_id")
	}
	if actualPlaceID != expectedPlaceID {
		return "", fmt.Errorf("official CLI active Studio place_id mismatch: got %s, want %s", actualPlaceID, expectedPlaceID)
	}
	return actualPlaceID, nil
}

type officialToolBusinessError struct {
	response officialToolResponse
}

func (e officialToolBusinessError) Error() string {
	text := firstOfficialContentText(e.response.Content)
	if text == "" {
		return "official CLI tool returned isError=true"
	}
	return text
}

func officialStudioMCPPath(studioPath string) (string, error) {
	if strings.TrimSpace(studioPath) == "" {
		return "", errors.New("managed Studio has no executable path")
	}
	path := filepath.Join(filepath.Dir(studioPath), "StudioMCP.exe")
	info, err := os.Stat(path)
	if err != nil {
		return "", fmt.Errorf("could not locate StudioMCP.exe next to Studio: %s: %w", path, err)
	}
	if info.IsDir() {
		return "", fmt.Errorf("StudioMCP path is a directory: %s", path)
	}
	return path, nil
}

func startOfficialProcess(ctx context.Context, studioMCPPath string) (*officialProcess, error) {
	cmd := exec.CommandContext(ctx, studioMCPPath, "--stdio")
	if cwd, err := os.Getwd(); err == nil && filepath.Base(cwd) == "go-helper" {
		cmd.Dir = filepath.Dir(cwd)
	}
	stdin, err := cmd.StdinPipe()
	if err != nil {
		return nil, err
	}
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		_ = stdin.Close()
		return nil, err
	}
	stderr := &bytes.Buffer{}
	cmd.Stderr = stderr
	if err := cmd.Start(); err != nil {
		_ = stdin.Close()
		return nil, err
	}
	return &officialProcess{
		cmd:    cmd,
		stdin:  stdin,
		reader: bufio.NewReader(stdout),
		stderr: stderr,
	}, nil
}

func (p *officialProcess) notify(method string, params any) error {
	payload := map[string]any{
		"jsonrpc": "2.0",
		"method":  method,
	}
	if params != nil {
		payload["params"] = params
	}
	return p.write(payload)
}

func (p *officialProcess) call(id int, method string, params any, out any) error {
	payload := map[string]any{
		"jsonrpc": "2.0",
		"id":      id,
		"method":  method,
	}
	if params != nil {
		payload["params"] = params
	}
	if err := p.write(payload); err != nil {
		return err
	}
	response, err := p.readResponse(id)
	if err != nil {
		return err
	}
	if response.Error != nil {
		return fmt.Errorf("official CLI JSON-RPC error %d: %s", response.Error.Code, response.Error.Message)
	}
	if out != nil && len(response.Result) > 0 {
		if err := json.Unmarshal(response.Result, out); err != nil {
			return fmt.Errorf("failed to decode official CLI response for %s: %w", method, err)
		}
	}
	return nil
}

func (p *officialProcess) callTool(id int, name string, arguments map[string]any) (officialToolMCPResult, error) {
	var result officialToolMCPResult
	err := p.call(id, "tools/call", map[string]any{
		"name":      name,
		"arguments": arguments,
	}, &result)
	return result, err
}

func (p *officialProcess) waitForStudioList(ctx context.Context, firstID int, timeout time.Duration) (officialToolMCPResult, int, error) {
	deadline := time.Now().Add(timeout)
	id := firstID
	var lastResult officialToolMCPResult
	for {
		result, err := p.callTool(id, "list_roblox_studios", map[string]any{})
		id++
		if err != nil {
			return officialToolMCPResult{}, id, err
		}
		if !officialStudioListPending(result) {
			return result, id, nil
		}
		lastResult = result
		if !time.Now().Before(deadline) {
			return officialToolMCPResult{}, id, fmt.Errorf("official CLI list_roblox_studios returned an error: %s", firstOfficialContentText(lastResult.Content))
		}
		select {
		case <-ctx.Done():
			return officialToolMCPResult{}, id, ctx.Err()
		case <-time.After(time.Second):
		}
	}
}

func (p *officialProcess) waitForVerifiedStudioPlace(ctx context.Context, firstID int, expectedPlaceID string, timeout time.Duration) (officialStudioRef, string, int, error) {
	deadline := time.Now().Add(timeout)
	id := firstID
	var lastErr error
	for {
		listResult, nextID, err := p.waitForStudioList(ctx, id, 5*time.Second)
		id = nextID
		if err != nil {
			lastErr = err
		} else {
			studioRef, err := chooseSingleOfficialStudio(listResult)
			if err != nil {
				lastErr = err
			} else if _, err := p.callTool(id, "set_active_studio", map[string]any{"studio_id": studioRef.ID}); err != nil {
				id++
				lastErr = err
			} else {
				id++
				verifiedPlaceID, err := p.verifyActiveStudioPlace(id, expectedPlaceID)
				id++
				if err == nil {
					return studioRef, verifiedPlaceID, id, nil
				}
				lastErr = err
			}
		}
		if !officialVerifyPlacePending(lastErr) || !time.Now().Before(deadline) {
			return officialStudioRef{}, "", id, lastErr
		}
		select {
		case <-ctx.Done():
			return officialStudioRef{}, "", id, ctx.Err()
		case <-time.After(time.Second):
		}
	}
}

func (p *officialProcess) verifyActiveStudioPlace(id int, expectedPlaceID string) (string, error) {
	result, err := p.callTool(id, "execute_luau", map[string]any{
		"datamodel_type": "Edit",
		"code":           "return tostring(game.PlaceId)",
	})
	if err != nil {
		return "", err
	}
	if result.IsError {
		return "", fmt.Errorf("official CLI could not verify active Studio place_id: %s", firstOfficialContentText(result.Content))
	}
	actualPlaceID := strings.TrimSpace(firstOfficialContentText(result.Content))
	if actualPlaceID == "" {
		return "", errors.New("official CLI returned empty active Studio place_id")
	}
	if actualPlaceID != expectedPlaceID {
		return "", fmt.Errorf("official CLI active Studio place_id mismatch: got %s, want %s", actualPlaceID, expectedPlaceID)
	}
	return actualPlaceID, nil
}

func (p *officialProcess) write(payload map[string]any) error {
	body, err := json.Marshal(payload)
	if err != nil {
		return err
	}
	body = append(body, '\n')
	_, err = p.stdin.Write(body)
	return err
}

func (p *officialProcess) readResponse(id int) (officialJSONRPCResponse, error) {
	for {
		line, err := p.reader.ReadBytes('\n')
		if err != nil {
			return officialJSONRPCResponse{}, fmt.Errorf("failed to read official CLI response id=%d: %w; stderr=%s", id, err, strings.TrimSpace(p.stderr.String()))
		}
		line = bytes.TrimSpace(line)
		if len(line) == 0 {
			continue
		}
		var response officialJSONRPCResponse
		if err := json.Unmarshal(line, &response); err != nil {
			return officialJSONRPCResponse{}, fmt.Errorf("failed to parse official CLI response: %w; line=%s", err, string(line))
		}
		if response.ID == id {
			return response, nil
		}
	}
}

func (p *officialProcess) closeAndWait() {
	_ = p.stdin.Close()
	_ = p.cmd.Wait()
}

func chooseSingleOfficialStudio(result officialToolMCPResult) (officialStudioRef, error) {
	if result.IsError {
		return officialStudioRef{}, fmt.Errorf("official CLI list_roblox_studios returned an error: %s", firstOfficialContentText(result.Content))
	}
	var payload officialStudioListPayload
	if err := decodeFirstOfficialJSONContent(result.Content, &payload); err != nil {
		return officialStudioRef{}, err
	}
	switch len(payload.Studios) {
	case 0:
		return officialStudioRef{}, errors.New("official CLI did not find any connected Roblox Studio")
	case 1:
		return payload.Studios[0], nil
	default:
		return officialStudioRef{}, fmt.Errorf("official CLI saw multiple Roblox Studio instances; local official commands require exactly one before public/multi-open support: %d", len(payload.Studios))
	}
}

func officialStudioListNotConnected(result officialToolMCPResult) bool {
	if !result.IsError {
		return false
	}
	return strings.Contains(firstOfficialContentText(result.Content), "Not connected to the WS host")
}

func officialStudioListPending(result officialToolMCPResult) bool {
	if officialStudioListNotConnected(result) {
		return true
	}
	if result.IsError {
		return false
	}
	var payload officialStudioListPayload
	if err := decodeFirstOfficialJSONContent(result.Content, &payload); err != nil {
		return false
	}
	return len(payload.Studios) == 0
}

func officialVerifyPlacePending(err error) bool {
	if err == nil {
		return false
	}
	message := err.Error()
	return strings.Contains(message, "previously active Studio has disconnected") ||
		strings.Contains(message, "doesn't have a place opened") ||
		strings.Contains(message, "did not find any connected Roblox Studio") ||
		strings.Contains(message, "Not connected to the WS host") ||
		strings.Contains(message, `"studios":[]`)
}

func decodeFirstOfficialJSONContent(content []officialContent, out any) error {
	text := firstOfficialContentText(content)
	if text == "" {
		return errors.New("official CLI response did not contain text JSON")
	}
	if err := json.Unmarshal([]byte(text), out); err != nil {
		return fmt.Errorf("failed to decode official CLI text JSON: %w", err)
	}
	return nil
}

func parseOfficialContent(content []officialContent) any {
	text := firstOfficialContentText(content)
	if text == "" {
		return nil
	}
	var parsed any
	if err := json.Unmarshal([]byte(text), &parsed); err == nil {
		return parsed
	}
	return text
}

func firstOfficialContentText(content []officialContent) string {
	for _, item := range content {
		if item.Type == "text" && strings.TrimSpace(item.Text) != "" {
			return item.Text
		}
	}
	return ""
}

func officialWaitJobTimedOut(parsed any) bool {
	payload, ok := parsed.(map[string]any)
	if !ok {
		return false
	}
	status, _ := payload["status"].(string)
	lastKnown, _ := payload["lastKnownStatus"].(string)
	return status == "Timeout" && lastKnown != ""
}
