package studio

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"regexp"
	"sort"
	"sync"
	"time"
)

var placeIDPattern = regexp.MustCompile(`^[0-9]+$`)

const (
	LaunchSourceInternal = "internal"
	LaunchSourceDebug    = "debug"
	LaunchSourceTask     = "task"
)

type Manager struct {
	mu        sync.Mutex
	job       *processJob
	logger    *slog.Logger
	processes map[int]ManagedProcess
	desired   map[string]DesiredStudio
	starting  map[string]bool
	canceled  map[string]bool
}

type ManagedProcess struct {
	PlaceID    string    `json:"place_id"`
	UniverseID string    `json:"universe_id"`
	StudioPath string    `json:"studio_path"`
	Source     string    `json:"source"`
	OwnerKind  string    `json:"owner_kind,omitempty"`
	OwnerID    string    `json:"owner_id,omitempty"`
	PID        int       `json:"pid"`
	StartedAt  time.Time `json:"started_at"`
	Running    bool      `json:"running"`
}

type LaunchResult struct {
	PlaceID    string   `json:"place_id"`
	UniverseID string   `json:"universe_id"`
	StudioPath string   `json:"studio_path"`
	Args       []string `json:"args"`
	PID        int      `json:"pid"`
	Source     string   `json:"source"`
}

type Summary struct {
	Count     int              `json:"count"`
	Studios   []ManagedProcess `json:"studios"`
	Desired   []DesiredStudio  `json:"desired"`
	UpdatedAt time.Time        `json:"updated_at"`
}

type DesiredStudio struct {
	PlaceID   string `json:"place_id"`
	Source    string `json:"source"`
	OwnerKind string `json:"owner_kind,omitempty"`
	OwnerID   string `json:"owner_id,omitempty"`
}

func NewManager(logger *slog.Logger) (*Manager, error) {
	job, err := newProcessJob()
	if err != nil {
		return nil, err
	}
	if logger == nil {
		logger = slog.Default()
	}
	return &Manager{
		job:       job,
		logger:    logger,
		processes: make(map[int]ManagedProcess),
		desired:   make(map[string]DesiredStudio),
		starting:  make(map[string]bool),
		canceled:  make(map[string]bool),
	}, nil
}

func (m *Manager) StartInternal(ctx context.Context, placeID string) (LaunchResult, error) {
	return m.start(ctx, placeID, LaunchSourceInternal, true)
}

func (m *Manager) StartDebug(ctx context.Context, placeID string) (LaunchResult, error) {
	return m.start(ctx, placeID, LaunchSourceDebug, true)
}

func PlaceIDIsValid(placeID string) bool {
	return placeIDPattern.MatchString(placeID)
}

func (m *Manager) Close() error {
	m.mu.Lock()
	count := len(m.processes)
	m.processes = make(map[int]ManagedProcess)
	m.mu.Unlock()
	if count > 0 {
		m.logger.Info("closing Studio manager; helper-started Studio processes will be terminated", "count", count)
	}
	return m.job.close()
}

func (m *Manager) Summary() Summary {
	m.mu.Lock()
	defer m.mu.Unlock()
	m.pruneStoppedProcessesLocked()

	studios := make([]ManagedProcess, 0, len(m.processes))
	for pid, process := range m.processes {
		process.Running = processIsManagedRunning(pid, process.StartedAt)
		studios = append(studios, process)
	}
	desired := make([]DesiredStudio, 0, len(m.desired))
	for _, target := range m.desired {
		desired = append(desired, target)
	}
	sort.Slice(studios, func(i, j int) bool {
		return studios[i].PID < studios[j].PID
	})
	sort.Slice(desired, func(i, j int) bool {
		return desired[i].PlaceID < desired[j].PlaceID
	})
	return Summary{
		Count:     len(studios),
		Studios:   studios,
		Desired:   desired,
		UpdatedAt: time.Now(),
	}
}

func (m *Manager) ReconcileDesired(ctx context.Context) {
	m.mu.Lock()
	m.pruneStoppedProcessesLocked()
	desired := make([]DesiredStudio, 0, len(m.desired))
	for _, target := range m.desired {
		if m.starting[desiredKey(target)] {
			continue
		}
		hasRunning := false
		for pid, process := range m.processes {
			if managedProcessMatchesDesired(process, target) && processIsManagedRunning(pid, process.StartedAt) {
				hasRunning = true
				break
			}
		}
		if !hasRunning {
			desired = append(desired, target)
		}
	}
	m.mu.Unlock()

	for _, target := range desired {
		result, err := m.startDesired(ctx, target, false)
		if err != nil {
			m.logger.Error("failed to reconcile desired Roblox Studio", "place_id", target.PlaceID, "source", target.Source, "error", err)
			continue
		}
		m.logger.Info("restarted desired Roblox Studio", "place_id", result.PlaceID, "source", result.Source, "pid", result.PID)
	}
}

func (m *Manager) EnsureTaskDesired(taskID string, placeID string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	target := DesiredStudio{
		PlaceID:   placeID,
		Source:    LaunchSourceTask,
		OwnerKind: "task",
		OwnerID:   taskID,
	}
	m.desired[desiredKey(target)] = target
	delete(m.canceled, desiredKey(target))
}

func (m *Manager) RemoveTaskDesired(taskID string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	for key, target := range m.desired {
		if target.OwnerKind == "task" && target.OwnerID == taskID {
			delete(m.desired, key)
			if m.starting[key] {
				m.canceled[key] = true
			}
		}
	}
}

func (m *Manager) KillTaskStudios(taskID string) []int {
	m.mu.Lock()
	m.pruneStoppedProcessesLocked()
	matches := make([]ManagedProcess, 0)
	for pid, process := range m.processes {
		if process.OwnerKind == "task" && process.OwnerID == taskID && processIsManagedRunning(pid, process.StartedAt) {
			process.PID = pid
			matches = append(matches, process)
			delete(m.processes, pid)
		}
	}
	m.mu.Unlock()

	killed := make([]int, 0, len(matches))
	for _, process := range matches {
		if err := killProcess(process.PID); err != nil {
			m.logger.Warn("failed to kill managed Roblox Studio", "pid", process.PID, "place_id", process.PlaceID, "error", err)
			continue
		}
		m.logger.Info("killed managed Roblox Studio", "pid", process.PID, "place_id", process.PlaceID)
		if !processIsRunning(process.PID) {
			killed = append(killed, process.PID)
		}
	}
	return killed
}

func (m *Manager) KillManagedPID(pid int) bool {
	m.mu.Lock()
	m.pruneStoppedProcessesLocked()
	process, ok := m.processes[pid]
	m.mu.Unlock()
	if !ok {
		return false
	}
	if err := killProcess(pid); err != nil {
		m.logger.Warn("failed to kill managed Roblox Studio", "pid", pid, "place_id", process.PlaceID, "error", err)
		return false
	}
	m.mu.Lock()
	delete(m.processes, pid)
	m.mu.Unlock()
	m.logger.Info("killed managed Roblox Studio", "pid", pid, "place_id", process.PlaceID)
	return true
}

func (m *Manager) KillManagedProcess(expected ManagedProcess) bool {
	if expected.PID <= 0 {
		return false
	}
	m.mu.Lock()
	m.pruneStoppedProcessesLocked()
	current, ok := m.processes[expected.PID]
	if !ok ||
		!current.StartedAt.Equal(expected.StartedAt) ||
		current.OwnerKind != expected.OwnerKind ||
		current.OwnerID != expected.OwnerID {
		m.mu.Unlock()
		return false
	}
	m.mu.Unlock()

	if err := killProcess(expected.PID); err != nil {
		m.logger.Warn("failed to kill managed Roblox Studio", "pid", expected.PID, "place_id", expected.PlaceID, "error", err)
		return false
	}
	m.mu.Lock()
	current, ok = m.processes[expected.PID]
	if ok && current.StartedAt.Equal(expected.StartedAt) && current.OwnerKind == expected.OwnerKind && current.OwnerID == expected.OwnerID {
		delete(m.processes, expected.PID)
	}
	m.mu.Unlock()
	m.logger.Info("killed managed Roblox Studio", "pid", expected.PID, "place_id", expected.PlaceID)
	return true
}

func (m *Manager) ManagedPIDForPlace(placeID string) (int, error) {
	if !placeIDPattern.MatchString(placeID) {
		return 0, errors.New("placeid must contain digits only")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.pruneStoppedProcessesLocked()

	matches := make([]int, 0, 1)
	for pid, process := range m.processes {
		if process.PlaceID == placeID && processIsManagedRunning(pid, process.StartedAt) {
			matches = append(matches, pid)
		}
	}
	if len(matches) == 0 {
		return 0, fmt.Errorf("no running managed Roblox Studio for placeId %s", placeID)
	}
	if len(matches) > 1 {
		sort.Ints(matches)
		return 0, fmt.Errorf("multiple running managed Roblox Studio processes for placeId %s: %v", placeID, matches)
	}
	return matches[0], nil
}

func (m *Manager) ManagedProcessForTask(taskID string) (ManagedProcess, error) {
	if taskID == "" {
		return ManagedProcess{}, errors.New("task_id must not be empty")
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.pruneStoppedProcessesLocked()

	matches := make([]ManagedProcess, 0, 1)
	for pid, process := range m.processes {
		if process.OwnerKind == "task" && process.OwnerID == taskID && processIsManagedRunning(pid, process.StartedAt) {
			process.PID = pid
			process.Running = true
			matches = append(matches, process)
		}
	}
	if len(matches) == 0 {
		return ManagedProcess{}, fmt.Errorf("no running task-owned Roblox Studio for task_id %s", taskID)
	}
	if len(matches) > 1 {
		sort.Slice(matches, func(i, j int) bool {
			return matches[i].PID < matches[j].PID
		})
		pids := make([]int, 0, len(matches))
		for _, match := range matches {
			pids = append(pids, match.PID)
		}
		return ManagedProcess{}, fmt.Errorf("multiple running task-owned Roblox Studio processes for task_id %s: %v", taskID, pids)
	}
	return matches[0], nil
}

func (m *Manager) PIDIsManagedRunning(pid int) bool {
	_, ok := m.ManagedRootPIDForPeerPID(pid)
	return ok
}

func (m *Manager) ManagedRootPIDForPeerPID(pid int) (int, bool) {
	process, ok := m.ManagedProcessForPeerPID(pid)
	return process.PID, ok
}

func (m *Manager) ManagedProcessForPeerPID(pid int) (ManagedProcess, bool) {
	if pid <= 0 {
		return ManagedProcess{}, false
	}
	m.mu.Lock()
	defer m.mu.Unlock()
	m.pruneStoppedProcessesLocked()

	roots := make([]ManagedProcess, 0, len(m.processes))
	for rootPID, process := range m.processes {
		if !processIsManagedRunning(rootPID, process.StartedAt) {
			continue
		}
		process.PID = rootPID
		process.Running = true
		if rootPID == pid {
			return process, true
		}
		roots = append(roots, process)
	}

	if !m.job.contains(pid) {
		return ManagedProcess{}, false
	}
	for _, root := range roots {
		if processHasAncestor(pid, root.PID) {
			return root, true
		}
	}
	return ManagedProcess{}, false
}

func (m *Manager) PruneStoppedProcesses() []ManagedProcess {
	m.mu.Lock()
	defer m.mu.Unlock()
	return m.pruneStoppedProcessesLocked()
}

func (m *Manager) pruneStoppedProcessesLocked() []ManagedProcess {
	stopped := make([]ManagedProcess, 0)
	for pid, process := range m.processes {
		if processIsManagedRunning(pid, process.StartedAt) {
			continue
		}
		process.PID = pid
		process.Running = false
		stopped = append(stopped, process)
		delete(m.processes, pid)
		m.logger.Info("removed stopped Roblox Studio from manager", "pid", pid, "place_id", process.PlaceID)
	}
	return stopped
}

func processHasAncestor(pid int, ancestorPID int) bool {
	if pid <= 0 || ancestorPID <= 0 || pid == ancestorPID {
		return false
	}
	seen := map[int]bool{}
	current := pid
	for i := 0; i < 64; i++ {
		parent, ok := processParentID(current)
		if !ok || parent <= 0 || seen[parent] {
			return false
		}
		if parent == ancestorPID {
			return true
		}
		seen[parent] = true
		current = parent
	}
	return false
}

func (m *Manager) start(ctx context.Context, placeID string, source string, markDesired bool) (LaunchResult, error) {
	target := DesiredStudio{
		PlaceID: placeID,
		Source:  source,
	}
	return m.startDesired(ctx, target, markDesired)
}

func (m *Manager) startDesired(ctx context.Context, target DesiredStudio, markDesired bool) (LaunchResult, error) {
	placeID := target.PlaceID
	if !placeIDPattern.MatchString(placeID) {
		return LaunchResult{}, errors.New("placeid must contain digits only")
	}
	if markDesired {
		m.mu.Lock()
		m.desired[desiredKey(target)] = target
		m.mu.Unlock()
	}
	key := desiredKey(target)
	m.mu.Lock()
	if m.starting[key] {
		m.mu.Unlock()
		return LaunchResult{}, fmt.Errorf("Roblox Studio launch already in progress for placeId %s", placeID)
	}
	m.starting[key] = true
	delete(m.canceled, key)
	m.mu.Unlock()
	defer func() {
		m.mu.Lock()
		delete(m.starting, key)
		delete(m.canceled, key)
		m.mu.Unlock()
	}()

	studioPath, err := ResolveStudioPath()
	if err != nil {
		return LaunchResult{}, err
	}

	universeID, err := ResolveUniverseID(ctx, placeID)
	if err != nil {
		return LaunchResult{}, err
	}

	args := []string{
		"-task",
		"EditPlace",
		"-placeId",
		placeID,
		"-universeId",
		universeID,
	}

	cmd := exec.Command(studioPath, args...)
	if parent := filepath.Dir(studioPath); parent != "" {
		cmd.Dir = parent
	}
	if err := cmd.Start(); err != nil {
		return LaunchResult{}, fmt.Errorf("failed to launch Roblox Studio at %s: %w", studioPath, err)
	}
	if err := m.job.assign(cmd); err != nil {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
		return LaunchResult{}, err
	}
	pid := cmd.Process.Pid
	startedAt := time.Now()
	if processStartedAt, ok := processStartTime(pid); ok {
		startedAt = processStartedAt
	}
	if target.OwnerKind == "task" {
		m.mu.Lock()
		_, stillDesired := m.desired[key]
		canceled := m.canceled[key]
		m.mu.Unlock()
		if !stillDesired || canceled {
			_ = cmd.Process.Kill()
			_, _ = cmd.Process.Wait()
			return LaunchResult{}, fmt.Errorf("task-owned Roblox Studio launch canceled because desired target was removed for placeId %s", placeID)
		}
	}
	m.mu.Lock()
	if target.OwnerKind == "task" {
		_, stillDesired := m.desired[key]
		canceled := m.canceled[key]
		if !stillDesired || canceled {
			m.mu.Unlock()
			_ = cmd.Process.Kill()
			_, _ = cmd.Process.Wait()
			return LaunchResult{}, fmt.Errorf("task-owned Roblox Studio launch canceled after process release for placeId %s", placeID)
		}
	}
	m.processes[pid] = ManagedProcess{
		PlaceID:    placeID,
		UniverseID: universeID,
		StudioPath: studioPath,
		Source:     target.Source,
		OwnerKind:  target.OwnerKind,
		OwnerID:    target.OwnerID,
		PID:        pid,
		StartedAt:  startedAt,
		Running:    true,
	}
	m.mu.Unlock()
	if err := cmd.Process.Release(); err != nil {
		m.mu.Lock()
		delete(m.processes, pid)
		m.mu.Unlock()
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
		return LaunchResult{}, fmt.Errorf("failed to release Roblox Studio process handle for pid %d: %w", pid, err)
	}

	return LaunchResult{
		PlaceID:    placeID,
		UniverseID: universeID,
		StudioPath: studioPath,
		Args:       args,
		PID:        pid,
		Source:     target.Source,
	}, nil
}

func desiredKey(target DesiredStudio) string {
	if target.OwnerKind != "" && target.OwnerID != "" {
		return target.OwnerKind + ":" + target.OwnerID
	}
	return target.Source + ":" + target.PlaceID
}

func managedProcessMatchesDesired(process ManagedProcess, target DesiredStudio) bool {
	if process.PlaceID != target.PlaceID {
		return false
	}
	if target.OwnerKind != "" || target.OwnerID != "" {
		return process.OwnerKind == target.OwnerKind && process.OwnerID == target.OwnerID
	}
	return process.Source == target.Source && process.OwnerKind == "" && process.OwnerID == ""
}

func ResolveStudioPath() (string, error) {
	if value := os.Getenv("CLOCK_P_STUDIO_PATH"); value != "" {
		info, err := os.Stat(value)
		if err != nil {
			return "", fmt.Errorf("CLOCK_P_STUDIO_PATH does not exist: %s: %w", value, err)
		}
		if info.IsDir() {
			return "", fmt.Errorf("CLOCK_P_STUDIO_PATH is a directory, expected RobloxStudioBeta.exe: %s", value)
		}
		return value, nil
	}

	localAppData := os.Getenv("LOCALAPPDATA")
	if localAppData == "" {
		return "", errors.New("cannot resolve LOCALAPPDATA for Studio discovery")
	}

	versionsRoot := filepath.Join(localAppData, "Roblox", "Versions")
	entries, err := os.ReadDir(versionsRoot)
	if err != nil {
		return "", fmt.Errorf("cannot read Roblox Studio versions directory %s: %w", versionsRoot, err)
	}

	type candidate struct {
		modTime time.Time
		path    string
	}
	candidates := make([]candidate, 0)
	for _, entry := range entries {
		path := filepath.Join(versionsRoot, entry.Name(), "RobloxStudioBeta.exe")
		info, err := os.Stat(path)
		if err != nil || info.IsDir() {
			continue
		}
		candidates = append(candidates, candidate{
			modTime: info.ModTime(),
			path:    path,
		})
	}

	if len(candidates) == 0 {
		return "", fmt.Errorf("could not locate RobloxStudioBeta.exe under %s", versionsRoot)
	}

	sort.Slice(candidates, func(i, j int) bool {
		return candidates[i].modTime.Before(candidates[j].modTime)
	})
	return candidates[len(candidates)-1].path, nil
}

func ResolveUniverseID(ctx context.Context, placeID string) (string, error) {
	if !placeIDPattern.MatchString(placeID) {
		return "", errors.New("placeid must contain digits only")
	}

	ctx, cancel := context.WithTimeout(ctx, 10*time.Second)
	defer cancel()

	url := fmt.Sprintf("https://apis.roblox.com/universes/v1/places/%s/universe", placeID)
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, nil)
	if err != nil {
		return "", err
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		return "", fmt.Errorf("failed to resolve universeId for placeId %s: %w", placeID, err)
	}
	defer resp.Body.Close()

	if resp.StatusCode < 200 || resp.StatusCode >= 300 {
		return "", fmt.Errorf("failed to resolve universeId for placeId %s: HTTP %s", placeID, resp.Status)
	}

	var payload struct {
		UniverseID uint64 `json:"universeId"`
	}
	if err := json.NewDecoder(resp.Body).Decode(&payload); err != nil {
		return "", fmt.Errorf("failed to parse universeId response for placeId %s: %w", placeID, err)
	}
	if payload.UniverseID == 0 {
		return "", fmt.Errorf("failed to resolve universeId for placeId %s: empty universeId", placeID)
	}
	return fmt.Sprintf("%d", payload.UniverseID), nil
}
