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
	th32csSnapProcess              = 0x00000002
)

var (
	procGetExitCodeProcess       = kernel32.NewProc("GetExitCodeProcess")
	procGetProcessTimes          = kernel32.NewProc("GetProcessTimes")
	procCreateToolhelp32Snapshot = kernel32.NewProc("CreateToolhelp32Snapshot")
	procProcess32FirstW          = kernel32.NewProc("Process32FirstW")
	procProcess32NextW           = kernel32.NewProc("Process32NextW")
)

type processEntry32 struct {
	Size            uint32
	Usage           uint32
	ProcessID       uint32
	DefaultHeapID   uintptr
	ModuleID        uint32
	ThreadCount     uint32
	ParentProcessID uint32
	PriClassBase    int32
	Flags           uint32
	ExeFile         [260]uint16
}

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

func processParentID(pid int) (int, bool) {
	if pid <= 0 {
		return 0, false
	}
	snapshot, _, _ := procCreateToolhelp32Snapshot.Call(uintptr(th32csSnapProcess), 0)
	if snapshot == uintptr(syscall.InvalidHandle) || snapshot == 0 {
		return 0, false
	}
	defer syscall.CloseHandle(syscall.Handle(snapshot))

	entry := processEntry32{Size: uint32(unsafe.Sizeof(processEntry32{}))}
	ok, _, _ := procProcess32FirstW.Call(snapshot, uintptr(unsafe.Pointer(&entry)))
	for ok != 0 {
		if int(entry.ProcessID) == pid {
			return int(entry.ParentProcessID), true
		}
		ok, _, _ = procProcess32NextW.Call(snapshot, uintptr(unsafe.Pointer(&entry)))
	}
	return 0, false
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
