package runtimelog

import (
	"os"
	"testing"
)

func TestStoreAppendReadCursorAndRetention(t *testing.T) {
	store, err := NewStore(2)
	if err != nil {
		t.Fatalf("new store failed: %v", err)
	}
	defer store.Close()

	for i := 0; i < 3; i++ {
		if _, err := store.Append("task-a", Upload{
			RuntimeID:   "server",
			Mode:        "play_server",
			TimestampMS: int64(1000 + i),
			Level:       "info",
			Message:     "line",
			Fields:      map[string]any{"i": i},
		}); err != nil {
			t.Fatalf("append %d failed: %v", i, err)
		}
	}

	entries, nextCursor, err := store.Read("task-a", "", 1)
	if err != nil {
		t.Fatalf("read first page failed: %v", err)
	}
	if len(entries) != 1 || entries[0].Seq != 2 || nextCursor != "2" {
		t.Fatalf("first page = %+v cursor=%q, want seq 2", entries, nextCursor)
	}

	entries, nextCursor, err = store.Read("task-a", nextCursor, 10)
	if err != nil {
		t.Fatalf("read second page failed: %v", err)
	}
	if len(entries) != 1 || entries[0].Seq != 3 || nextCursor != "3" {
		t.Fatalf("second page = %+v cursor=%q, want seq 3", entries, nextCursor)
	}
}

func TestStoreSeparatesTasks(t *testing.T) {
	store, err := NewStore(10)
	if err != nil {
		t.Fatalf("new store failed: %v", err)
	}
	defer store.Close()

	if _, err := store.Append("task-a", validUpload("a")); err != nil {
		t.Fatalf("append task a failed: %v", err)
	}
	if _, err := store.Append("task-b", validUpload("b")); err != nil {
		t.Fatalf("append task b failed: %v", err)
	}
	entries, _, err := store.Read("task-a", "", 10)
	if err != nil {
		t.Fatalf("read task a failed: %v", err)
	}
	if len(entries) != 1 || entries[0].Message != "a" {
		t.Fatalf("task a entries = %+v", entries)
	}
}

func TestStoreCloseCleansDirectory(t *testing.T) {
	store, err := NewStore(10)
	if err != nil {
		t.Fatalf("new store failed: %v", err)
	}
	dir := store.Dir()
	if _, err := store.Append("task-a", validUpload("a")); err != nil {
		t.Fatalf("append failed: %v", err)
	}
	if err := store.Close(); err != nil {
		t.Fatalf("close failed: %v", err)
	}
	if _, err := os.Stat(dir); !os.IsNotExist(err) {
		t.Fatalf("store dir still exists after close: %s err=%v", dir, err)
	}
}

func validUpload(message string) Upload {
	return Upload{
		RuntimeID:   "server",
		Mode:        "play_server",
		TimestampMS: 1234,
		Level:       "info",
		Message:     message,
		Fields:      map[string]any{},
	}
}
