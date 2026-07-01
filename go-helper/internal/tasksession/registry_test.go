package tasksession

import (
	"testing"
	"time"
)

type fakeDesiredStore struct {
	desired map[string]string
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

func validHeartbeat(taskID string) HeartbeatRequest {
	return HeartbeatRequest{
		TaskID:               taskID,
		MachineName:          "win-a",
		PlaceID:              "123",
		TaskAgentPID:         42,
		TaskAgentStartedAtMS: 1000,
		TaskSessionToken:     "token-" + taskID,
		CodeSync: CodeSyncBinding{
			ProtocolVersion:    2,
			WorkspaceID:        "workspace-" + taskID,
			PlaceID:            "123",
			MachineName:        "win-a",
			MappingProfile:     "sync_lua_v1",
			CodeSyncConfigHash: "config-hash-" + taskID,
			RootsAuthorityHash: "roots-hash-" + taskID,
			Roots: []CodeSyncRootRoute{
				{RootID: "root", StudioPath: []string{"Workspace", "ClockPTest"}},
			},
		},
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
	cases := []struct {
		name   string
		mutate func(*HeartbeatRequest)
	}{
		{
			name: "machine_name",
			mutate: func(request *HeartbeatRequest) {
				request.MachineName = "win-b"
				request.CodeSync.MachineName = "win-b"
			},
		},
		{
			name: "place_id",
			mutate: func(request *HeartbeatRequest) {
				request.PlaceID = "456"
				request.CodeSync.PlaceID = "456"
			},
		},
		{
			name: "task_agent_pid",
			mutate: func(request *HeartbeatRequest) {
				request.TaskAgentPID = 99
			},
		},
		{
			name: "task_agent_started_at_ms",
			mutate: func(request *HeartbeatRequest) {
				request.TaskAgentStartedAtMS = 2000
			},
		},
		{
			name: "code_sync_config_hash",
			mutate: func(request *HeartbeatRequest) {
				request.CodeSync.CodeSyncConfigHash = "changed"
			},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			registry := NewRegistry(31*time.Second, newFakeDesiredStore())
			if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
				t.Fatalf("heartbeat failed: %v", err)
			}

			changed := validHeartbeat("t123")
			tc.mutate(&changed)
			_, err := registry.Heartbeat("t123", changed)
			if err == nil {
				t.Fatal("expected immutable mismatch")
			}
			sessionError, ok := err.(*Error)
			if !ok || sessionError.Code != CodeImmutableMismatch {
				t.Fatalf("expected immutable mismatch, got %T %v", err, err)
			}
		})
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
	if release.State != "ended" || len(release.KilledPIDs) != 0 {
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

func TestLeaseExpiryRejectsChangedContract(t *testing.T) {
	cases := []struct {
		name   string
		mutate func(*HeartbeatRequest)
	}{
		{
			name: "machine_name",
			mutate: func(request *HeartbeatRequest) {
				request.MachineName = "win-b"
				request.CodeSync.MachineName = "win-b"
			},
		},
		{
			name: "place_id",
			mutate: func(request *HeartbeatRequest) {
				request.PlaceID = "456"
				request.CodeSync.PlaceID = "456"
			},
		},
		{
			name: "task_agent_pid",
			mutate: func(request *HeartbeatRequest) {
				request.TaskAgentPID = 99
			},
		},
		{
			name: "task_agent_started_at_ms",
			mutate: func(request *HeartbeatRequest) {
				request.TaskAgentStartedAtMS = 2000
			},
		},
		{
			name: "roots_authority_hash",
			mutate: func(request *HeartbeatRequest) {
				request.CodeSync.RootsAuthorityHash = "changed"
			},
		},
	}
	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			store := newFakeDesiredStore()
			registry := NewRegistry(31*time.Second, store)
			now := time.Unix(100, 0)
			registry.SetNowForTest(func() time.Time { return now })
			if _, err := registry.Heartbeat("t123", validHeartbeat("t123")); err != nil {
				t.Fatalf("heartbeat failed: %v", err)
			}

			now = now.Add(32 * time.Second)
			if expired := registry.ExpireStale(); len(expired) != 1 || expired[0] != "t123" {
				t.Fatalf("unexpected expired sessions: %+v", expired)
			}

			changed := validHeartbeat("t123")
			tc.mutate(&changed)
			_, err := registry.Heartbeat("t123", changed)
			if err == nil {
				t.Fatal("expected changed contract after expiry to be rejected")
			}
			sessionError, ok := err.(*Error)
			if !ok || sessionError.Code != CodeImmutableMismatch {
				t.Fatalf("expected immutable mismatch, got %T %v", err, err)
			}
			if _, ok := store.desired["t123"]; ok {
				t.Fatal("changed expired heartbeat restored desired target")
			}
		})
	}
}

func TestSamePlaceTasksRemainIndependentThroughReleaseAndExpiry(t *testing.T) {
	store := newFakeDesiredStore()
	registry := NewRegistry(31*time.Second, store)
	now := time.Unix(100, 0)
	registry.SetNowForTest(func() time.Time { return now })

	taskA := validHeartbeat("task-a")
	taskA.TaskAgentPID = 101
	taskA.TaskAgentStartedAtMS = 1001
	taskB := validHeartbeat("task-b")
	taskB.TaskAgentPID = 102
	taskB.TaskAgentStartedAtMS = 1002
	if _, err := registry.Heartbeat("task-a", taskA); err != nil {
		t.Fatalf("task A heartbeat failed: %v", err)
	}
	if _, err := registry.Heartbeat("task-b", taskB); err != nil {
		t.Fatalf("task B heartbeat failed: %v", err)
	}
	if store.desired["task-a"] != "123" || store.desired["task-b"] != "123" {
		t.Fatalf("same-place desired targets were not independent: %+v", store.desired)
	}

	if _, err := registry.Release("task-b", ReleaseRequest{TaskAgentPID: 102, TaskAgentStartedAtMS: 1002}); err != nil {
		t.Fatalf("task B release failed: %v", err)
	}
	if _, ok := store.desired["task-b"]; ok {
		t.Fatal("task B desired target survived release")
	}
	if store.desired["task-a"] != "123" {
		t.Fatalf("task A desired target was mutated by task B release: %+v", store.desired)
	}

	now = now.Add(32 * time.Second)
	if expired := registry.ExpireStale(); len(expired) != 1 || expired[0] != "task-a" {
		t.Fatalf("unexpected expired sessions: %+v", expired)
	}
	if len(store.desired) != 0 {
		t.Fatalf("desired targets survived task A expiry: %+v", store.desired)
	}
	if status := registry.Status("task-b"); !status.OK || status.State != "ended" {
		t.Fatalf("task B ended tombstone was mutated by task A expiry: %+v", status)
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
