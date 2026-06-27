//go:build windows

package studio

import (
	"fmt"
	"os/exec"
	"syscall"
	"unsafe"
)

const (
	jobObjectExtendedLimitInformationClass = 9
	jobObjectLimitKillOnJobClose           = 0x00002000

	processSetQuota  = 0x0100
	processTerminate = 0x0001
)

var (
	kernel32                     = syscall.NewLazyDLL("kernel32.dll")
	procCreateJobObjectW         = kernel32.NewProc("CreateJobObjectW")
	procSetInformationJobObject  = kernel32.NewProc("SetInformationJobObject")
	procAssignProcessToJobObject = kernel32.NewProc("AssignProcessToJobObject")
	procIsProcessInJob           = kernel32.NewProc("IsProcessInJob")
)

type ioCounters struct {
	ReadOperationCount  uint64
	WriteOperationCount uint64
	OtherOperationCount uint64
	ReadTransferCount   uint64
	WriteTransferCount  uint64
	OtherTransferCount  uint64
}

type jobObjectBasicLimitInformation struct {
	PerProcessUserTimeLimit int64
	PerJobUserTimeLimit     int64
	LimitFlags              uint32
	MinimumWorkingSetSize   uintptr
	MaximumWorkingSetSize   uintptr
	ActiveProcessLimit      uint32
	Affinity                uintptr
	PriorityClass           uint32
	SchedulingClass         uint32
}

type jobObjectExtendedLimitInformation struct {
	BasicLimitInformation jobObjectBasicLimitInformation
	IOInfo                ioCounters
	ProcessMemoryLimit    uintptr
	JobMemoryLimit        uintptr
	PeakProcessMemoryUsed uintptr
	PeakJobMemoryUsed     uintptr
}

type processJob struct {
	handle syscall.Handle
}

func newProcessJob() (*processJob, error) {
	handle, _, err := procCreateJobObjectW.Call(0, 0)
	if handle == 0 {
		return nil, fmt.Errorf("failed to create Studio job object: %w", err)
	}

	info := jobObjectExtendedLimitInformation{}
	info.BasicLimitInformation.LimitFlags = jobObjectLimitKillOnJobClose
	ok, _, err := procSetInformationJobObject.Call(
		handle,
		uintptr(jobObjectExtendedLimitInformationClass),
		uintptr(unsafe.Pointer(&info)),
		unsafe.Sizeof(info),
	)
	if ok == 0 {
		_ = syscall.CloseHandle(syscall.Handle(handle))
		return nil, fmt.Errorf("failed to configure Studio job object: %w", err)
	}

	return &processJob{handle: syscall.Handle(handle)}, nil
}

func (j *processJob) assign(cmd *exec.Cmd) error {
	if j == nil || j.handle == 0 {
		return nil
	}
	if cmd.Process == nil {
		return fmt.Errorf("cannot assign Studio process to job before process start")
	}

	processHandle, err := syscall.OpenProcess(processSetQuota|processTerminate, false, uint32(cmd.Process.Pid))
	if err != nil {
		return fmt.Errorf("failed to open Studio process %d for job assignment: %w", cmd.Process.Pid, err)
	}
	defer syscall.CloseHandle(processHandle)

	ok, _, callErr := procAssignProcessToJobObject.Call(uintptr(j.handle), uintptr(processHandle))
	if ok == 0 {
		return fmt.Errorf("failed to assign Studio process %d to helper job: %w", cmd.Process.Pid, callErr)
	}
	return nil
}

func (j *processJob) close() error {
	if j == nil || j.handle == 0 {
		return nil
	}
	err := syscall.CloseHandle(j.handle)
	j.handle = 0
	if err != nil {
		return fmt.Errorf("failed to close Studio job object: %w", err)
	}
	return nil
}

func (j *processJob) contains(pid int) bool {
	if j == nil || j.handle == 0 || pid <= 0 {
		return false
	}
	processHandle, ok := openProcessForQuery(pid)
	if !ok {
		return false
	}
	defer syscall.CloseHandle(processHandle)

	var inJob int32
	okValue, _, _ := procIsProcessInJob.Call(
		uintptr(processHandle),
		uintptr(j.handle),
		uintptr(unsafe.Pointer(&inJob)),
	)
	return okValue != 0 && inJob != 0
}
