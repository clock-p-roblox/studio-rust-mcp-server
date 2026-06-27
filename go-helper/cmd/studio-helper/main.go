package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"os"
	"os/signal"
	"strconv"
	"sync"
	"syscall"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/screenshot"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
)

const defaultAutoStartPlaceID = "105986423068266"

type healthResponse struct {
	OK      bool   `json:"ok"`
	Service string `json:"service"`
}

type mcp2MessageType string

const (
	mcp2MessageTypeCommand           mcp2MessageType = "command"
	mcp2MessageTypePong              mcp2MessageType = "pong"
	mcp2MessageTypeShouldRestartPull mcp2MessageType = "should_restart_pull"
)

type mcp2CommandKind string

const (
	mcp2CommandDebugEcho     mcp2CommandKind = "debug_echo"
	mcp2CommandDebugStopLoop mcp2CommandKind = "debug_stop_loop"
	mcp2CommandStudioPlay    mcp2CommandKind = "studio_play"
	mcp2CommandStudioStop    mcp2CommandKind = "studio_stop"
)

type mcp2Command struct {
	CommandID int64           `json:"command_id,omitempty"`
	Type      mcp2MessageType `json:"type"`
	Kind      mcp2CommandKind `json:"kind,omitempty"`
	Args      any             `json:"args,omitempty"`
}

type mcp2PullResponse struct {
	CommandID int64           `json:"command_id,omitempty"`
	Type      mcp2MessageType `json:"type"`
	Kind      mcp2CommandKind `json:"kind,omitempty"`
	Reason    string          `json:"reason,omitempty"`
	Args      any             `json:"args,omitempty"`
}

type mcp2ResponseResult struct {
	CommandID  int64          `json:"command_id"`
	Mode       string         `json:"mode,omitempty"`
	ModeSeq    int64          `json:"mode_seq,omitempty"`
	OK         bool           `json:"ok"`
	Result     map[string]any `json:"result,omitempty"`
	Error      string         `json:"error,omitempty"`
	ReceivedAt time.Time      `json:"received_at"`
}

type debugCommandArgs struct {
	PlaceID    string `json:"place_id"`
	EnqueuedAt string `json:"enqueued_at"`
	SleepMS    int64  `json:"sleep_ms,omitempty"`
}

type studioPlayCommandArgs struct {
	PlaceID string `json:"place_id"`
	Mode    string `json:"mode"`
}

type studioStopCommandArgs struct {
	PlaceID string `json:"place_id"`
}

type mcp2ChannelSummary struct {
	ActiveMode             string       `json:"active_mode"`
	ActiveModeSeq          *int64       `json:"active_mode_seq"`
	ActiveStudioPID        *int         `json:"active_studio_pid"`
	LastPullAt             *time.Time   `json:"last_pull_at"`
	Stale                  bool         `json:"stale"`
	QueuedCommandCount     int          `json:"queued_command_count"`
	WaitingPull            bool         `json:"waiting_pull"`
	WaitingPullCount       int          `json:"waiting_pull_count"`
	WaitingResponseCommand *mcp2Command `json:"waiting_response_command"`
	ResultCount            int          `json:"result_count"`
}

type mcp2CommandBroker struct {
	mu                     sync.Mutex
	nextCommandID          int64
	activeMode             string
	activeModeSeq          *int64
	activeStudioPID        *int
	lastPullAt             *time.Time
	waitingPullCount       int
	waitingResponseCommand *mcp2Command
	pending                []mcp2Command
	results                []mcp2ResponseResult
	notify                 chan struct{}
}

func newMCP2CommandBroker() *mcp2CommandBroker {
	return &mcp2CommandBroker{
		nextCommandID: 1,
		activeMode:    "wait_init_mode",
		notify:        make(chan struct{}),
	}
}

func (b *mcp2CommandBroker) ping(mode string, modeSeq int64, studioPID int) mcp2PullResponse {
	b.mu.Lock()
	defer b.mu.Unlock()

	b.acceptLifecycleLocked(mode, modeSeq, studioPID)
	now := time.Now()
	b.lastPullAt = &now
	b.setActiveStudioPIDLocked(studioPID)
	return mcp2PullResponse{
		Type: mcp2MessageTypePong,
	}
}

func (b *mcp2CommandBroker) pull(ctx context.Context, mode string, modeSeq int64, studioPID int, timeout time.Duration) (mcp2PullResponse, bool) {
	timer := time.NewTimer(timeout)
	defer timer.Stop()

	b.mu.Lock()
	b.acceptLifecycleLocked(mode, modeSeq, studioPID)
	now := time.Now()
	b.lastPullAt = &now
	b.setActiveStudioPIDLocked(studioPID)
	b.waitingPullCount++
	b.mu.Unlock()

	defer func() {
		b.mu.Lock()
		b.waitingPullCount--
		b.mu.Unlock()
	}()

	for {
		b.mu.Lock()
		if b.activeModeSeq == nil || *b.activeModeSeq != modeSeq {
			b.mu.Unlock()
			return mcp2PullResponse{}, true
		}
		if len(b.pending) > 0 {
			command := b.pending[0]
			b.pending = b.pending[1:]
			commandCopy := command
			b.waitingResponseCommand = &commandCopy
			b.mu.Unlock()
			return mcp2PullResponse{
				CommandID: command.CommandID,
				Type:      command.Type,
				Kind:      command.Kind,
				Args:      command.Args,
			}, false
		}
		notify := b.notify
		b.mu.Unlock()

		select {
		case <-ctx.Done():
			return mcp2PullResponse{
				Type:   mcp2MessageTypeShouldRestartPull,
				Reason: "request_context_closed",
			}, false
		case <-timer.C:
			return mcp2PullResponse{
				Type:   mcp2MessageTypeShouldRestartPull,
				Reason: "no_command_timeout",
			}, false
		case <-notify:
		}
	}
}

func (b *mcp2CommandBroker) enqueueDebug(placeID string, kind string, sleepMS int64) (mcp2Command, error) {
	commandKind := mcp2CommandKind(kind)
	if kind == "" {
		commandKind = mcp2CommandDebugEcho
	}
	if commandKind != mcp2CommandDebugEcho && commandKind != mcp2CommandDebugStopLoop {
		return mcp2Command{}, fmt.Errorf("unsupported debug command kind: %s", kind)
	}
	args := debugCommandArgs{
		PlaceID:    placeID,
		EnqueuedAt: time.Now().Format(time.RFC3339Nano),
	}
	if sleepMS > 0 {
		args.SleepMS = sleepMS
	}
	return b.enqueue(mcp2MessageTypeCommand, commandKind, args)
}

func (b *mcp2CommandBroker) enqueueStudioPlay(placeID string) (mcp2Command, error) {
	return b.enqueue(mcp2MessageTypeCommand, mcp2CommandStudioPlay, studioPlayCommandArgs{
		PlaceID: placeID,
		Mode:    "start_play",
	})
}

func (b *mcp2CommandBroker) enqueueStudioStop(placeID string) (mcp2Command, error) {
	return b.enqueue(mcp2MessageTypeCommand, mcp2CommandStudioStop, studioStopCommandArgs{
		PlaceID: placeID,
	})
}

func (b *mcp2CommandBroker) enqueue(commandType mcp2MessageType, commandKind mcp2CommandKind, args any) (mcp2Command, error) {
	b.mu.Lock()
	defer b.mu.Unlock()

	if b.activeMode == "wait_init_mode" {
		return mcp2Command{}, errors.New("mcp2 channel is waiting for initial mode")
	}
	command := mcp2Command{
		CommandID: b.nextCommandID,
		Type:      commandType,
		Kind:      commandKind,
		Args:      args,
	}
	b.nextCommandID++
	b.pending = append(b.pending, command)
	b.signalLocked()
	return command, nil
}

func (b *mcp2CommandBroker) record(result mcp2ResponseResult) {
	b.mu.Lock()
	defer b.mu.Unlock()

	if b.waitingResponseCommand != nil && b.waitingResponseCommand.CommandID == result.CommandID {
		b.waitingResponseCommand = nil
	}
	b.results = append(b.results, result)
	if len(b.results) > 200 {
		b.results = b.results[len(b.results)-200:]
	}
}

func (b *mcp2CommandBroker) markStaleIfNeeded(staleAfter time.Duration) (int, bool) {
	b.mu.Lock()
	defer b.mu.Unlock()

	if b.activeMode == "wait_init_mode" || b.lastPullAt == nil || time.Since(*b.lastPullAt) <= staleAfter {
		return 0, false
	}

	studioPID := 0
	if b.activeStudioPID != nil {
		studioPID = *b.activeStudioPID
	}
	b.activeMode = "wait_init_mode"
	b.activeModeSeq = nil
	b.activeStudioPID = nil
	b.lastPullAt = nil
	b.pending = nil
	b.waitingResponseCommand = nil
	b.signalLocked()
	return studioPID, true
}

func (b *mcp2CommandBroker) summary() mcp2ChannelSummary {
	b.mu.Lock()
	defer b.mu.Unlock()

	var activeModeSeq *int64
	if b.activeModeSeq != nil {
		value := *b.activeModeSeq
		activeModeSeq = &value
	}
	var activeStudioPID *int
	if b.activeStudioPID != nil {
		value := *b.activeStudioPID
		activeStudioPID = &value
	}
	var lastPullAt *time.Time
	if b.lastPullAt != nil {
		value := *b.lastPullAt
		lastPullAt = &value
	}
	var waitingResponseCommand *mcp2Command
	if b.waitingResponseCommand != nil {
		value := *b.waitingResponseCommand
		waitingResponseCommand = &value
	}

	stale := false
	if lastPullAt != nil {
		stale = time.Since(*lastPullAt) > 60*time.Second
	}

	return mcp2ChannelSummary{
		ActiveMode:             b.activeMode,
		ActiveModeSeq:          activeModeSeq,
		ActiveStudioPID:        activeStudioPID,
		LastPullAt:             lastPullAt,
		Stale:                  stale,
		QueuedCommandCount:     len(b.pending),
		WaitingPull:            b.waitingPullCount > 0,
		WaitingPullCount:       b.waitingPullCount,
		WaitingResponseCommand: waitingResponseCommand,
		ResultCount:            len(b.results),
	}
}

func (b *mcp2CommandBroker) acceptLifecycleLocked(mode string, modeSeq int64, studioPID int) {
	if b.activeModeSeq != nil && *b.activeModeSeq == modeSeq {
		b.setActiveStudioPIDLocked(studioPID)
		return
	}

	b.activeMode = mode
	seq := modeSeq
	b.activeModeSeq = &seq
	b.activeStudioPID = nil
	b.setActiveStudioPIDLocked(studioPID)
	b.pending = nil
	b.waitingResponseCommand = nil
	b.signalLocked()
}

func (b *mcp2CommandBroker) setActiveStudioPIDLocked(studioPID int) {
	if studioPID <= 0 {
		return
	}
	value := studioPID
	b.activeStudioPID = &value
}

func (b *mcp2CommandBroker) signalLocked() {
	close(b.notify)
	b.notify = make(chan struct{})
}

func main() {
	addr := flag.String("addr", defaultAddr(), "HTTP listen address")
	autoStartPlaceID := flag.String("auto-start-place-id", defaultAutoStartPlaceID, "Roblox place id to launch when helper starts; empty disables auto launch")
	mcp2StaleAfter := flag.Duration("mcp2-stale-after", 60*time.Second, "mcp2 channel stale timeout")
	mcp2StaleCheckInterval := flag.Duration("mcp2-stale-check-interval", 5*time.Second, "mcp2 channel stale watchdog interval")
	flag.Parse()

	logger := slog.New(slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{
		Level: slog.LevelInfo,
	}))
	studioManager, err := studio.NewManager(logger)
	if err != nil {
		logger.Error("failed to create Studio manager", "error", err)
		os.Exit(1)
	}
	commandBroker := newMCP2CommandBroker()
	defer func() {
		if err := studioManager.Close(); err != nil {
			logger.Error("failed to close Studio manager", "error", err)
		}
	}()

	mux := http.NewServeMux()
	mux.HandleFunc("GET /healthz", func(w http.ResponseWriter, r *http.Request) {
		writeJSON(w, http.StatusOK, healthResponse{
			OK:      true,
			Service: "studio-helper",
		})
	})
	mux.HandleFunc("GET /plugin/mcp2/pull_command", func(w http.ResponseWriter, r *http.Request) {
		mode := r.URL.Query().Get("mode")
		modeSeq := r.URL.Query().Get("mode_seq")
		onlyPing := r.URL.Query().Get("only_ping") == "1"
		parsedModeSeq, err := parseModeSeq(modeSeq)
		if err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": err.Error(),
			})
			return
		}
		studioPID := resolvePeerStudioPID(r, *addr, logger)
		if onlyPing {
			response := commandBroker.ping(mode, parsedModeSeq, studioPID)
			logger.Info(
				"mcp2 ping",
				"remote_addr", r.RemoteAddr,
				"mode", mode,
				"mode_seq", modeSeq,
				"studio_pid", studioPID,
			)
			writeJSON(w, http.StatusOK, response)
			return
		}
		response, closeConnection := commandBroker.pull(r.Context(), mode, parsedModeSeq, studioPID, 10*time.Second)
		if closeConnection {
			logger.Info("mcp2 pull superseded; closing http connection", "remote_addr", r.RemoteAddr, "mode", mode, "mode_seq", modeSeq)
			if err := closeHTTPConnection(w); err != nil {
				logger.Warn("failed to close superseded mcp2 pull connection", "remote_addr", r.RemoteAddr, "mode", mode, "mode_seq", modeSeq, "error", err)
			}
			return
		}
		logger.Info(
			"mcp2 pull completed",
			"remote_addr", r.RemoteAddr,
			"user_agent", r.UserAgent(),
			"mode", mode,
			"mode_seq", modeSeq,
			"studio_pid", studioPID,
			"command_id", response.CommandID,
			"type", response.Type,
			"kind", response.Kind,
			"reason", response.Reason,
		)
		writeJSON(w, http.StatusOK, response)
	})
	mux.HandleFunc("POST /plugin/mcp2/response_result", func(w http.ResponseWriter, r *http.Request) {
		var result mcp2ResponseResult
		if err := json.NewDecoder(r.Body).Decode(&result); err != nil {
			logger.Warn("invalid mcp2 response result", "remote_addr", r.RemoteAddr, "error", err)
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": err.Error(),
			})
			return
		}
		if result.CommandID <= 0 {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": "command_id must be positive",
			})
			return
		}

		result.ReceivedAt = time.Now()
		commandBroker.record(result)
		logger.Info(
			"mcp2 response acknowledged",
			"remote_addr", r.RemoteAddr,
			"mode", result.Mode,
			"mode_seq", result.ModeSeq,
			"command_id", result.CommandID,
			"ok", result.OK,
			"error", result.Error,
		)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"command_id": result.CommandID,
		})
	})
	mux.HandleFunc("POST /debug/command/{placeid}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("placeid")
		if !studio.PlaceIDIsValid(placeID) {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": "placeid must contain digits only",
			})
			return
		}
		var request struct {
			Kind    string `json:"kind"`
			SleepMS int64  `json:"sleep_ms"`
		}
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil && !errors.Is(err, io.EOF) {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": err.Error(),
			})
			return
		}
		command, err := commandBroker.enqueueDebug(placeID, request.Kind, request.SleepMS)
		if err != nil {
			writeJSON(w, http.StatusConflict, map[string]any{
				"ok":    false,
				"error": err.Error(),
			})
			return
		}
		logger.Info("debug mcp2 command enqueued", "place_id", placeID, "command_id", command.CommandID, "kind", command.Kind)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":      true,
			"command": command,
		})
	})
	mux.HandleFunc("POST /debug/studio/play/{placeid}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("placeid")
		if !studio.PlaceIDIsValid(placeID) {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": "placeid must contain digits only",
			})
			return
		}
		command, err := commandBroker.enqueueStudioPlay(placeID)
		if err != nil {
			writeJSON(w, http.StatusConflict, map[string]any{
				"ok":    false,
				"error": err.Error(),
			})
			return
		}
		logger.Info("studio play command enqueued", "place_id", placeID, "command_id", command.CommandID, "kind", command.Kind)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"queued":     true,
			"command_id": command.CommandID,
			"command":    command,
		})
	})
	mux.HandleFunc("POST /debug/studio/stop/{placeid}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("placeid")
		if !studio.PlaceIDIsValid(placeID) {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": "placeid must contain digits only",
			})
			return
		}
		command, err := commandBroker.enqueueStudioStop(placeID)
		if err != nil {
			writeJSON(w, http.StatusConflict, map[string]any{
				"ok":    false,
				"error": err.Error(),
			})
			return
		}
		logger.Info("studio stop command enqueued", "place_id", placeID, "command_id", command.CommandID, "kind", command.Kind)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"queued":     true,
			"command_id": command.CommandID,
			"command":    command,
		})
	})
	mux.HandleFunc("GET /studio/summary", func(w http.ResponseWriter, r *http.Request) {
		summary := struct {
			studio.Summary
			MCP2Channel mcp2ChannelSummary `json:"mcp2_channel"`
		}{
			Summary:     studioManager.Summary(),
			MCP2Channel: commandBroker.summary(),
		}
		printPrettyJSON("studio summary", summary)
		writeJSON(w, http.StatusOK, summary)
	})
	mux.HandleFunc("POST /debug/start-roblox-studio/{placeid}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("placeid")
		result, err := studioManager.StartDebug(r.Context(), placeID)
		if err != nil {
			logger.Error("failed to start Roblox Studio", "place_id", placeID, "error", err)
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":       false,
				"place_id": placeID,
				"error":    err.Error(),
			})
			return
		}
		logger.Info(
			"started Roblox Studio",
			"place_id", result.PlaceID,
			"universe_id", result.UniverseID,
			"pid", result.PID,
			"studio_path", result.StudioPath,
		)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":     true,
			"launch": result,
		})
	})
	mux.HandleFunc("GET /debug/studio/screenshot/{placeid}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("placeid")
		if !studio.PlaceIDIsValid(placeID) {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": "placeid must contain digits only",
			})
			return
		}
		studioPID, err := studioManager.ManagedPIDForPlace(placeID)
		if err != nil {
			writeJSON(w, http.StatusConflict, map[string]any{
				"ok":       false,
				"place_id": placeID,
				"error":    err.Error(),
			})
			return
		}
		result, err := screenshot.CaptureStudioScreenshot(r.Context(), studioPID, "", "studio-"+placeID)
		if err != nil {
			logger.Warn("failed to capture Roblox Studio screenshot", "place_id", placeID, "studio_pid", studioPID, "error", err)
			writeJSON(w, http.StatusInternalServerError, map[string]any{
				"ok":         false,
				"place_id":   placeID,
				"studio_pid": studioPID,
				"error":      err.Error(),
			})
			return
		}
		logger.Info(
			"captured Roblox Studio screenshot",
			"place_id", placeID,
			"studio_pid", result.StudioPID,
			"path", result.Path,
			"bytes", result.Bytes,
			"fallback", result.Fallback,
		)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"place_id":   placeID,
			"screenshot": result,
		})
	})

	server := &http.Server{
		Addr:              *addr,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	listener, err := net.Listen("tcp", *addr)
	if err != nil {
		logger.Error("failed to listen", "addr", *addr, "error", err)
		os.Exit(1)
	}

	errCh := make(chan error, 1)
	go func() {
		logger.Info("studio helper http server listening", "addr", listener.Addr().String())
		errCh <- server.Serve(listener)
	}()
	go func() {
		ticker := time.NewTicker(*mcp2StaleCheckInterval)
		defer ticker.Stop()
		for {
			select {
			case <-ticker.C:
				if studioPID, stale := commandBroker.markStaleIfNeeded(*mcp2StaleAfter); stale {
					logger.Warn("mcp2 channel stale; clearing channel and killing matching managed Studio", "studio_pid", studioPID)
					if studioPID > 0 {
						studioManager.KillManagedPID(studioPID)
					}
				}
				studioManager.ReconcileDesired(context.Background())
			}
		}
	}()
	if *autoStartPlaceID != "" {
		go func(placeID string) {
			result, err := studioManager.StartInternal(context.Background(), placeID)
			if err != nil {
				logger.Error("failed to auto-start Roblox Studio", "place_id", placeID, "error", err)
				return
			}
			logger.Info(
				"auto-started Roblox Studio",
				"place_id", result.PlaceID,
				"universe_id", result.UniverseID,
				"pid", result.PID,
				"studio_path", result.StudioPath,
			)
		}(*autoStartPlaceID)
	}

	sigCh := make(chan os.Signal, 1)
	signal.Notify(sigCh, os.Interrupt, syscall.SIGTERM)

	select {
	case sig := <-sigCh:
		logger.Info("shutdown requested", "signal", sig.String())
	case err := <-errCh:
		if !errors.Is(err, http.ErrServerClosed) {
			logger.Error("http server stopped", "error", err)
			os.Exit(1)
		}
		return
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if err := server.Shutdown(ctx); err != nil {
		logger.Error("graceful shutdown failed", "error", err)
		os.Exit(1)
	}

	logger.Info("studio helper http server stopped")
}

func printPrettyJSON(label string, value any) {
	body, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		slog.Error("failed to format console json", "label", label, "error", err)
		return
	}
	fmt.Printf("%s:\n%s\n", label, body)
}

func defaultAddr() string {
	if value := os.Getenv("STUDIO_HELPER_ADDR"); value != "" {
		return value
	}
	return "127.0.0.1:44750"
}

func parseModeSeq(value string) (int64, error) {
	if value == "" {
		return 0, errors.New("mode_seq is required")
	}
	parsed, err := strconv.ParseInt(value, 10, 64)
	if err != nil || parsed <= 0 {
		return 0, fmt.Errorf("mode_seq must be a positive integer: %s", value)
	}
	return parsed, nil
}

func resolvePeerStudioPID(r *http.Request, listenAddr string, logger *slog.Logger) int {
	helperPort, err := listenPort(listenAddr)
	if err != nil {
		logger.Warn("failed to parse helper listen port for peer pid lookup", "addr", listenAddr, "error", err)
		return 0
	}
	peerAddr, err := net.ResolveTCPAddr("tcp", r.RemoteAddr)
	if err != nil {
		logger.Warn("failed to parse mcp2 peer addr for pid lookup", "remote_addr", r.RemoteAddr, "error", err)
		return 0
	}
	pid, err := studio.ResolvePeerProcessID(peerAddr, helperPort)
	if err != nil {
		logger.Warn("failed to resolve mcp2 peer pid", "remote_addr", r.RemoteAddr, "error", err)
		return 0
	}
	return pid
}

func listenPort(addr string) (int, error) {
	_, portText, err := net.SplitHostPort(addr)
	if err != nil {
		return 0, err
	}
	port, err := strconv.Atoi(portText)
	if err != nil || port <= 0 {
		return 0, fmt.Errorf("invalid listen port: %s", portText)
	}
	return port, nil
}

func closeHTTPConnection(w http.ResponseWriter) error {
	conn, _, err := http.NewResponseController(w).Hijack()
	if err != nil {
		return err
	}
	return conn.Close()
}

func writeJSON(w http.ResponseWriter, status int, value any) {
	w.Header().Set("Content-Type", "application/json")
	w.WriteHeader(status)
	if err := json.NewEncoder(w).Encode(value); err != nil {
		slog.Error("failed to write json response", "error", err)
	}
}
