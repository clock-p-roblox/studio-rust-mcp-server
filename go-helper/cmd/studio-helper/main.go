package main

import (
	"bufio"
	"context"
	"crypto/tls"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"io"
	"log/slog"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/signal"
	"sort"
	"strconv"
	"strings"
	"sync"
	"syscall"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/runtimelog"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/screenshot"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/studio"
	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/tasksession"
)

const defaultAutoStartPlaceID = ""

const (
	taskStudioCommandQueuedTimeout   = 30 * time.Second
	taskStudioCommandResponseTimeout = 60 * time.Second
	taskStudioCommandWaitTimeout     = taskStudioCommandQueuedTimeout + taskStudioCommandResponseTimeout + 5*time.Second
)

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
	mcp2CommandStudioRunCode mcp2CommandKind = "studio_run_code"
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

type studioRunCodeCommandArgs struct {
	PlaceID string `json:"place_id"`
	Code    string `json:"code"`
}

type mcp2ChannelSummary struct {
	ActiveMode             string       `json:"active_mode"`
	ActiveModeSeq          *int64       `json:"active_mode_seq"`
	ActiveStudioPID        *int         `json:"active_studio_pid"`
	ExpectedNextMode       string       `json:"expected_next_mode,omitempty"`
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

type rojoPluginBinding struct {
	TaskID      string
	PlaceID     string
	UpstreamURL string
	StudioPID   int
}

type mcp2CommandBroker struct {
	mu                     sync.Mutex
	nextCommandID          int64
	activeMode             string
	activeModeSeq          *int64
	activeStudioPID        *int
	expectedNextMode       string
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
			b.startResponseTimeout(command.CommandID)
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

func (b *mcp2CommandBroker) enqueueStudioRunCode(placeID string, code string) (mcp2Command, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	if b.activeMode != "edit" {
		return mcp2Command{}, fmt.Errorf("studio_run_code requires edit mode; current mode=%s", b.activeMode)
	}
	return b.enqueueLocked(mcp2MessageTypeCommand, mcp2CommandStudioRunCode, studioRunCodeCommandArgs{
		PlaceID: placeID,
		Code:    code,
	})
}

func (b *mcp2CommandBroker) enqueue(commandType mcp2MessageType, commandKind mcp2CommandKind, args any) (mcp2Command, error) {
	b.mu.Lock()
	defer b.mu.Unlock()
	return b.enqueueLocked(commandType, commandKind, args)
}

func (b *mcp2CommandBroker) enqueueLocked(commandType mcp2MessageType, commandKind mcp2CommandKind, args any) (mcp2Command, error) {
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
	b.startQueuedTimeout(command.CommandID)
	return command, nil
}

func (b *mcp2CommandBroker) startQueuedTimeout(commandID int64) {
	go func() {
		timer := time.NewTimer(taskStudioCommandQueuedTimeout)
		defer timer.Stop()
		<-timer.C
		b.cancelPendingCommandWithReason(commandID, "queued_timeout")
	}()
}

func (b *mcp2CommandBroker) startResponseTimeout(commandID int64) {
	go func() {
		timer := time.NewTimer(taskStudioCommandResponseTimeout)
		defer timer.Stop()
		<-timer.C
		b.cancelWaitingResponseCommandWithReason(commandID, "response_timeout")
	}()
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

	waitingCommand := *b.waitingResponseCommand
	if nextMode := expectedNextModeForCommandResult(waitingCommand, result); nextMode != "" {
		b.expectedNextMode = nextMode
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

func (b *mcp2CommandBroker) cancelPendingCommandWithReason(commandID int64, reason string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()

	for _, command := range b.pending {
		if command.CommandID == commandID {
			return b.completeCommandLocked(commandID, reason, nil)
		}
	}
	return false
}

func (b *mcp2CommandBroker) cancelWaitingResponseCommandWithReason(commandID int64, reason string) bool {
	b.mu.Lock()
	defer b.mu.Unlock()

	if b.waitingResponseCommand == nil || b.waitingResponseCommand.CommandID != commandID {
		return false
	}
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
	b.expectedNextMode = ""
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
	b.expectedNextMode = ""
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
		ExpectedNextMode:       b.expectedNextMode,
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
	if !b.canAcceptLifecycleChangeLocked(mode) {
		return false
	}

	b.activeMode = mode
	seq := modeSeq
	b.activeModeSeq = &seq
	b.activeStudioPID = nil
	if b.expectedNextMode == mode {
		b.expectedNextMode = ""
	}
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

func (b *mcp2CommandBroker) canAcceptLifecycleChangeLocked(mode string) bool {
	if b.activeMode == "wait_init_mode" || b.activeModeSeq == nil {
		return true
	}
	if mode == b.activeMode {
		return true
	}
	if b.expectedNextMode != "" {
		return mode == b.expectedNextMode
	}
	if b.waitingResponseCommand == nil {
		return false
	}
	return mode == expectedNextModeForCommandKind(b.waitingResponseCommand.Kind)
}

func expectedNextModeForCommandResult(command mcp2Command, result mcp2ResponseResult) string {
	if !result.OK {
		return ""
	}
	status, _ := result.Result["status"].(string)
	switch command.Kind {
	case mcp2CommandStudioPlay:
		if status == "play_requested" {
			return "play_server"
		}
	case mcp2CommandStudioStop:
		if status == "stop_requested" {
			return "edit"
		}
	}
	return ""
}

func expectedNextModeForCommandKind(kind mcp2CommandKind) string {
	switch kind {
	case mcp2CommandStudioPlay:
		return "play_server"
	case mcp2CommandStudioStop:
		return "edit"
	default:
		return ""
	}
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
	registerDomain := flag.Bool("register-domain", true, "register helper2 public domain through embedded clockbridge")
	registerDomainDryRun := flag.Bool("register-domain-dry-run", false, "resolve helper2 public domain without starting embedded clockbridge")
	publicDomainSuffix := flag.String("public-domain-suffix", defaultPublicDomainSuffix, "domain suffix for helper2 public exposure")
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
	officialRunner := officialCLIProcessRunner{}
	runtimeLogs, err := runtimelog.NewStore(runtimelog.DefaultMaxEntriesPerTask)
	if err != nil {
		logger.Error("failed to create runtime log store", "error", err)
		os.Exit(1)
	}
	effectiveListenAddr := *addr
	publicExposure, err := newPublicExposureManager(publicExposureConfig{Enabled: false}, logger)
	if err != nil {
		logger.Error("failed to initialize disabled public exposure", "error", err)
		os.Exit(1)
	}
	defer func() {
		publicExposure.Stop()
		if err := runtimeLogs.Close(); err != nil {
			logger.Warn("failed to clean runtime log store", "dir", runtimeLogs.Dir(), "error", err)
		}
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
	registerMCPHandlers(mux, taskSessions, studioManager, commandBrokers, runtimeLogs, officialRunner, logger, func() publicExposureStatus {
		return publicExposure.Status()
	})
	registerOfficialHTTPHandlers(mux, taskSessions, studioManager, officialRunner)
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
	mux.HandleFunc("GET /session/{task_id}/runtime-log", func(w http.ResponseWriter, r *http.Request) {
		taskID := r.PathValue("task_id")
		status := taskSessions.Status(taskID)
		if !status.OK || status.Contract == nil {
			writeJSON(w, http.StatusNotFound, status)
			return
		}
		limit, err := parseRuntimeLogLimit(r.URL.Query().Get("limit"))
		if err != nil {
			writeRuntimeLogAPIError(w, http.StatusBadRequest, "bad_limit", err.Error(), map[string]any{"task_id": taskID})
			return
		}
		entries, nextCursor, err := runtimeLogs.Read(taskID, r.URL.Query().Get("cursor"), limit)
		if err != nil {
			writeRuntimeLogAPIError(w, http.StatusBadRequest, "bad_cursor", err.Error(), map[string]any{"task_id": taskID})
			return
		}
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":          true,
			"task_id":     taskID,
			"entries":     entries,
			"next_cursor": nextCursor,
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
		waitCtx, cancel := context.WithTimeout(r.Context(), taskStudioCommandWaitTimeout)
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
		waitCtx, cancel := context.WithTimeout(r.Context(), taskStudioCommandWaitTimeout)
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
	mux.HandleFunc("POST /session/{task_id}/studio/run-code-direct", func(w http.ResponseWriter, r *http.Request) {
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
		var request struct {
			Code string `json:"code"`
		}
		if err := json.NewDecoder(r.Body).Decode(&request); err != nil {
			writeTaskAPIError(w, http.StatusBadRequest, taskID, "bad_request", err.Error(), "fix_request", nil)
			return
		}
		if strings.TrimSpace(request.Code) == "" {
			writeTaskAPIError(w, http.StatusBadRequest, taskID, "missing_code", "code is required", "fix_request", nil)
			return
		}
		if _, err := studioManager.ManagedProcessForTask(taskID); err != nil {
			writeTaskAPIError(w, http.StatusConflict, taskID, "studio_not_available", err.Error(), "wait_for_studio", nil)
			return
		}
		broker := commandBrokers.forTask(taskID)
		command, err := broker.enqueueStudioRunCode(status.Contract.PlaceID, request.Code)
		if err != nil {
			writeTaskAPIError(w, http.StatusConflict, taskID, "studio_not_edit", err.Error(), "ensure_edit", nil)
			return
		}
		waitCtx, cancel := context.WithTimeout(r.Context(), taskStudioCommandWaitTimeout)
		defer cancel()
		terminal, found := broker.waitForTerminal(waitCtx, command.CommandID)
		if !found {
			broker.cancelCommandWithReason(command.CommandID, "command_timeout")
			writeTaskAPIError(w, http.StatusGatewayTimeout, taskID, "command_timeout", "mcp2 command did not complete before timeout", "retry", map[string]any{"command_id": command.CommandID})
			return
		}
		writeTaskStudioRunCodeTerminal(w, taskID, command, terminal)
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
	mux.HandleFunc("GET /v1/rojo/config", func(w http.ResponseWriter, r *http.Request) {
		requestedPlaceID := strings.TrimSpace(r.URL.Query().Get("place_id"))
		requestedTaskID := strings.TrimSpace(r.URL.Query().Get("task_id"))
		binding, ok := resolveRojoPluginBinding(w, r, effectiveListenAddr, studioManager, taskSessions, logger, requestedPlaceID, requestedTaskID)
		if !ok {
			return
		}
		baseURL, err := localRojoForwardBaseURL(effectiveListenAddr, r.Host, binding.PlaceID, binding.TaskID)
		if err != nil {
			writeRojoAPIError(w, http.StatusInternalServerError, "helper_addr_unavailable", err.Error(), map[string]any{"task_id": binding.TaskID})
			return
		}
		logger.Info("resolved task-bound Rojo config", "task_id", binding.TaskID, "place_id", binding.PlaceID, "studio_pid", binding.StudioPID, "base_url", baseURL)
		writeJSON(w, http.StatusOK, map[string]any{
			"ok":          true,
			"place_id":    binding.PlaceID,
			"task_id":     binding.TaskID,
			"base_url":    baseURL,
			"auth_header": "",
		})
	})
	mux.HandleFunc("GET /rojo-forward/{place_id}/task/{task_id}/api/socket/{cursor}", func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("place_id")
		taskID := r.PathValue("task_id")
		cursor := r.PathValue("cursor")
		binding, ok := resolveRojoPluginBinding(w, r, effectiveListenAddr, studioManager, taskSessions, logger, placeID, taskID)
		if !ok {
			return
		}
		targetURL, err := rojoForwardTargetWSURL(binding.UpstreamURL, "api/socket/"+cursor, r.URL.RawQuery)
		if err != nil {
			writeRojoAPIError(w, http.StatusBadGateway, "invalid_rojo_upstream", err.Error(), map[string]any{"task_id": binding.TaskID})
			return
		}
		logger.Info("proxying task-bound Rojo websocket", "task_id", binding.TaskID, "place_id", binding.PlaceID, "studio_pid", binding.StudioPID, "target", targetURL)
		proxyRojoWebSocket(w, r, targetURL)
	})
	rojoForwardHTTP := func(w http.ResponseWriter, r *http.Request) {
		placeID := r.PathValue("place_id")
		taskID := r.PathValue("task_id")
		path := r.PathValue("path")
		binding, ok := resolveRojoPluginBinding(w, r, effectiveListenAddr, studioManager, taskSessions, logger, placeID, taskID)
		if !ok {
			return
		}
		targetURL, err := rojoForwardTargetHTTPURL(binding.UpstreamURL, path, r.URL.RawQuery)
		if err != nil {
			writeRojoAPIError(w, http.StatusBadGateway, "invalid_rojo_upstream", err.Error(), map[string]any{"task_id": binding.TaskID})
			return
		}
		status, bytes, err := proxyRojoHTTPRequest(r.Context(), w, r, targetURL)
		if err != nil {
			logger.Warn("failed to proxy task-bound Rojo HTTP request", "task_id", binding.TaskID, "place_id", binding.PlaceID, "target", targetURL, "error", err)
			writeRojoAPIError(w, http.StatusBadGateway, "rojo_proxy_failed", err.Error(), map[string]any{"task_id": binding.TaskID})
			return
		}
		logger.Info("proxied task-bound Rojo HTTP request", "task_id", binding.TaskID, "place_id", binding.PlaceID, "method", r.Method, "target", targetURL, "status", status, "bytes", bytes)
	}
	mux.HandleFunc("/rojo-forward/{place_id}/task/{task_id}", rojoForwardHTTP)
	mux.HandleFunc("/rojo-forward/{place_id}/task/{task_id}/{path...}", rojoForwardHTTP)
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
		if managedStudio.OwnerKind != "task" || managedStudio.OwnerID == "" {
			writeJSON(w, http.StatusForbidden, map[string]any{
				"ok":     false,
				"error":  "task_binding_required",
				"reason": "mcp2 traffic must come from a task-owned Roblox Studio process",
			})
			return
		}
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
		studioPID := managedStudio.PID
		broker := commandBrokers.forTask(managedStudio.OwnerID)
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
				ignoredReason = "task_binding_required"
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
	effectiveListenAddr = listener.Addr().String()
	publicExposure, err = newPublicExposureManager(publicExposureConfig{
		Enabled:      *registerDomain,
		DryRun:       *registerDomainDryRun,
		DomainSuffix: *publicDomainSuffix,
		ListenAddr:   effectiveListenAddr,
	}, logger)
	if err != nil {
		logger.Error("failed to configure helper2 public exposure", "error", err)
		_ = listener.Close()
		os.Exit(1)
	}

	errCh := make(chan error, 1)
	rootCtx, stopPublicExposure := context.WithCancel(context.Background())
	defer stopPublicExposure()
	if err := publicExposure.Start(rootCtx); err != nil {
		logger.Error("failed to start helper2 public exposure", "error", err)
		_ = listener.Close()
		os.Exit(1)
	}
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

	ctx, cancel := context.WithTimeout(context.Background(), 15*time.Second)
	defer cancel()

	stopPublicExposure()
	publicExposure.Stop()
	if err := server.Shutdown(ctx); err != nil {
		logger.Error("graceful shutdown failed", "error", err)
		_ = server.Close()
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

func parseRuntimeLogLimit(value string) (int, error) {
	value = strings.TrimSpace(value)
	if value == "" {
		return runtimelog.DefaultReadLimit, nil
	}
	limit, err := strconv.Atoi(value)
	if err != nil || limit <= 0 {
		return 0, fmt.Errorf("limit must be a positive integer: %s", value)
	}
	if limit > runtimelog.MaxReadLimit {
		return runtimelog.MaxReadLimit, nil
	}
	return limit, nil
}

func resolveRojoPluginBinding(
	w http.ResponseWriter,
	r *http.Request,
	listenAddr string,
	studioManager *studio.Manager,
	taskSessions *tasksession.Registry,
	logger *slog.Logger,
	requestedPlaceID string,
	requestedTaskID string,
) (rojoPluginBinding, bool) {
	managedStudio, ok := resolvePeerManagedStudioProcess(r, listenAddr, studioManager, logger)
	if !ok {
		writeRojoAPIError(w, http.StatusForbidden, "invalid_plugin_binding", "request is not from a helper-managed Roblox Studio process", nil)
		return rojoPluginBinding{}, false
	}
	return resolveRojoPluginBindingForManagedStudio(w, managedStudio, taskSessions, requestedPlaceID, requestedTaskID)
}

func resolveRojoPluginBindingForManagedStudio(
	w http.ResponseWriter,
	managedStudio studio.ManagedProcess,
	taskSessions *tasksession.Registry,
	requestedPlaceID string,
	requestedTaskID string,
) (rojoPluginBinding, bool) {
	if managedStudio.OwnerKind != "task" || managedStudio.OwnerID == "" {
		writeRojoAPIError(w, http.StatusForbidden, "task_binding_required", "managed Studio is not bound to a task session", map[string]any{"studio_pid": managedStudio.PID})
		return rojoPluginBinding{}, false
	}
	status := taskSessions.Status(managedStudio.OwnerID)
	if !status.OK || status.State != "live" || status.Contract == nil {
		writeRojoAPIError(w, http.StatusForbidden, "task_not_live", "managed Studio is not bound to a live task session", map[string]any{
			"task_id":    managedStudio.OwnerID,
			"state":      status.State,
			"studio_pid": managedStudio.PID,
		})
		return rojoPluginBinding{}, false
	}
	if requestedTaskID != "" && requestedTaskID != status.Contract.TaskID {
		writeRojoAPIError(w, http.StatusForbidden, "task_binding_mismatch", "requested task does not match the Studio task binding", map[string]any{
			"requested_task_id": requestedTaskID,
			"bound_task_id":     status.Contract.TaskID,
			"studio_pid":        managedStudio.PID,
		})
		return rojoPluginBinding{}, false
	}
	if requestedPlaceID != "" && requestedPlaceID != status.Contract.PlaceID {
		writeRojoAPIError(w, http.StatusForbidden, "place_binding_mismatch", "requested place does not match the Studio task binding", map[string]any{
			"requested_place_id": requestedPlaceID,
			"bound_place_id":     status.Contract.PlaceID,
			"task_id":            status.Contract.TaskID,
			"studio_pid":         managedStudio.PID,
		})
		return rojoPluginBinding{}, false
	}
	if strings.TrimSpace(status.Contract.RojoUpstreamURL) == "" {
		writeRojoAPIError(w, http.StatusConflict, "rojo_upstream_unavailable", "task session has no Rojo upstream URL", map[string]any{"task_id": status.Contract.TaskID})
		return rojoPluginBinding{}, false
	}
	return rojoPluginBinding{
		TaskID:      status.Contract.TaskID,
		PlaceID:     status.Contract.PlaceID,
		UpstreamURL: status.Contract.RojoUpstreamURL,
		StudioPID:   managedStudio.PID,
	}, true
}

func localRojoForwardBaseURL(listenAddr string, requestHost string, placeID string, taskID string) (string, error) {
	port, err := listenPort(listenAddr)
	if err != nil && requestHost != "" {
		_, portText, splitErr := net.SplitHostPort(requestHost)
		if splitErr == nil {
			port, err = strconv.Atoi(portText)
		}
	}
	if err != nil || port <= 0 {
		return "", fmt.Errorf("could not resolve helper listen port from %q", listenAddr)
	}
	return fmt.Sprintf("http://127.0.0.1:%d/rojo-forward/%s/task/%s", port, url.PathEscape(placeID), url.PathEscape(taskID)), nil
}

func rojoForwardTargetHTTPURL(baseURL string, path string, rawQuery string) (string, error) {
	return rojoForwardTargetURL(baseURL, "http", "https", path, rawQuery)
}

func rojoForwardTargetWSURL(baseURL string, path string, rawQuery string) (string, error) {
	parsed, err := url.Parse(strings.TrimSpace(baseURL))
	if err != nil {
		return "", err
	}
	switch parsed.Scheme {
	case "http":
		parsed.Scheme = "ws"
	case "https":
		parsed.Scheme = "wss"
	case "ws", "wss":
	default:
		return "", fmt.Errorf("Rojo upstream URL must use http(s) or ws(s): %s", baseURL)
	}
	parsed.Path = rojoForwardJoinedPath(parsed.Path, path)
	parsed.RawQuery = rawQuery
	return parsed.String(), nil
}

func rojoForwardTargetURL(baseURL string, allowedSchemeA string, allowedSchemeB string, path string, rawQuery string) (string, error) {
	parsed, err := url.Parse(strings.TrimSpace(baseURL))
	if err != nil {
		return "", err
	}
	if parsed.Scheme != allowedSchemeA && parsed.Scheme != allowedSchemeB {
		return "", fmt.Errorf("Rojo upstream URL must use %s or %s: %s", allowedSchemeA, allowedSchemeB, baseURL)
	}
	if parsed.Host == "" {
		return "", fmt.Errorf("Rojo upstream URL must include host: %s", baseURL)
	}
	parsed.Path = rojoForwardJoinedPath(parsed.Path, path)
	parsed.RawQuery = rawQuery
	return parsed.String(), nil
}

func rojoForwardJoinedPath(basePath string, path string) string {
	basePath = strings.TrimRight(basePath, "/")
	path = strings.TrimLeft(path, "/")
	if path == "" {
		if basePath == "" {
			return "/"
		}
		return basePath + "/"
	}
	if basePath == "" {
		return "/" + path
	}
	return basePath + "/" + path
}

func proxyRojoHTTPRequest(ctx context.Context, w http.ResponseWriter, r *http.Request, targetURL string) (int, int64, error) {
	var body io.Reader
	if r.Body != nil {
		body = r.Body
	}
	req, err := http.NewRequestWithContext(ctx, r.Method, targetURL, body)
	if err != nil {
		return 0, 0, err
	}
	copyRojoForwardRequestHeaders(req.Header, r.Header)
	req.Header.Set("Accept-Encoding", "identity")
	response, err := http.DefaultClient.Do(req)
	if err != nil {
		return 0, 0, err
	}
	defer response.Body.Close()

	copyRojoForwardResponseHeaders(w.Header(), response.Header)
	w.WriteHeader(response.StatusCode)
	written, err := io.Copy(w, response.Body)
	if err != nil {
		return response.StatusCode, written, err
	}
	return response.StatusCode, written, nil
}

func copyRojoForwardRequestHeaders(dst http.Header, src http.Header) {
	for key, values := range src {
		if rojoHopByHopHeader(key) || strings.EqualFold(key, "Host") || strings.EqualFold(key, "Accept-Encoding") {
			continue
		}
		for _, value := range values {
			dst.Add(key, value)
		}
	}
}

func copyRojoForwardResponseHeaders(dst http.Header, src http.Header) {
	for key, values := range src {
		if rojoHopByHopHeader(key) {
			continue
		}
		for _, value := range values {
			dst.Add(key, value)
		}
	}
}

func rojoHopByHopHeader(key string) bool {
	switch strings.ToLower(key) {
	case "connection", "keep-alive", "proxy-authenticate", "proxy-authorization", "te", "trailer", "transfer-encoding", "upgrade":
		return true
	default:
		return false
	}
}

func proxyRojoWebSocket(w http.ResponseWriter, r *http.Request, targetURL string) {
	if !headerContainsToken(r.Header, "Connection", "upgrade") || !strings.EqualFold(r.Header.Get("Upgrade"), "websocket") {
		writeRojoAPIError(w, http.StatusBadRequest, "websocket_upgrade_required", "Rojo socket endpoint requires a WebSocket upgrade request", nil)
		return
	}
	target, err := url.Parse(targetURL)
	if err != nil {
		writeRojoAPIError(w, http.StatusBadGateway, "invalid_rojo_upstream", err.Error(), nil)
		return
	}
	hijacker, ok := w.(http.Hijacker)
	if !ok {
		writeRojoAPIError(w, http.StatusInternalServerError, "websocket_hijack_unavailable", "HTTP server does not support connection hijacking", nil)
		return
	}
	clientConn, clientRW, err := hijacker.Hijack()
	if err != nil {
		return
	}
	defer clientConn.Close()

	remoteConn, err := dialRojoWebSocketTarget(r.Context(), target)
	if err != nil {
		writeRawHTTPError(clientRW.Writer, http.StatusBadGateway, "failed to connect Rojo websocket upstream")
		return
	}
	defer remoteConn.Close()

	if err := writeRojoWebSocketHandshake(remoteConn, r, target); err != nil {
		writeRawHTTPError(clientRW.Writer, http.StatusBadGateway, "failed to write Rojo websocket handshake")
		return
	}
	remoteReader := bufio.NewReader(remoteConn)
	response, err := http.ReadResponse(remoteReader, r)
	if err != nil {
		writeRawHTTPError(clientRW.Writer, http.StatusBadGateway, "failed to read Rojo websocket handshake")
		return
	}
	defer response.Body.Close()
	if response.StatusCode != http.StatusSwitchingProtocols {
		if err := response.Write(clientRW.Writer); err == nil {
			_ = clientRW.Writer.Flush()
		}
		return
	}
	if err := response.Write(clientRW.Writer); err != nil {
		return
	}
	if err := clientRW.Writer.Flush(); err != nil {
		return
	}

	done := make(chan struct{}, 2)
	go func() {
		_, _ = io.Copy(remoteConn, clientRW)
		done <- struct{}{}
	}()
	go func() {
		_, _ = io.Copy(clientConn, io.MultiReader(remoteReader, remoteConn))
		done <- struct{}{}
	}()
	<-done
}

func dialRojoWebSocketTarget(ctx context.Context, target *url.URL) (net.Conn, error) {
	host := target.Host
	if _, _, err := net.SplitHostPort(host); err != nil {
		switch target.Scheme {
		case "ws":
			host = net.JoinHostPort(host, "80")
		case "wss":
			host = net.JoinHostPort(host, "443")
		}
	}
	dialer := &net.Dialer{Timeout: 10 * time.Second}
	if target.Scheme == "wss" {
		return tls.DialWithDialer(dialer, "tcp", host, &tls.Config{ServerName: target.Hostname(), MinVersion: tls.VersionTLS12})
	}
	return dialer.DialContext(ctx, "tcp", host)
}

func writeRojoWebSocketHandshake(conn net.Conn, original *http.Request, target *url.URL) error {
	path := target.EscapedPath()
	if path == "" {
		path = "/"
	}
	if target.RawQuery != "" {
		path += "?" + target.RawQuery
	}
	request := fmt.Sprintf("GET %s HTTP/1.1\r\nHost: %s\r\nConnection: Upgrade\r\nUpgrade: websocket\r\n", path, target.Host)
	for _, key := range []string{"Sec-WebSocket-Key", "Sec-WebSocket-Version", "Sec-WebSocket-Protocol", "Sec-WebSocket-Extensions", "Origin"} {
		for _, value := range original.Header.Values(key) {
			request += key + ": " + value + "\r\n"
		}
	}
	request += "\r\n"
	_, err := io.WriteString(conn, request)
	return err
}

func headerContainsToken(header http.Header, key string, token string) bool {
	for _, value := range header.Values(key) {
		for _, part := range strings.Split(value, ",") {
			if strings.EqualFold(strings.TrimSpace(part), token) {
				return true
			}
		}
	}
	return false
}

func writeRawHTTPError(writer *bufio.Writer, status int, message string) {
	body := fmt.Sprintf("{\"ok\":false,\"code\":\"rojo_websocket_proxy_failed\",\"message\":%q}\n", message)
	_, _ = fmt.Fprintf(writer, "HTTP/1.1 %d %s\r\nContent-Type: application/json\r\nContent-Length: %d\r\nConnection: close\r\n\r\n%s", status, http.StatusText(status), len(body), body)
	_ = writer.Flush()
}

func writeRojoAPIError(w http.ResponseWriter, status int, code string, message string, extra map[string]any) {
	body := map[string]any{
		"ok":      false,
		"code":    code,
		"message": message,
	}
	for key, value := range extra {
		body[key] = value
	}
	writeJSON(w, status, body)
}

func writeRuntimeLogAPIError(w http.ResponseWriter, status int, code string, message string, extra map[string]any) {
	body := map[string]any{
		"ok":      false,
		"code":    code,
		"message": message,
	}
	for key, value := range extra {
		body[key] = value
	}
	writeJSON(w, status, body)
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

func writeTaskStudioRunCodeTerminal(w http.ResponseWriter, taskID string, command mcp2Command, terminal mcp2CommandTerminal) {
	payload, ok := taskStudioRunCodeTerminalPayload(taskID, command, terminal)
	if !ok {
		message := "mcp2 command ended before a successful response"
		code := terminal.Reason
		if terminal.Result != nil && terminal.Result.Error != "" {
			message = terminal.Result.Error
			code = "helper_command_failed"
		}
		writeTaskAPIError(w, http.StatusConflict, taskID, code, message, "retry", map[string]any{
			"command_id":     command.CommandID,
			"terminal":       terminal,
			"command_result": payload["command_result"],
		})
		return
	}
	writeJSON(w, http.StatusOK, payload)
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
		} else if runService, ok := terminal.Result.Result["run_service_flags"]; ok {
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
