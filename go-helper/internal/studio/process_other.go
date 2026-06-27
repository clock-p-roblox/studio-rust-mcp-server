//go:build !windows

package studio

import (
	"syscall"
	"time"
)

func processIsRunning(pid int) bool {
	return syscall.Kill(pid, 0) == nil
}

func processIsManagedRunning(pid int, _ time.Time) bool {
	return processIsRunning(pid)
}

func processStartTime(_ int) (time.Time, bool) {
	return time.Time{}, false
}

func killProcess(pid int) error {
	if pid <= 0 {
		return nil
	}
	return syscall.Kill(pid, syscall.SIGKILL)
}
