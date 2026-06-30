package main

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"strings"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/tasksession"
)

const (
	officialToolGenerateMesh            = "generate_mesh"
	officialToolGenerateProceduralModel = "generate_procedural_model"
	officialToolStoreImage              = "store_image"
	officialToolWaitJobFinished         = "wait_job_finished"
	officialToolSearchAsset             = "search_asset"
	officialToolInsertAsset             = "insert_asset"
)

type officialPingRequest struct{}

type officialStoreImageRequest struct {
	FilePath string `json:"file_path"`
}

type officialVector3Request struct {
	X float64 `json:"x"`
	Y float64 `json:"y"`
	Z float64 `json:"z"`
}

type officialGenerateMeshRequest struct {
	TextPrompt   string                  `json:"text_prompt"`
	Size         *officialVector3Request `json:"size,omitempty"`
	MaxTriangles *int                    `json:"max_triangles,omitempty"`
	PartNames    string                  `json:"part_names,omitempty"`
}

type officialGenerateProceduralModelRequest struct {
	Prompt           string `json:"prompt"`
	AttachedImageURI string `json:"attached_image_uri,omitempty"`
	PartNames        string `json:"part_names,omitempty"`
}

type officialWaitJobRequest struct {
	GenerationID string   `json:"generation_id"`
	Timeout      *float64 `json:"timeout,omitempty"`
}

type officialSearchCreatorStoreRequest struct {
	Query                string   `json:"query,omitempty"`
	AssetType            string   `json:"asset_type,omitempty"`
	MaxResults           *int     `json:"max_results,omitempty"`
	PriceFilter          string   `json:"price_filter,omitempty"`
	MinPriceCents        *float64 `json:"min_price_cents,omitempty"`
	MaxPriceCents        *float64 `json:"max_price_cents,omitempty"`
	VerifiedCreatorsOnly *bool    `json:"verified_creators_only,omitempty"`
}

type officialInsertFromCreatorStoreRequest struct {
	AssetID    string `json:"asset_id"`
	AssetName  string `json:"asset_name,omitempty"`
	AssetType  string `json:"asset_type,omitempty"`
	ParentPath string `json:"parent_path,omitempty"`
}

func registerOfficialHTTPHandlers(
	mux *http.ServeMux,
	taskSessions *tasksession.Registry,
	studioManager *studio.Manager,
	runner officialToolRunner,
) {
	mux.HandleFunc("POST /session/{task_id}/official/ping", func(w http.ResponseWriter, r *http.Request) {
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, "", map[string]any{})
	})
	mux.HandleFunc("POST /session/{task_id}/official/store-image", func(w http.ResponseWriter, r *http.Request) {
		var request officialStoreImageRequest
		if !decodeOfficialHTTPRequest(w, r, &request) {
			return
		}
		args, err := officialStoreImageArgs(request)
		if err != nil {
			writeTaskAPIError(w, http.StatusBadRequest, r.PathValue("task_id"), "bad_request", err.Error(), "fix_request", nil)
			return
		}
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, officialToolStoreImage, args)
	})
	mux.HandleFunc("POST /session/{task_id}/official/generate-mesh", func(w http.ResponseWriter, r *http.Request) {
		var request officialGenerateMeshRequest
		if !decodeOfficialHTTPRequest(w, r, &request) {
			return
		}
		args, err := officialGenerateMeshArgs(request)
		if err != nil {
			writeTaskAPIError(w, http.StatusBadRequest, r.PathValue("task_id"), "bad_request", err.Error(), "fix_request", nil)
			return
		}
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, officialToolGenerateMesh, args)
	})
	mux.HandleFunc("POST /session/{task_id}/official/generate-procedural-model", func(w http.ResponseWriter, r *http.Request) {
		var request officialGenerateProceduralModelRequest
		if !decodeOfficialHTTPRequest(w, r, &request) {
			return
		}
		args, err := officialGenerateProceduralModelArgs(request)
		if err != nil {
			writeTaskAPIError(w, http.StatusBadRequest, r.PathValue("task_id"), "bad_request", err.Error(), "fix_request", nil)
			return
		}
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, officialToolGenerateProceduralModel, args)
	})
	mux.HandleFunc("POST /session/{task_id}/official/wait-job", func(w http.ResponseWriter, r *http.Request) {
		var request officialWaitJobRequest
		if !decodeOfficialHTTPRequest(w, r, &request) {
			return
		}
		args, err := officialWaitJobArgs(request)
		if err != nil {
			writeTaskAPIError(w, http.StatusBadRequest, r.PathValue("task_id"), "bad_request", err.Error(), "fix_request", nil)
			return
		}
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, officialToolWaitJobFinished, args)
	})
	mux.HandleFunc("POST /session/{task_id}/official/search-creator-store", func(w http.ResponseWriter, r *http.Request) {
		var request officialSearchCreatorStoreRequest
		if !decodeOfficialHTTPRequest(w, r, &request) {
			return
		}
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, officialToolSearchAsset, officialSearchCreatorStoreArgs(request))
	})
	mux.HandleFunc("POST /session/{task_id}/official/insert-from-creator-store", func(w http.ResponseWriter, r *http.Request) {
		var request officialInsertFromCreatorStoreRequest
		if !decodeOfficialHTTPRequest(w, r, &request) {
			return
		}
		args, err := officialInsertFromCreatorStoreArgs(request)
		if err != nil {
			writeTaskAPIError(w, http.StatusBadRequest, r.PathValue("task_id"), "bad_request", err.Error(), "fix_request", nil)
			return
		}
		runOfficialHTTP(w, r, taskSessions, studioManager, runner, officialToolInsertAsset, args)
	})
}

func decodeOfficialHTTPRequest(w http.ResponseWriter, r *http.Request, out any) bool {
	if err := json.NewDecoder(r.Body).Decode(out); err != nil {
		writeTaskAPIError(w, http.StatusBadRequest, r.PathValue("task_id"), "bad_request", err.Error(), "fix_request", nil)
		return false
	}
	return true
}

func runOfficialHTTP(
	w http.ResponseWriter,
	r *http.Request,
	taskSessions *tasksession.Registry,
	studioManager *studio.Manager,
	runner officialToolRunner,
	toolName string,
	args map[string]any,
) {
	taskID := r.PathValue("task_id")
	status := taskSessions.Status(taskID)
	if !status.OK || status.Contract == nil {
		writeJSON(w, http.StatusNotFound, map[string]any{
			"ok":      false,
			"code":    "task_not_registered",
			"message": fmt.Sprintf("helper2 has no registered session for task_id %s", taskID),
			"task_id": taskID,
			"state":   status.State,
		})
		return
	}
	if !requireTaskSessionToken(w, r, taskID, status) {
		return
	}
	payload, statusCode := runOfficialTaskTool(r.Context(), taskID, taskSessions, studioManager, runner, toolName, args)
	writeJSON(w, statusCode, payload)
}

func runOfficialTaskTool(
	ctx context.Context,
	taskID string,
	taskSessions *tasksession.Registry,
	studioManager *studio.Manager,
	runner officialToolRunner,
	toolName string,
	args map[string]any,
) (map[string]any, int) {
	status := taskSessions.Status(taskID)
	if !status.OK || status.Contract == nil {
		return map[string]any{
			"ok":      false,
			"code":    "task_not_registered",
			"message": fmt.Sprintf("helper2 has no registered session for task_id %s", taskID),
			"task_id": taskID,
			"state":   status.State,
		}, http.StatusNotFound
	}
	if status.State != "live" {
		return map[string]any{
			"ok":      false,
			"code":    "task_not_live",
			"message": "task session is not live",
			"task_id": taskID,
			"state":   status.State,
			"action":  "restart_task_agent",
		}, http.StatusConflict
	}
	managedStudio, err := studioManager.ManagedProcessForTask(taskID)
	if err != nil {
		return map[string]any{
			"ok":      false,
			"code":    "studio_not_available",
			"message": err.Error(),
			"task_id": taskID,
			"action":  "wait_for_studio",
		}, http.StatusConflict
	}
	response, err := runner.Run(ctx, managedStudio, toolName, args)
	payload := map[string]any{
		"ok":         err == nil,
		"task_id":    taskID,
		"place_id":   managedStudio.PlaceID,
		"studio_pid": managedStudio.PID,
		"official":   response,
	}
	if err != nil {
		payload["code"] = officialErrorCode(err)
		payload["message"] = err.Error()
		payload["action"] = "retry_or_check_studio"
		return payload, http.StatusBadGateway
	}
	return payload, http.StatusOK
}

func officialErrorCode(err error) string {
	var business officialToolBusinessError
	if errors.As(err, &business) {
		return "official_tool_error"
	}
	message := err.Error()
	if strings.Contains(message, "context canceled") || strings.Contains(message, "signal: killed") {
		return "official_cli_canceled"
	}
	if strings.Contains(message, "multiple Roblox Studio") {
		return "official_studio_binding_ambiguous"
	}
	if strings.Contains(message, "did not find any connected Roblox Studio") {
		return "official_studio_unavailable"
	}
	if strings.Contains(message, "StudioMCP.exe") || strings.Contains(message, "executable file not found") {
		return "official_cli_unavailable"
	}
	if strings.Contains(message, "JSON-RPC error") {
		return "official_jsonrpc_error"
	}
	if strings.Contains(message, "failed to read official CLI response") || strings.Contains(message, "failed to write official CLI request") {
		return "official_cli_io_error"
	}
	if strings.Contains(message, "failed to parse official CLI response") || strings.Contains(message, "failed to decode official CLI") {
		return "official_cli_protocol_error"
	}
	return "official_cli_error"
}

func officialStoreImageArgs(request officialStoreImageRequest) (map[string]any, error) {
	if strings.TrimSpace(request.FilePath) == "" {
		return nil, fmt.Errorf("file_path is required")
	}
	return map[string]any{"filePath": request.FilePath}, nil
}

func officialGenerateMeshArgs(request officialGenerateMeshRequest) (map[string]any, error) {
	if strings.TrimSpace(request.TextPrompt) == "" {
		return nil, fmt.Errorf("text_prompt is required")
	}
	args := map[string]any{"textPrompt": request.TextPrompt}
	if request.Size != nil {
		args["size"] = map[string]any{"x": request.Size.X, "y": request.Size.Y, "z": request.Size.Z}
	}
	if request.MaxTriangles != nil {
		args["maxTriangles"] = *request.MaxTriangles
	}
	if strings.TrimSpace(request.PartNames) != "" {
		args["partNames"] = request.PartNames
	}
	return args, nil
}

func officialGenerateProceduralModelArgs(request officialGenerateProceduralModelRequest) (map[string]any, error) {
	args := map[string]any{"prompt": request.Prompt}
	if strings.TrimSpace(request.AttachedImageURI) != "" {
		args["attachedImageUri"] = request.AttachedImageURI
	}
	if strings.TrimSpace(request.PartNames) != "" {
		args["partNames"] = request.PartNames
	}
	return args, nil
}

func officialWaitJobArgs(request officialWaitJobRequest) (map[string]any, error) {
	if strings.TrimSpace(request.GenerationID) == "" {
		return nil, fmt.Errorf("generation_id is required")
	}
	args := map[string]any{"generationId": strings.TrimSpace(request.GenerationID)}
	if request.Timeout != nil {
		args["timeout"] = *request.Timeout
	} else {
		args["timeout"] = float64(1)
	}
	return args, nil
}

func officialSearchCreatorStoreArgs(request officialSearchCreatorStoreRequest) map[string]any {
	args := map[string]any{"scope": "creator_store"}
	if strings.TrimSpace(request.Query) != "" {
		args["query"] = request.Query
	}
	if strings.TrimSpace(request.AssetType) != "" {
		args["assetType"] = request.AssetType
	}
	if request.MaxResults != nil {
		args["maxResults"] = *request.MaxResults
	}
	if strings.TrimSpace(request.PriceFilter) != "" {
		args["priceFilter"] = request.PriceFilter
	}
	if request.MinPriceCents != nil {
		args["minPriceCents"] = *request.MinPriceCents
	}
	if request.MaxPriceCents != nil {
		args["maxPriceCents"] = *request.MaxPriceCents
	}
	if request.VerifiedCreatorsOnly != nil {
		args["verifiedCreatorsOnly"] = *request.VerifiedCreatorsOnly
	}
	return args
}

func officialInsertFromCreatorStoreArgs(request officialInsertFromCreatorStoreRequest) (map[string]any, error) {
	if strings.TrimSpace(request.AssetID) == "" {
		return nil, fmt.Errorf("asset_id is required")
	}
	args := map[string]any{"assetId": strings.TrimSpace(request.AssetID)}
	if strings.TrimSpace(request.AssetName) != "" {
		args["assetName"] = request.AssetName
	}
	if strings.TrimSpace(request.AssetType) != "" {
		args["assetType"] = request.AssetType
	}
	if strings.TrimSpace(request.ParentPath) != "" {
		args["parentPath"] = request.ParentPath
	}
	return args, nil
}
