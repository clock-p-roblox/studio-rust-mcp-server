package runtimelog

import (
	"bufio"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"regexp"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
)

const (
	DefaultMaxEntriesPerTask = 5000
	DefaultReadLimit         = 200
	MaxReadLimit             = 1000
)

var safePathPartPattern = regexp.MustCompile(`[^A-Za-z0-9_.-]+`)

type Upload struct {
	RuntimeID   string         `json:"runtime_id"`
	Mode        string         `json:"mode"`
	TimestampMS int64          `json:"timestamp_ms"`
	Level       string         `json:"level"`
	Message     string         `json:"message"`
	Fields      map[string]any `json:"fields"`
}

type Entry struct {
	Seq         int64          `json:"seq"`
	TaskID      string         `json:"task_id"`
	RuntimeID   string         `json:"runtime_id"`
	Mode        string         `json:"mode"`
	TimestampMS int64          `json:"timestamp_ms"`
	Level       string         `json:"level"`
	Message     string         `json:"message"`
	Fields      map[string]any `json:"fields"`
	ReceivedAt  time.Time      `json:"received_at"`
}

type Store struct {
	mu                sync.Mutex
	dir               string
	maxEntriesPerTask int
	nextSeq           map[string]int64
}

func NewStore(maxEntriesPerTask int) (*Store, error) {
	if maxEntriesPerTask <= 0 {
		maxEntriesPerTask = DefaultMaxEntriesPerTask
	}
	dir, err := os.MkdirTemp("", "studio-helper2-runtime-logs-*")
	if err != nil {
		return nil, err
	}
	return &Store{
		dir:               dir,
		maxEntriesPerTask: maxEntriesPerTask,
		nextSeq:           make(map[string]int64),
	}, nil
}

func (s *Store) Dir() string {
	return s.dir
}

func (s *Store) Close() error {
	if strings.TrimSpace(s.dir) == "" {
		return nil
	}
	return os.RemoveAll(s.dir)
}

func (s *Store) Append(taskID string, upload Upload) (Entry, error) {
	taskID = strings.TrimSpace(taskID)
	if taskID == "" {
		return Entry{}, errors.New("task_id must not be empty")
	}
	if err := validateUpload(upload); err != nil {
		return Entry{}, err
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	if s.nextSeq[taskID] == 0 {
		nextSeq, err := s.nextSeqForTaskLocked(taskID)
		if err != nil {
			return Entry{}, err
		}
		s.nextSeq[taskID] = nextSeq
	}
	entry := Entry{
		Seq:         s.nextSeq[taskID],
		TaskID:      taskID,
		RuntimeID:   strings.TrimSpace(upload.RuntimeID),
		Mode:        strings.TrimSpace(upload.Mode),
		TimestampMS: upload.TimestampMS,
		Level:       strings.TrimSpace(upload.Level),
		Message:     upload.Message,
		Fields:      upload.Fields,
		ReceivedAt:  time.Now().UTC(),
	}
	if entry.Fields == nil {
		entry.Fields = map[string]any{}
	}
	s.nextSeq[taskID]++

	if err := s.appendEntryLocked(entry); err != nil {
		return Entry{}, err
	}
	if err := s.enforceRetentionLocked(taskID); err != nil {
		return Entry{}, err
	}
	return entry, nil
}

func (s *Store) Read(taskID string, cursor string, limit int) ([]Entry, string, error) {
	taskID = strings.TrimSpace(taskID)
	if taskID == "" {
		return nil, "", errors.New("task_id must not be empty")
	}
	afterSeq := int64(0)
	if strings.TrimSpace(cursor) != "" {
		parsed, err := strconv.ParseInt(strings.TrimSpace(cursor), 10, 64)
		if err != nil || parsed < 0 {
			return nil, "", fmt.Errorf("cursor must be a non-negative integer: %s", cursor)
		}
		afterSeq = parsed
	}
	if limit <= 0 {
		limit = DefaultReadLimit
	}
	if limit > MaxReadLimit {
		limit = MaxReadLimit
	}

	s.mu.Lock()
	defer s.mu.Unlock()

	entries, err := s.readTaskEntriesLocked(taskID)
	if err != nil {
		return nil, "", err
	}
	filtered := make([]Entry, 0, minInt(limit, len(entries)))
	nextCursor := strconv.FormatInt(afterSeq, 10)
	for _, entry := range entries {
		if entry.Seq <= afterSeq {
			continue
		}
		filtered = append(filtered, entry)
		nextCursor = strconv.FormatInt(entry.Seq, 10)
		if len(filtered) >= limit {
			break
		}
	}
	return filtered, nextCursor, nil
}

func validateUpload(upload Upload) error {
	if strings.TrimSpace(upload.RuntimeID) == "" {
		return errors.New("runtime_id must not be empty")
	}
	if strings.TrimSpace(upload.Mode) == "" {
		return errors.New("mode must not be empty")
	}
	if upload.TimestampMS <= 0 {
		return errors.New("timestamp_ms must be positive")
	}
	if strings.TrimSpace(upload.Level) == "" {
		return errors.New("level must not be empty")
	}
	return nil
}

func (s *Store) appendEntryLocked(entry Entry) error {
	if err := os.MkdirAll(s.taskDir(entry.TaskID), 0o755); err != nil {
		return err
	}
	file, err := os.OpenFile(s.runtimePath(entry.TaskID, entry.RuntimeID), os.O_CREATE|os.O_WRONLY|os.O_APPEND, 0o644)
	if err != nil {
		return err
	}
	defer file.Close()
	body, err := json.Marshal(entry)
	if err != nil {
		return err
	}
	if _, err := file.Write(append(body, '\n')); err != nil {
		return err
	}
	return nil
}

func (s *Store) enforceRetentionLocked(taskID string) error {
	entries, err := s.readTaskEntriesLocked(taskID)
	if err != nil {
		return err
	}
	if len(entries) <= s.maxEntriesPerTask {
		return nil
	}
	entries = entries[len(entries)-s.maxEntriesPerTask:]
	return s.rewriteTaskEntriesLocked(taskID, entries)
}

func (s *Store) nextSeqForTaskLocked(taskID string) (int64, error) {
	entries, err := s.readTaskEntriesLocked(taskID)
	if err != nil {
		return 0, err
	}
	next := int64(1)
	for _, entry := range entries {
		if entry.Seq >= next {
			next = entry.Seq + 1
		}
	}
	return next, nil
}

func (s *Store) readTaskEntriesLocked(taskID string) ([]Entry, error) {
	taskDir := s.taskDir(taskID)
	files, err := os.ReadDir(taskDir)
	if os.IsNotExist(err) {
		return []Entry{}, nil
	}
	if err != nil {
		return nil, err
	}
	entries := make([]Entry, 0)
	for _, file := range files {
		if file.IsDir() || !strings.HasSuffix(file.Name(), ".jsonl") {
			continue
		}
		fileEntries, err := readEntriesFromFile(filepath.Join(taskDir, file.Name()))
		if err != nil {
			return nil, err
		}
		entries = append(entries, fileEntries...)
	}
	sort.Slice(entries, func(i, j int) bool {
		if entries[i].Seq == entries[j].Seq {
			return entries[i].RuntimeID < entries[j].RuntimeID
		}
		return entries[i].Seq < entries[j].Seq
	})
	return entries, nil
}

func readEntriesFromFile(path string) ([]Entry, error) {
	file, err := os.Open(path)
	if err != nil {
		return nil, err
	}
	defer file.Close()

	entries := make([]Entry, 0)
	scanner := bufio.NewScanner(file)
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}
		var entry Entry
		if err := json.Unmarshal([]byte(line), &entry); err != nil {
			return nil, fmt.Errorf("failed to parse runtime log %s: %w", path, err)
		}
		entries = append(entries, entry)
	}
	if err := scanner.Err(); err != nil {
		return nil, err
	}
	return entries, nil
}

func (s *Store) rewriteTaskEntriesLocked(taskID string, entries []Entry) error {
	taskDir := s.taskDir(taskID)
	if err := os.RemoveAll(taskDir); err != nil {
		return err
	}
	if err := os.MkdirAll(taskDir, 0o755); err != nil {
		return err
	}
	for _, entry := range entries {
		if err := s.appendEntryLocked(entry); err != nil {
			return err
		}
	}
	return nil
}

func (s *Store) taskDir(taskID string) string {
	return filepath.Join(s.dir, sanitizePathPart(taskID))
}

func (s *Store) runtimePath(taskID string, runtimeID string) string {
	return filepath.Join(s.taskDir(taskID), sanitizePathPart(runtimeID)+".jsonl")
}

func sanitizePathPart(value string) string {
	value = safePathPartPattern.ReplaceAllString(strings.TrimSpace(value), "_")
	value = strings.Trim(value, "._")
	if value == "" {
		return "unknown"
	}
	return value
}

func minInt(a int, b int) int {
	if a < b {
		return a
	}
	return b
}
