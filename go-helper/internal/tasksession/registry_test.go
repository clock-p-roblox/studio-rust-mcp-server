package tasksession

import (
	"testing"
	"time"
)

type fakeDesiredStore struct {
	desired map[string]string
	killed  []string
}

func newFakeDesiredStore() *fakeDesiredStore {
	return &fakeDesiredStore{desired: make(map[string]string)}
}

func (s *fakeDesiredStore) EnsureTaskDesired(taskID string, placeID string) {
	s.desired[taskID] = placeID
}

func (s *fakeDesiredStore) RemoveTaskDesired(taskID string) {
	delete(s.desired, taskID)
}

func (s *fakeDesiredStore) KillTaskStudios(taskID string) []int {
	s.killed = append(s.killed, taskID)
	return []int{101}
}

func validHeartbeat(taskID string) HeartbeatRequest {
	return HeartbeatRequest{
		TaskID:               taskID,
		MachineName:          "win-a",
		PlaceID:              "123",
		TaskAgentPID:         42,
		TaskAgentStartedAtMS: 1000,
		RojoUpstreamURL:      "http://127.0.0.1:5000",
	}
}

func TestHeartbeatCreatesAndRefreshesLiveSession(t *testing.T) {
	store := newFakeDesiredStore()
	registry := NewRegistry(31*time.Second, store)

	response, err := registry.Heartbeat("t123", validHeartbeat("t123"))
	if err != nil {
		t.Fatalf("heartbeat failed: %v", err)
	}
	if !response.OK || response.State != "live" {
		t.Fatalf("unexpected heartbeat response: %+v", response)
	}
	if store.desired["t123"] != "123" {
		t.Fatalf("desired Studio target was not recorded: %+v", store.desired)
	}

	if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
		t.Fatalf("refresh heartbeat failed: %v", err)
	}
	if status := registry.Status("t123"); !status.OK || status.State != "live" {
		t.Fatalf("unexpected status: %+v", status)
	}
}

func TestHeartbeatRejectsImmutableChanges(t *testing.T) {
	registry := NewRegistry(31*time.Second, newFakeDesiredStore())
	if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
		t.Fatalf("heartbeat failed: %v", err)
	}

	changed := validHeartbeat("t123")
	changed.RojoUpstreamURL = "http://127.0.0.1:6000"
	_, err := registry.Heartbeat("t123", changed)
	if err == nil {
		t.Fatal("expected immutable mismatch")
	}
	sessionError, ok := err.(*Error)
	if !ok || sessionError.Code != CodeImmutableMismatch {
		t.Fatalf("expected immutable mismatch, got %T %v", err, err)
	}
}

func TestReleaseCreatesEndedTombstone(t *testing.T) {
	store := newFakeDesiredStore()
	registry := NewRegistry(31*time.Second, store)
	if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
		t.Fatalf("heartbeat failed: %v", err)
	}
	release, err := registry.Release("t123", ReleaseRequest{TaskAgentPID: 42, TaskAgentStartedAtMS: 1000})
	if err != nil {
		t.Fatalf("release failed: %v", err)
	}
	if release.State != "ended" || len(release.KilledPIDs) != 1 {
		t.Fatalf("unexpected release response: %+v", release)
	}
	if _, ok := store.desired["t123"]; ok {
		t.Fatal("desired target survived release")
	}

	_, err = registry.Heartbeat("t123", validHeartbeat("t123"))
	if err == nil {
		t.Fatal("expected heartbeat after release to be rejected")
	}
	sessionError, ok := err.(*Error)
	if !ok || sessionError.Code != CodeTaskEnded {
		t.Fatalf("expected task_ended, got %T %v", err, err)
	}
	again, err := registry.Release("t123", ReleaseRequest{TaskAgentPID: 42, TaskAgentStartedAtMS: 1000})
	if err != nil {
		t.Fatalf("idempotent release failed: %v", err)
	}
	if again.KilledPIDs == nil || len(again.KilledPIDs) != 0 {
		t.Fatalf("idempotent release should return empty killed_pids array: %+v", again)
	}
}

func TestReleaseRejectsMismatchedIdentity(t *testing.T) {
	registry := NewRegistry(31*time.Second, newFakeDesiredStore())
	if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
		t.Fatalf("heartbeat failed: %v", err)
	}
	_, err := registry.Release("t123", ReleaseRequest{TaskAgentPID: 99, TaskAgentStartedAtMS: 1000})
	if err == nil {
		t.Fatal("expected release identity mismatch")
	}
	sessionError, ok := err.(*Error)
	if !ok || sessionError.Code != CodeImmutableMismatch {
		t.Fatalf("expected immutable mismatch, got %T %v", err, err)
	}
}

func TestLeaseExpiryIsRecoverableWithSameContract(t *testing.T) {
	store := newFakeDesiredStore()
	registry := NewRegistry(31*time.Second, store)
	now := time.Unix(100, 0)
	registry.SetNowForTest(func() time.Time { return now })
	if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
		t.Fatalf("heartbeat failed: %v", err)
	}

	now = now.Add(32 * time.Second)
	expired := registry.ExpireStale()
	if len(expired) != 1 || expired[0] != "t123" {
		t.Fatalf("unexpected expired sessions: %+v", expired)
	}
	if status := registry.Status("t123"); !status.OK || status.State != "expired" {
		t.Fatalf("unexpected expired status: %+v", status)
	}

	if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
		t.Fatalf("recovering heartbeat failed: %v", err)
	}
	if status := registry.Status("t123"); !status.OK || status.State != "live" {
		t.Fatalf("unexpected recovered status: %+v", status)
	}
}

func TestHeartbeatRejectsInvalidPlaceID(t *testing.T) {
	registry := NewRegistry(31*time.Second, newFakeDesiredStore())
	request := validHeartbeat("t123")
	request.PlaceID = "abc"
	if _, err := registry.Heartbeat("t123", request); err == nil {
		t.Fatal("expected invalid place_id to be rejected")
	}
}
