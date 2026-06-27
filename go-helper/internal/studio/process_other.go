//go:build !windows

package studio

import "syscall"

func processIsRunning(pid int) bool {
	return syscall.Kill(pid, 0) == nil
}

func killProcess(pid int) error {
	if pid <= 0 {
		return nil
	}
	return syscall.Kill(pid, syscall.SIGKILL)
}
