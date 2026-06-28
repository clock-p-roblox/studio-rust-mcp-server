package studio

import (
	"os"
	"os/exec"
	"reflect"
	"testing"
	"time"
)

func TestEnsureTaskDesiredAllowsSamePlaceIndependentTasks(t *testing.T) {
	manager, err := NewManager(nil)
	if err != nil {
		t.Fatalf("new manager failed: %v", err)
	}
	t.Cleanup(func() {
		if err := manager.Close(); err != nil {
			t.Fatalf("close manager failed: %v", err)
		}
	})

	manager.EnsureTaskDesired("task-a", "123")
	manager.EnsureTaskDesired("task-b", "123")

	desiredByTask := make(map[string]DesiredStudio)
	for _, target := range manager.Summary().Desired {
		if target.OwnerKind == "task" {
			desiredByTask[target.OwnerID] = target
		}
	}
	if len(desiredByTask) != 2 {
		t.Fatalf("desired task targets = %+v, want two independent same-place targets", desiredByTask)
	}
	for _, taskID := range []string{"task-a", "task-b"} {
		target, ok := desiredByTask[taskID]
		if !ok {
			t.Fatalf("missing desired target for %s: %+v", taskID, desiredByTask)
		}
		if target.PlaceID != "123" || target.Source != LaunchSourceTask {
			t.Fatalf("target for %s = %+v, want place 123 task source", taskID, target)
		}
	}

	manager.RemoveTaskDesired("task-a")
	remaining := manager.Summary().Desired
	if len(remaining) != 1 || remaining[0].OwnerID != "task-b" || remaining[0].PlaceID != "123" {
		t.Fatalf("remaining desired targets after removing task-a = %+v, want task-b only", remaining)
	}
}

func TestKillTaskStudiosDetachesAndKillsOnlyMatchingTask(t *testing.T) {
	manager, err := NewManager(nil)
	if err != nil {
		t.Fatalf("new manager failed: %v", err)
	}
	t.Cleanup(func() {
		if err := manager.Close(); err != nil {
			t.Fatalf("close manager failed: %v", err)
		}
	})

	processA, startedA := startManagedTestProcess(t)
	processB, startedB := startManagedTestProcess(t)
	t.Cleanup(func() {
		_ = killProcess(processA.Process.Pid)
		_ = killProcess(processB.Process.Pid)
	})

	manager.mu.Lock()
	manager.processes[processA.Process.Pid] = ManagedProcess{
		PlaceID:   "123",
		Source:    LaunchSourceTask,
		OwnerKind: "task",
		OwnerID:   "task-a",
		PID:       processA.Process.Pid,
		StartedAt: startedA,
		Running:   true,
	}
	manager.processes[processB.Process.Pid] = ManagedProcess{
		PlaceID:   "123",
		Source:    LaunchSourceTask,
		OwnerKind: "task",
		OwnerID:   "task-b",
		PID:       processB.Process.Pid,
		StartedAt: startedB,
		Running:   true,
	}
	manager.mu.Unlock()

	killed := manager.KillTaskStudios("task-b")
	if !reflect.DeepEqual(killed, []int{processB.Process.Pid}) {
		t.Fatalf("killed pids = %+v, want task B only %d", killed, processB.Process.Pid)
	}
	waitUntilProcessStops(t, processB.Process.Pid)
	if !processIsManagedRunning(processA.Process.Pid, startedA) {
		t.Fatal("task A process was killed by task B cleanup")
	}
	summary := manager.Summary()
	if len(summary.Studios) != 1 || summary.Studios[0].OwnerID != "task-a" || summary.Studios[0].PID != processA.Process.Pid {
		t.Fatalf("manager summary after task B cleanup = %+v, want task A only", summary.Studios)
	}
}

func TestHelperProcess(t *testing.T) {
	if os.Getenv("CLOCK_P_STUDIO_TEST_HELPER_PROCESS") != "1" {
		return
	}
	time.Sleep(time.Minute)
	os.Exit(0)
}

func startManagedTestProcess(t *testing.T) (*exec.Cmd, time.Time) {
	t.Helper()
	cmd := exec.Command(os.Args[0], "-test.run=TestHelperProcess")
	cmd.Env = append(os.Environ(), "CLOCK_P_STUDIO_TEST_HELPER_PROCESS=1")
	if err := cmd.Start(); err != nil {
		t.Fatalf("failed to start helper process: %v", err)
	}
	startedAt := waitUntilProcessStartTime(t, cmd.Process.Pid)
	return cmd, startedAt
}

func waitUntilProcessStartTime(t *testing.T, pid int) time.Time {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if startedAt, ok := processStartTime(pid); ok {
			return startedAt
		}
		time.Sleep(25 * time.Millisecond)
	}
	t.Fatalf("process %d did not expose a start time", pid)
	return time.Time{}
}

func waitUntilProcessStops(t *testing.T, pid int) {
	t.Helper()
	deadline := time.Now().Add(5 * time.Second)
	for time.Now().Before(deadline) {
		if !processIsRunning(pid) {
			return
		}
		time.Sleep(25 * time.Millisecond)
	}
	t.Fatalf("process %d did not stop", pid)
}
