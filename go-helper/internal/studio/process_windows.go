//go:build windows

package studio

import (
	"fmt"
	"os/exec"
	"syscall"
	"time"
	"unsafe"
)

const (
	processQueryLimitedInformation = 0x1000
	stillActive                    = 259
)

var (
	procGetExitCodeProcess = kernel32.NewProc("GetExitCodeProcess")
	procGetProcessTimes    = kernel32.NewProc("GetProcessTimes")
)

func processIsRunning(pid int) bool {
	handle, ok := openProcessForQuery(pid)
	if !ok {
		return false
	}
	defer syscall.CloseHandle(handle)

	return processHandleIsRunning(handle)
}

func processIsManagedRunning(pid int, startedAt time.Time) bool {
	handle, ok := openProcessForQuery(pid)
	if !ok {
		return false
	}
	defer syscall.CloseHandle(handle)

	if !processHandleIsRunning(handle) {
		return false
	}
	if startedAt.IsZero() {
		return true
	}
	creationTime, ok := processHandleStartTime(handle)
	return ok && creationTime.Equal(startedAt)
}

func processStartTime(pid int) (time.Time, bool) {
	handle, ok := openProcessForQuery(pid)
	if !ok {
		return time.Time{}, false
	}
	defer syscall.CloseHandle(handle)

	return processHandleStartTime(handle)
}

func openProcessForQuery(pid int) (syscall.Handle, bool) {
	handle, err := syscall.OpenProcess(processQueryLimitedInformation, false, uint32(pid))
	if err != nil {
		return 0, false
	}
	return handle, true
}

func processHandleIsRunning(handle syscall.Handle) bool {
	var exitCode uint32
	ok, _, _ := procGetExitCodeProcess.Call(uintptr(handle), uintptr(unsafe.Pointer(&exitCode)))
	return ok != 0 && exitCode == stillActive
}

func processHandleStartTime(handle syscall.Handle) (time.Time, bool) {
	var creationTime syscall.Filetime
	var exitTime syscall.Filetime
	var kernelTime syscall.Filetime
	var userTime syscall.Filetime
	ok, _, _ := procGetProcessTimes.Call(
		uintptr(handle),
		uintptr(unsafe.Pointer(&creationTime)),
		uintptr(unsafe.Pointer(&exitTime)),
		uintptr(unsafe.Pointer(&kernelTime)),
		uintptr(unsafe.Pointer(&userTime)),
	)
	if ok == 0 {
		return time.Time{}, false
	}
	return time.Unix(0, creationTime.Nanoseconds()).UTC(), true
}

func killProcess(pid int) error {
	if pid <= 0 {
		return nil
	}
	return exec.Command("taskkill", "/PID", fmt.Sprintf("%d", pid), "/T", "/F").Run()
}
