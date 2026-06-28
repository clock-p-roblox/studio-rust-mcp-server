package studio

import "testing"

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
