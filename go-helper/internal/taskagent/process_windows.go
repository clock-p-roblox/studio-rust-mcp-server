//go:build windows

package taskagent

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
		return nil, fmt.Errorf("failed to create Rojo job object: %w", err)
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
		return nil, fmt.Errorf("failed to configure Rojo job object: %w", err)
	}

	return &processJob{handle: syscall.Handle(handle)}, nil
}

func (j *processJob) assign(cmd *exec.Cmd) error {
	if j == nil || j.handle == 0 {
		return nil
	}
	if cmd.Process == nil {
		return fmt.Errorf("cannot assign Rojo process to job before process start")
	}

	processHandle, err := syscall.OpenProcess(processSetQuota|processTerminate, false, uint32(cmd.Process.Pid))
	if err != nil {
		return fmt.Errorf("failed to open Rojo process %d for job assignment: %w", cmd.Process.Pid, err)
	}
	defer syscall.CloseHandle(processHandle)

	ok, _, callErr := procAssignProcessToJobObject.Call(uintptr(j.handle), uintptr(processHandle))
	if ok == 0 {
		return fmt.Errorf("failed to assign Rojo process %d to task-agent job: %w", cmd.Process.Pid, callErr)
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
		return fmt.Errorf("failed to close Rojo job object: %w", err)
	}
	return nil
}

func startManagedCommand(cmd *exec.Cmd, job *processJob) error {
	cmd.SysProcAttr = &syscall.SysProcAttr{CreationFlags: syscall.CREATE_NEW_PROCESS_GROUP}
	if err := cmd.Start(); err != nil {
		return err
	}
	if err := job.assign(cmd); err != nil {
		_ = cmd.Process.Kill()
		_, _ = cmd.Process.Wait()
		return err
	}
	return nil
}

func stopManagedCommand(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	_ = exec.Command("taskkill", "/PID", fmt.Sprintf("%d", cmd.Process.Pid), "/T", "/F").Run()
}
