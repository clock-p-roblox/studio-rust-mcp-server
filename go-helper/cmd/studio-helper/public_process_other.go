//go:build !windows && !linux

package main

import (
	"os/exec"
	"syscall"
	"time"
)

type publicProcessJob struct{}

func newPublicProcessJob() (*publicProcessJob, error) {
	return &publicProcessJob{}, nil
}

func (j *publicProcessJob) close() error {
	return nil
}

func startPublicManagedCommand(cmd *exec.Cmd, _ *publicProcessJob) error {
	cmd.SysProcAttr = &syscall.SysProcAttr{Setpgid: true}
	return cmd.Start()
}

func stopPublicManagedCommand(cmd *exec.Cmd) {
	if cmd.Process == nil || cmd.Process.Pid <= 0 {
		return
	}
	_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGTERM)
	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if syscall.Kill(-cmd.Process.Pid, 0) != nil {
			return
		}
		time.Sleep(50 * time.Millisecond)
	}
	_ = syscall.Kill(-cmd.Process.Pid, syscall.SIGKILL)
}
