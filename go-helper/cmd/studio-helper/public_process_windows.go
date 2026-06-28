//go:build windows

package main

import (
	"fmt"
	"os/exec"
	"syscall"
	"unsafe"
)

const (
	publicJobObjectExtendedLimitInformationClass = 9
	publicJobObjectLimitKillOnJobClose           = 0x00002000

	publicProcessSetQuota  = 0x0100
	publicProcessTerminate = 0x0001
)

var (
	publicKernel32                     = syscall.NewLazyDLL("kernel32.dll")
	publicProcCreateJobObjectW         = publicKernel32.NewProc("CreateJobObjectW")
	publicProcSetInformationJobObject  = publicKernel32.NewProc("SetInformationJobObject")
	publicProcAssignProcessToJobObject = publicKernel32.NewProc("AssignProcessToJobObject")
)

type publicIOCounters struct {
	ReadOperationCount  uint64
	WriteOperationCount uint64
	OtherOperationCount uint64
	ReadTransferCount   uint64
	WriteTransferCount  uint64
	OtherTransferCount  uint64
}

type publicJobObjectBasicLimitInformation struct {
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

type publicJobObjectExtendedLimitInformation struct {
	BasicLimitInformation publicJobObjectBasicLimitInformation
	IOInfo                publicIOCounters
	ProcessMemoryLimit    uintptr
	JobMemoryLimit        uintptr
	PeakProcessMemoryUsed uintptr
	PeakJobMemoryUsed     uintptr
}

type publicProcessJob struct {
	handle syscall.Handle
}

func newPublicProcessJob() (*publicProcessJob, error) {
	handle, _, err := publicProcCreateJobObjectW.Call(0, 0)
	if handle == 0 {
		return nil, fmt.Errorf("failed to create public exposure job object: %w", err)
	}

	info := publicJobObjectExtendedLimitInformation{}
	info.BasicLimitInformation.LimitFlags = publicJobObjectLimitKillOnJobClose
	ok, _, err := publicProcSetInformationJobObject.Call(
		handle,
		uintptr(publicJobObjectExtendedLimitInformationClass),
		uintptr(unsafe.Pointer(&info)),
		unsafe.Sizeof(info),
	)
	if ok == 0 {
		_ = syscall.CloseHandle(syscall.Handle(handle))
		return nil, fmt.Errorf("failed to configure public exposure job object: %w", err)
	}
	return &publicProcessJob{handle: syscall.Handle(handle)}, nil
}

func (j *publicProcessJob) assign(cmd *exec.Cmd) error {
	if j == nil || j.handle == 0 {
		return nil
	}
	if cmd.Process == nil {
		return fmt.Errorf("cannot assign public exposure process to job before process start")
	}
	processHandle, err := syscall.OpenProcess(publicProcessSetQuota|publicProcessTerminate, false, uint32(cmd.Process.Pid))
	if err != nil {
		return fmt.Errorf("failed to open public exposure process %d for job assignment: %w", cmd.Process.Pid, err)
	}
	defer syscall.CloseHandle(processHandle)

	ok, _, callErr := publicProcAssignProcessToJobObject.Call(uintptr(j.handle), uintptr(processHandle))
	if ok == 0 {
		return fmt.Errorf("failed to assign public exposure process %d to job: %w", cmd.Process.Pid, callErr)
	}
	return nil
}

func (j *publicProcessJob) close() error {
	if j == nil || j.handle == 0 {
		return nil
	}
	err := syscall.CloseHandle(j.handle)
	j.handle = 0
	if err != nil {
		return fmt.Errorf("failed to close public exposure job object: %w", err)
	}
	return nil
}

func startPublicManagedCommand(cmd *exec.Cmd, job *publicProcessJob) error {
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

func stopPublicManagedCommand(cmd *exec.Cmd) {
	if cmd.Process == nil {
		return
	}
	_ = exec.Command("taskkill", "/PID", fmt.Sprintf("%d", cmd.Process.Pid), "/T", "/F").Run()
}
