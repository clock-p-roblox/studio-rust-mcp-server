//go:build linux

package studio

import (
	"os"
	"strconv"
	"strings"
	"syscall"
	"time"
)

const linuxClockTicksPerSecond = 100

func processIsRunning(pid int) bool {
	return syscall.Kill(pid, 0) == nil
}

func processIsManagedRunning(pid int, startedAt time.Time) bool {
	if !processIsRunning(pid) {
		return false
	}
	if startedAt.IsZero() {
		return true
	}
	creationTime, ok := processStartTime(pid)
	return ok && creationTime.Equal(startedAt)
}

func processStartTime(pid int) (time.Time, bool) {
	if pid <= 0 {
		return time.Time{}, false
	}
	statBody, err := os.ReadFile("/proc/" + strconv.Itoa(pid) + "/stat")
	if err != nil {
		return time.Time{}, false
	}
	statText := string(statBody)
	endComm := strings.LastIndex(statText, ")")
	if endComm < 0 || endComm+2 >= len(statText) {
		return time.Time{}, false
	}
	fields := strings.Fields(statText[endComm+2:])
	if len(fields) < 20 {
		return time.Time{}, false
	}
	startTicks, err := strconv.ParseInt(fields[19], 10, 64)
	if err != nil {
		return time.Time{}, false
	}
	bootTime, ok := linuxBootTime()
	if !ok {
		return time.Time{}, false
	}
	seconds := startTicks / linuxClockTicksPerSecond
	nanos := (startTicks % linuxClockTicksPerSecond) * int64(time.Second) / linuxClockTicksPerSecond
	return bootTime.Add(time.Duration(seconds)*time.Second + time.Duration(nanos)).UTC(), true
}

func linuxBootTime() (time.Time, bool) {
	body, err := os.ReadFile("/proc/stat")
	if err != nil {
		return time.Time{}, false
	}
	for _, line := range strings.Split(string(body), "\n") {
		if !strings.HasPrefix(line, "btime ") {
			continue
		}
		fields := strings.Fields(line)
		if len(fields) != 2 {
			return time.Time{}, false
		}
		seconds, err := strconv.ParseInt(fields[1], 10, 64)
		if err != nil {
			return time.Time{}, false
		}
		return time.Unix(seconds, 0).UTC(), true
	}
	return time.Time{}, false
}

func processParentID(_ int) (int, bool) {
	return 0, false
}

func killProcess(pid int) error {
	if pid <= 0 {
		return nil
	}
	return syscall.Kill(pid, syscall.SIGKILL)
}
