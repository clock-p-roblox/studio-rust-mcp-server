//go:build linux

package taskagent

import (
	"os/exec"
	"syscall"
	"time"
)

type processJob struct{}

func newProcessJob() (*processJob, error) {
	return &processJob{}, nil
}

func (j *processJob) close() error {
	return nil
}

func startManagedCommand(cmd *exec.Cmd, _ *processJob) error {
	cmd.SysProcAttr = &syscall.SysProcAttr{
		Setpgid:   true,
		Pdeathsig: syscall.SIGTERM,
	}
	return cmd.Start()
}

func stopManagedCommand(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	stopProcessGroup(cmd.Process.Pid)
}

func stopProcessGroup(pid int) {
	if pid <= 0 {
		return
	}
	_ = syscall.Kill(-pid, syscall.SIGTERM)
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if syscall.Kill(-pid, 0) != nil {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	_ = syscall.Kill(-pid, syscall.SIGKILL)
}
