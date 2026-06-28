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
	"sort"
	"strconv"
	"sync"
	"syscall"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/screenshot"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/tasksession"
)

const defaultAutoStartPlaceID = ""

const taskStudioCommandTimeout = 30 * time.Second

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
	mcp2CommandStudioMode    mcp2CommandKind = "studio_mode_query"
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

type studioModeCommandArgs struct {
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

type mcp2CommandTerminal struct {
	CommandID   int64               `json:"command_id"`
	CompletedAt time.Time           `json:"completed_at"`
	Reason      string              `json:"reason"`
	Result      *mcp2ResponseResult `json:"result,omitempty"`
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
	terminals              map[int64]mcp2CommandTerminal
	notify                 chan struct{}
}

func newMCP2CommandBroker() *mcp2CommandBroker {
	return &mcp2CommandBroker{
		nextCommandID: 1,
		activeMode:    "wait_init_mode",
		terminals:     make(map[int64]mcp2CommandTerminal),
		notify:        make(chan struct{}),
	}
}

type mcp2StaleChannel struct {
	TaskID    string
	StudioPID int
}

type mcp2CommandBrokerRegistry struct {
	mu    sync.Mutex
	tasks map[string]*mcp2CommandBroker
	debug *mcp2CommandBroker
}

func newMCP2CommandBrokerRegistry() *mcp2CommandBrokerRegistry {
	return &mcp2CommandBrokerRegistry{
		tasks: make(map[string]*mcp2CommandBroker),
		debug: newMCP2CommandBroker(),
	}
}

func (r *mcp2CommandBrokerRegistry) forTask(taskID string) *mcp2CommandBroker {
	r.mu.Lock()
	defer r.mu.Unlock()
	broker := r.tasks[taskID]
	if broker == nil {
		broker = newMCP2CommandBroker()
		r.tasks[taskID] = broker
	}
	return broker
}

func (r *mcp2CommandBrokerRegistry) getTask(taskID string) (*mcp2CommandBroker, bool) {
	r.mu.Lock()
	defer r.mu.Unlock()
	broker := r.tasks[taskID]
	return broker, broker != nil
}

func (r *mcp2CommandBrokerRegistry) forDebug() *mcp2CommandBroker {
	return r.debug
}

func (r *mcp2CommandBrokerRegistry) removeTask(taskID string, reason string) {
	r.mu.Lock()
	broker := r.tasks[taskID]
	delete(r.tasks, taskID)
	r.mu.Unlock()
	if broker != nil {
		broker.cleanup(reason)
	}
}

func (r *mcp2CommandBrokerRegistry) summaryForTask(taskID string) mcp2ChannelSummary {
	r.mu.Lock()
	broker := r.tasks[taskID]
	r.mu.Unlock()
	if broker == nil {
		return newMCP2CommandBroker().summary()
	}
	return broker.summary()
}

func (r *mcp2CommandBrokerRegistry) recentTerminalsForTask(taskID string, limit int) []mcp2CommandTerminal {
	r.mu.Lock()
	broker := r.tasks[taskID]
	r.mu.Unlock()
	if broker == nil {
		return []mcp2CommandTerminal{}
	}
	return broker.recentTerminals(limit)
}

func (r *mcp2CommandBrokerRegistry) summaries() map[string]mcp2ChannelSummary {
	r.mu.Lock()
	snapshot := make(map[string]*mcp2CommandBroker, len(r.tasks))
	for taskID, broker := range r.tasks {
		snapshot[taskID] = broker
	}
	debug := r.debug
	r.mu.Unlock()

	summaries := make(map[string]mcp2ChannelSummary, len(snapshot)+1)
	for taskID, broker := range snapshot {
		summaries[taskID] = broker.summary()
	}
	summaries["debug"] = debug.summary()
	return summaries
}

func (r *mcp2CommandBrokerRegistry) markStaleIfNeeded(staleAfter time.Duration) []mcp2StaleChannel {
	r.mu.Lock()
	snapshot := make(map[string]*mcp2CommandBroker, len(r.tasks))
	for taskID, broker := range r.tasks {
		snapshot[taskID] = broker
	}
	debug := r.debug
	r.mu.Unlock()

	stale := make([]mcp2StaleChannel, 0)
	for taskID, broker := range snapshot {
		if studioPID, isStale := broker.markStaleIfNeeded(staleAfter); isStale {
			stale = append(stale, mcp2StaleChannel{TaskID: taskID, StudioPID: studioPID})
		}
	}
	if studioPID, isStale := debug.markStaleIfNeeded(staleAfter); isStale {
		stale = append(stale, mcp2StaleChannel{TaskID: "debug", StudioPID: studioPID})
	}
	return stale
}

func (b *mcp2CommandBroker) ping(mode string, modeSeq int64, studioPID int) (mcp2PullResponse, bool) {
	b.mu.Lock()
	defer b.mu.Unlock()

	if studioPID <= 0 {
		return mcp2PullResponse{
			Type:   mcp2MessageTypePong,
			Reason: "invalid_lifecycle_source",
		}, false
	}
	if b.activeModeSeq == nil || *b.activeModeSeq != modeSeq {
		return mcp2PullResponse{
			Type:   mcp2MessageTypePong,
			Reason: "wrong_lifecycle",
		}, false
	}
	if b.activeStudioPID == nil || *b.activeStudioPID != studioPID {
		return mcp2PullResponse{
			Type:   mcp2MessageTypePong,
			Reason: "studio_pid_mismatch",
		}, false
	}
	now := time.Now()
	b.lastPullAt = &now
	return mcp2PullResponse{
		Type: mcp2MessageTypePong,
	}, true
}

func (b *mcp2CommandBroker) pull(ctx context.Context, mode string, modeSeq int64, studioPID int, timeout time.Duration) (mcp2PullResponse, bool) {
	timer := time.NewTimer(timeout)
	defer timer.Stop()

	b.mu.Lock()
	if !b.acceptLifecycleLocked(mode, modeSeq, studioPID) {
		b.mu.Unlock()
		return mcp2PullResponse{
			Type:   mcp2MessageTypeShouldRestartPull,
			Reason: "invalid_lifecycle_source",
		}, false
	}
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

func (b *mcp2CommandBroker) enqueueStudioModeQuery(placeID string) (mcp2Command, error) {
	return b.enqueue(mcp2MessageTypeCommand, mcp2CommandStudioMode, studioModeCommandArgs{
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

func (b *mcp2CommandBroker) record(result mcp2ResponseResult, studioPID int) (bool, string) {
	b.mu.Lock()
	defer b.mu.Unlock()

	if studioPID <= 0 {
		return false, "invalid_lifecycle_source"
	}
	if b.activeStudioPID == nil || *b.activeStudioPID != studioPID {
		return false, "studio_pid_mismatch"
	}
	if b.activeModeSeq == nil || result.ModeSeq != *b.activeModeSeq {
		return false, "mode_seq_mismatch"
	}
	if b.waitingResponseCommand == nil || b.waitingResponseCommand.CommandID != result.CommandID {
		return false, "not_waiting_for_command"
	}

	b.results = append(b.results, result)
	if len(b.results) > 200 {
		b.results = b.results[len(b.results)-200:]
	}
	resultCopy := result
	b.completeCommandLocked(result.CommandID, "response", &resultCopy)
	return true, ""
}

func (b *mcp2CommandBroker) waitForTerminal(ctx context.Context, commandID int64) (mcp2CommandTerminal, bool) {
	for {
		b.mu.Lock()
		if terminal, ok := b.terminals[commandID]; ok {
			b.mu.Unlock()
			return terminal, true
		}
		notify := b.notify
		b.mu.Unlock()

		select {
		case <-ctx.Done():
			return mcp2CommandTerminal{}, false
		case <-notify:
		}
	}
}

func (b *mcp2CommandBroker) recentTerminals(limit int) []mcp2CommandTerminal {
	b.mu.Lock()
	defer b.mu.Unlock()
	if limit <= 0 {
		limit = 20
	}
	terminals := make([]mcp2CommandTerminal, 0, len(b.terminals))
	for _, terminal := range b.terminals {
		terminals = append(terminals, terminal)
	}
	sort.Slice(terminals, func(i, j int) bool {
		return terminals[i].CompletedAt.Before(terminals[j].CompletedAt)
	})
	if len(terminals) > limit {
		terminals = terminals[len(terminals)-limit:]
	}
	return terminals
}

func (b *mcp2CommandBroker) completeCommandLocked(commandID int64, reason string, result *mcp2ResponseResult) bool {
	if _, exists := b.terminals[commandID]; exists {
		return false
	}
	for i := 0; i < len(b.pending); i++ {
		if b.pending[i].CommandID == commandID {
			b.pending = append(b.pending[:i], b.pending[i+1:]...)
			break
		}
	}
	if b.waitingResponseCommand != nil && b.waitingResponseCommand.CommandID == commandID {
		b.waitingResponseCommand = nil
	}
	b.terminals[commandID] = mcp2CommandTerminal{
		CommandID:   commandID,
		CompletedAt: time.Now(),
		Reason:      reason,
		Result:      result,
	}
	if len(b.terminals) > 200 {
		oldestID := int64(0)
		var oldestTime time.Time
		for id, terminal := range b.terminals {
			if oldestID == 0 || terminal.CompletedAt.Before(oldestTime) {
				oldestID = id
				oldestTime = terminal.CompletedAt
			}
		}
		delete(b.terminals, oldestID)
	}
	b.signalLocked()
	return true
}

func (b *mcp2CommandBroker) waitForResult(ctx context.Context, commandID int64) (mcp2ResponseResult, bool) {
	for {
		b.mu.Lock()
		for _, result := range b.results {
			if result.CommandID == commandID {
				b.mu.Unlock()
				return result, true
			}
		}
		notify := b.notify
		b.mu.Unlock()

		select {
		case <-ctx.Done():
			return mcp2ResponseResult{}, false
		case <-notify:
		}
	}
}

func (b *mcp2CommandBroker) cancelCommand(commandID int64) bool {
	return b.cancelCommandWithReason(commandID, "canceled")
}

func (b *mcp2CommandBroker) cancelCommandWithReason(commandID int64, reason string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()

	return b.completeCommandLocked(commandID, reason, nil)
}

func (b *mcp2CommandBroker) cleanup(reason string) {
	b.mu.Lock()
	defer b.mu.Unlock()

	pending := append([]mcp2Command(nil), b.pending...)
	for _, command := range pending {
		b.completeCommandLocked(command.CommandID, reason, nil)
	}
	b.pending = nil
	if b.waitingResponseCommand != nil {
		b.completeCommandLocked(b.waitingResponseCommand.CommandID, reason, nil)
	}
	b.activeMode = "wait_init_mode"
	b.activeModeSeq = nil
	b.activeStudioPID = nil
	b.lastPullAt = nil
	b.signalLocked()
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
	pending := append([]mcp2Command(nil), b.pending...)
	for _, command := range pending {
		b.completeCommandLocked(command.CommandID, "stale", nil)
	}
	b.pending = nil
	if b.waitingResponseCommand != nil {
		b.completeCommandLocked(b.waitingResponseCommand.CommandID, "stale", nil)
	}
	b.signalLocked()
	return studioPID, true
}

func (b *mcp2CommandBroker) reset() {
	b.cleanup("reset")
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

func (b *mcp2CommandBroker) acceptLifecycleLocked(mode string, modeSeq int64, studioPID int) bool {
	if studioPID <= 0 {
		return false
	}
	if b.activeModeSeq != nil && *b.activeModeSeq == modeSeq {
		if b.activeStudioPID != nil && *b.activeStudioPID != studioPID {
			return false
		}
		b.setActiveStudioPIDLocked(studioPID)
		return true
	}

	b.activeMode = mode
	seq := modeSeq
	b.activeModeSeq = &seq
	b.activeStudioPID = nil
	b.setActiveStudioPIDLocked(studioPID)
	pending := append([]mcp2Command(nil), b.pending...)
	for _, command := range pending {
		b.completeCommandLocked(command.CommandID, "mode_seq_changed", nil)
	}
	b.pending = nil
	if b.waitingResponseCommand != nil {
		b.completeCommandLocked(b.waitingResponseCommand.CommandID, "mode_seq_changed", nil)
	}
	b.signalLocked()
	return true
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
	commandBrokers := newMCP2CommandBrokerRegistry()
	taskSessions := tasksession.NewRegistry(31*time.Second, studioManager)
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
	mux.HandleFunc("POST /session/{task_id}/heartbeat", func(w http.ResponseWriter, r *http.Request) {
		pathTaskID := r.PathValue("task_id")
		var request tasksession.HeartbeatRequest
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":      false,
				"code":    "bad_request",
				"message": err.Error(),
			})
			return
		}
		response, err := taskSessions.Heartbeat(pathTaskID, request)
		if err != nil {
			writeSessionError(w, err)
			return
		}
		logger.Info(
			"task heartbeat accepted",
			"task_id", response.TaskID,
			"machine_name", request.MachineName,
			"place_id", request.PlaceID,
			"task_agent_pid", request.TaskAgentPID,
			"rojo_upstream_url", request.RojoUpstreamURL,
		)
		writeJSON(w, http.StatusOK, response)
	})
	mux.HandleFunc("POST /session/{task_id}/release", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		var request tasksession.ReleaseRequest
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":      false,
				"code":    "bad_request",
				"message": err.Error(),
			})
			return
		}
		response, err := taskSessions.Release(taskID, request)
		if err != nil {
			writeSessionError(w, err)
			return
		}
		commandBrokers.removeTask(taskID, "task_released")
		response.KilledPIDs = studioManager.KillTaskStudios(taskID)
		logger.Info("task released", "task_id", response.TaskID, "killed_pids", response.KilledPIDs)
		writeJSON(w, http.StatusOK, response)
	})
	mux.HandleFunc("GET /session/{task_id}/status", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		status := taskSessions.Status(taskID)
		if !status.OK {
			writeJSON(w, http.StatusNotFound, status)
			return
		}
		studioSummary := studioManager.Summary()
		desired := make([]studio.DesiredStudio, 0)
		for _, target := range studioSummary.Desired {
			if target.OwnerKind == "task" && target.OwnerID == taskID {
				desired = append(desired, target)
			}
		}
		studios := make([]studio.ManagedProcess, 0)
		for _, process := range studioSummary.Studios {
			if process.OwnerKind == "task" && process.OwnerID == taskID {
				studios = append(studios, process)
			}
		}
		rojoUpstreamURL := ""
		if status.Contract != nil {
			rojoUpstreamURL = status.Contract.RojoUpstreamURL
		}
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":                status.OK,
			"task_id":           status.TaskID,
			"state":             status.State,
			"contract":          status.Contract,
			"last_heartbeat_at": status.LastHeartbeatAt,
			"lease_age_ms":      status.LeaseAgeMS,
			"lease_timeout_ms":  status.LeaseTimeoutMS,
			"rojo_upstream_url": rojoUpstreamURL,
			"desired_studio":    desired,
			"studios":           studios,
			"mcp2_channel":      commandBrokers.summaryForTask(taskID),
			"recent_commands":   commandBrokers.recentTerminalsForTask(taskID, 20),
		})
	})
	mux.HandleFunc("POST /session/{task_id}/studio/play", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		status := taskSessions.Status(taskID)
		if !status.OK || status.Contract == nil {
			writeJSON(w, http.StatusNotFound, status)
			return
		}
		if status.State != "live" {
			writeTaskAPIError(w, http.StatusConflict, taskID, "task_not_live", "task session is not live", "restart_task_agent", map[string]any{"state": status.State})
			return
		}
		broker := commandBrokers.forTask(taskID)
		command, err := broker.enqueueStudioPlay(status.Contract.PlaceID)
		if err != nil {
			writeTaskAPIError(w, http.StatusConflict, taskID, "mcp2_unavailable", err.Error(), "wait_for_studio_mcp2", nil)
			return
		}
		waitCtx, cancel := context.WithTimeout(r.Context(), taskStudioCommandTimeout)
		defer cancel()
		terminal, found := broker.waitForTerminal(waitCtx, command.CommandID)
		if !found {
			broker.cancelCommandWithReason(command.CommandID, "command_timeout")
			writeTaskAPIError(w, http.StatusGatewayTimeout, taskID, "command_timeout", "mcp2 command did not complete before timeout", "poll_studio_mode", map[string]any{"command_id": command.CommandID})
			return
		}
		writeTaskStudioCommandTerminal(w, taskID, command, terminal)
	})
	mux.HandleFunc("POST /session/{task_id}/studio/stop", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		status := taskSessions.Status(taskID)
		if !status.OK || status.Contract == nil {
			writeJSON(w, http.StatusNotFound, status)
			return
		}
		if status.State != "live" {
			writeTaskAPIError(w, http.StatusConflict, taskID, "task_not_live", "task session is not live", "restart_task_agent", map[string]any{"state": status.State})
			return
		}
		broker := commandBrokers.forTask(taskID)
		command, err := broker.enqueueStudioStop(status.Contract.PlaceID)
		if err != nil {
			writeTaskAPIError(w, http.StatusConflict, taskID, "mcp2_unavailable", err.Error(), "wait_for_studio_mcp2", nil)
			return
		}
		waitCtx, cancel := context.WithTimeout(r.Context(), taskStudioCommandTimeout)
		defer cancel()
		terminal, found := broker.waitForTerminal(waitCtx, command.CommandID)
		if !found {
			broker.cancelCommandWithReason(command.CommandID, "command_timeout")
			writeTaskAPIError(w, http.StatusGatewayTimeout, taskID, "command_timeout", "mcp2 command did not complete before timeout", "poll_studio_mode", map[string]any{"command_id": command.CommandID})
			return
		}
		writeTaskStudioCommandTerminal(w, taskID, command, terminal)
	})
	mux.HandleFunc("GET /session/{task_id}/studio/mode", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		status := taskSessions.Status(taskID)
		if !status.OK || status.Contract == nil {
			writeJSON(w, http.StatusNotFound, status)
			return
		}
		if status.State != "live" {
			writeJSON(w, http.StatusOK, map[string]any{
				"ok":        true,
				"task_id":   taskID,
				"available": false,
				"mode":      "unknown",
				"reason":    "task_session_not_live",
				"state":     status.State,
			})
			return
		}
		broker := commandBrokers.forTask(taskID)
		command, err := broker.enqueueStudioModeQuery(status.Contract.PlaceID)
		if err != nil {
			writeJSON(w, http.StatusOK, map[string]any{
				"ok":        true,
				"task_id":   taskID,
				"available": false,
				"mode":      "unknown",
				"reason":    err.Error(),
			})
			return
		}

		waitCtx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
		defer cancel()
		terminal, found := broker.waitForTerminal(waitCtx, command.CommandID)
		if !found {
			broker.cancelCommandWithReason(command.CommandID, "mode_query_timeout")
			writeJSON(w, http.StatusOK, map[string]any{
				"ok":         true,
				"task_id":    taskID,
				"available":  false,
				"mode":       "unknown",
				"command_id": command.CommandID,
				"reason":     "mode_query_timeout",
			})
			return
		}
		writeTaskStudioModeTerminal(w, taskID, command, terminal)
	})
	mux.HandleFunc("GET /session/{task_id}/studio/screenshot", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		status := taskSessions.Status(taskID)
		if !status.OK || status.Contract == nil {
			writeJSON(w, http.StatusNotFound, status)
			return
		}
		if status.State != "live" {
			writeTaskAPIError(w, http.StatusConflict, taskID, "task_not_live", "task session is not live", "restart_task_agent", map[string]any{"state": status.State})
			return
		}
		managedStudio, err := studioManager.ManagedProcessForTask(taskID)
		if err != nil {
			writeTaskAPIError(w, http.StatusConflict, taskID, "studio_not_available", err.Error(), "wait_for_studio", nil)
			return
		}
		result, err := screenshot.CaptureStudioScreenshotForExactPID(r.Context(), managedStudio.PID, "", "task-"+taskID)
		if err != nil {
			logger.Warn("failed to capture task Roblox Studio screenshot", "task_id", taskID, "studio_pid", managedStudio.PID, "error", err)
			writeTaskAPIError(w, http.StatusInternalServerError, taskID, "screenshot_failed", err.Error(), "retry", map[string]any{"studio_pid": managedStudio.PID})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"task_id":    taskID,
			"studio_pid": managedStudio.PID,
			"screenshot": result,
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
		managedStudio, ok := resolvePeerManagedStudioProcess(r, *addr, studioManager, logger)
		if !ok {
			writeJSON(w, http.StatusForbidden, map[string]any{
				"ok":     false,
				"error":  "invalid_lifecycle_source",
				"reason": "request is not from a helper-managed Roblox Studio process",
			})
			return
		}
		studioPID := managedStudio.PID
		broker := commandBrokers.forDebug()
		if managedStudio.OwnerKind == "task" && managedStudio.OwnerID != "" {
			status := taskSessions.Status(managedStudio.OwnerID)
			if !status.OK || status.State != "live" {
				writeJSON(w, http.StatusForbidden, map[string]any{
					"ok":      false,
					"error":   "task_not_live",
					"reason":  "managed Studio is not bound to a live task session",
					"task_id": managedStudio.OwnerID,
					"state":   status.State,
				})
				return
			}
			broker = commandBrokers.forTask(managedStudio.OwnerID)
		}
		if onlyPing {
			response, accepted := broker.ping(mode, parsedModeSeq, studioPID)
			logger.Info(
				"mcp2 ping",
				"remote_addr", r.RemoteAddr,
				"mode", mode,
				"mode_seq", modeSeq,
				"studio_pid", studioPID,
				"owner_kind", managedStudio.OwnerKind,
				"owner_id", managedStudio.OwnerID,
				"accepted", accepted,
				"reason", response.Reason,
			)
			if !accepted {
				writeJSON(w, http.StatusConflict, response)
				return
			}
			writeJSON(w, http.StatusOK, response)
			return
		}
		response, closeConnection := broker.pull(r.Context(), mode, parsedModeSeq, studioPID, 10*time.Second)
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
			"owner_kind", managedStudio.OwnerKind,
			"owner_id", managedStudio.OwnerID,
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
		managedStudio, ok := resolvePeerManagedStudioProcess(r, *addr, studioManager, logger)
		studioPID := 0
		var broker *mcp2CommandBroker
		ignoredReason := ""
		if ok {
			studioPID = managedStudio.PID
			if managedStudio.OwnerKind == "task" && managedStudio.OwnerID != "" {
				status := taskSessions.Status(managedStudio.OwnerID)
				if !status.OK || status.State != "live" {
					ignoredReason = "task_not_live"
				} else if existing, exists := commandBrokers.getTask(managedStudio.OwnerID); exists {
					broker = existing
				} else {
					ignoredReason = "task_channel_not_found"
				}
			} else {
				broker = commandBrokers.forDebug()
			}
		} else {
			ignoredReason = "invalid_lifecycle_source"
		}
		recorded := false
		if broker != nil {
			recorded, ignoredReason = broker.record(result, studioPID)
		}
		logger.Info(
			"mcp2 response acknowledged",
			"remote_addr", r.RemoteAddr,
			"studio_pid", studioPID,
			"owner_kind", managedStudio.OwnerKind,
			"owner_id", managedStudio.OwnerID,
			"mode", result.Mode,
			"mode_seq", result.ModeSeq,
			"command_id", result.CommandID,
			"ok", result.OK,
			"error", result.Error,
			"recorded", recorded,
			"ignored_reason", ignoredReason,
		)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"command_id": result.CommandID,
			"recorded":   recorded,
			"ignored":    !recorded,
			"reason":     ignoredReason,
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
		command, err := commandBrokers.forDebug().enqueueDebug(placeID, request.Kind, request.SleepMS)
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
		command, err := commandBrokers.forDebug().enqueueStudioPlay(placeID)
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
		command, err := commandBrokers.forDebug().enqueueStudioStop(placeID)
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
	mux.HandleFunc("GET /debug/studio/mode/{placeid}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("placeid")
		if !studio.PlaceIDIsValid(placeID) {
			writeJSON(w, http.StatusBadRequest, map[string]any{
				"ok":    false,
				"error": "placeid must contain digits only",
			})
			return
		}
		debugBroker := commandBrokers.forDebug()
		command, err := debugBroker.enqueueStudioModeQuery(placeID)
		if err != nil {
			writeJSON(w, http.StatusOK, map[string]any{
				"ok":        true,
				"available": false,
				"place_id":  placeID,
				"error":     err.Error(),
			})
			return
		}

		waitCtx, cancel := context.WithTimeout(r.Context(), 5*time.Second)
		defer cancel()
		result, found := debugBroker.waitForResult(waitCtx, command.CommandID)
		if !found {
			debugBroker.cancelCommand(command.CommandID)
			writeJSON(w, http.StatusOK, map[string]any{
				"ok":         true,
				"available":  false,
				"place_id":   placeID,
				"command_id": command.CommandID,
				"reason":     "mode_query_timeout",
			})
			return
		}

		writeJSON(w, http.StatusOK, map[string]any{
			"ok":             true,
			"available":      result.OK,
			"place_id":       placeID,
			"command_id":     command.CommandID,
			"command_result": result,
		})
	})
	mux.HandleFunc("GET /studio/summary", func(w http.ResponseWriter, r *http.Request) {
		summary := struct {
			studio.Summary
			MCP2Channels map[string]mcp2ChannelSummary `json:"mcp2_channels"`
			TaskSessions tasksession.Summary           `json:"task_sessions"`
		}{
			Summary:      studioManager.Summary(),
			MCP2Channels: commandBrokers.summaries(),
			TaskSessions: taskSessions.Summary(),
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
				for _, stopped := range studioManager.PruneStoppedProcesses() {
					if stopped.OwnerKind == "task" && stopped.OwnerID != "" {
						logger.Warn("task-owned Studio stopped; clearing task mcp2 channel", "task_id", stopped.OwnerID, "studio_pid", stopped.PID)
						commandBrokers.removeTask(stopped.OwnerID, "studio_process_stopped")
					}
				}
				for _, stale := range commandBrokers.markStaleIfNeeded(*mcp2StaleAfter) {
					logger.Warn("mcp2 channel stale; clearing channel and killing matching managed Studio", "task_id", stale.TaskID, "studio_pid", stale.StudioPID)
					if stale.StudioPID > 0 {
						studioManager.KillManagedPID(stale.StudioPID)
					}
				}
				for _, taskID := range taskSessions.ExpireStale() {
					logger.Warn("task session lease expired", "task_id", taskID)
					commandBrokers.removeTask(taskID, "lease_expired")
					studioManager.KillTaskStudios(taskID)
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

func writeTaskStudioCommandTerminal(w http.ResponseWriter, taskID string, command mcp2Command, terminal mcp2CommandTerminal) {
	if terminal.Result == nil {
		writeTaskAPIError(w, http.StatusConflict, taskID, terminal.Reason, "mcp2 command ended before a response was received", "retry", map[string]any{
			"command_id": command.CommandID,
			"accepted":   false,
			"terminal":   terminal,
		})
		return
	}
	writeJSON(w, http.StatusOK, map[string]any{
		"ok":                  terminal.Result.OK,
		"task_id":             taskID,
		"command_id":          command.CommandID,
		"accepted":            terminal.Result.OK,
		"final_mode_verified": false,
		"next_action":         "poll_studio_mode",
		"command_result":      terminal.Result,
		"terminal":            terminal,
	})
}

func writeTaskStudioModeTerminal(w http.ResponseWriter, taskID string, command mcp2Command, terminal mcp2CommandTerminal) {
	if terminal.Result == nil {
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":         true,
			"task_id":    taskID,
			"available":  false,
			"mode":       "unknown",
			"command_id": command.CommandID,
			"reason":     terminal.Reason,
			"terminal":   terminal,
		})
		return
	}
	mode := terminal.Result.Mode
	if mode == "" && terminal.Result.Result != nil {
		if value, ok := terminal.Result.Result["mode"].(string); ok {
			mode = value
		}
	}
	if mode == "" {
		mode = "unknown"
	}
	response := map[string]any{
		"ok":             true,
		"task_id":        taskID,
		"available":      terminal.Result.OK,
		"mode":           mode,
		"mode_seq":       terminal.Result.ModeSeq,
		"command_id":     command.CommandID,
		"command_result": terminal.Result,
		"terminal":       terminal,
	}
	if terminal.Result.Result != nil {
		if runService, ok := terminal.Result.Result["run_service"]; ok {
			response["run_service"] = runService
		}
	}
	writeJSON(w, http.StatusOK, response)
}

func writeTaskAPIError(w http.ResponseWriter, status int, taskID string, code string, message string, action string, extra map[string]any) {
	body := map[string]any{
		"ok":      false,
		"code":    code,
		"message": message,
		"task_id": taskID,
	}
	if action != "" {
		body["action"] = action
	}
	for key, value := range extra {
		body[key] = value
	}
	writeJSON(w, status, body)
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

func resolvePeerManagedStudioProcess(r *http.Request, listenAddr string, studioManager *studio.Manager, logger *slog.Logger) (studio.ManagedProcess, bool) {
	helperPort, err := listenPort(listenAddr)
	if err != nil {
		logger.Warn("failed to parse helper listen port for peer pid lookup", "addr", listenAddr, "error", err)
		return studio.ManagedProcess{}, false
	}
	peerAddr, err := net.ResolveTCPAddr("tcp", r.RemoteAddr)
	if err != nil {
		logger.Warn("failed to parse mcp2 peer addr for pid lookup", "remote_addr", r.RemoteAddr, "error", err)
		return studio.ManagedProcess{}, false
	}
	pid, err := studio.ResolvePeerProcessID(peerAddr, helperPort)
	if err != nil {
		logger.Warn("failed to resolve mcp2 peer pid", "remote_addr", r.RemoteAddr, "error", err)
		return studio.ManagedProcess{}, false
	}
	managedProcess, ok := studioManager.ManagedProcessForPeerPID(pid)
	if !ok {
		if pid > 0 {
			logger.Warn("mcp2 peer pid is not a running managed Roblox Studio", "remote_addr", r.RemoteAddr, "pid", pid)
		}
		return studio.ManagedProcess{}, false
	}
	return managedProcess, true
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

func writeSessionError(w http.ResponseWriter, err error) {
	var sessionError *tasksession.Error
	if errors.As(err, &sessionError) {
		status := http.StatusConflict
		if sessionError.Code == tasksession.CodeTaskNotRegistered {
			status = http.StatusNotFound
		}
		writeJSON(w, status, map[string]any{
			"ok":      false,
			"code":    sessionError.Code,
			"message": sessionError.Message,
		})
		return
	}
	writeJSON(w, http.StatusBadRequest, map[string]any{
		"ok":      false,
		"code":    "bad_request",
		"message": err.Error(),
	})
}
