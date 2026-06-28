//go:build windows

package screenshot

import (
	"context"
	"encoding/base64"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"path/filepath"
	"sort"
	"strings"
	"syscall"
	"time"
	"unsafe"
)

const (
	swmRestore = 9
)

var (
	user32                  = syscall.NewLazyDLL("user32.dll")
	procEnumWindows         = user32.NewProc("EnumWindows")
	procEnumChildWindows    = user32.NewProc("EnumChildWindows")
	procGetWindowThreadPID  = user32.NewProc("GetWindowThreadProcessId")
	procIsWindowVisible     = user32.NewProc("IsWindowVisible")
	procGetWindowRect       = user32.NewProc("GetWindowRect")
	procGetClientRect       = user32.NewProc("GetClientRect")
	procGetWindowTextLength = user32.NewProc("GetWindowTextLengthW")
	procGetWindowText       = user32.NewProc("GetWindowTextW")
)

type rect struct {
	Left   int32
	Top    int32
	Right  int32
	Bottom int32
}

type Result struct {
	Path        string `json:"path"`
	StudioPID   int    `json:"studio_pid"`
	WindowTitle string `json:"window_title"`
	Width       int    `json:"width"`
	Height      int    `json:"height"`
	Bytes       int64  `json:"bytes"`
	Fallback    bool   `json:"fallback"`
}

type windowCandidate struct {
	hwnd       uintptr
	processID  int
	title      string
	x          int
	y          int
	width      int
	height     int
	isFallback bool
}

type childWindowCandidate struct {
	hwnd   uintptr
	x      int
	y      int
	width  int
	height int
}

type topWindowSearch struct {
	candidates []windowCandidate
}

type childWindowSearch struct {
	minWidth   int
	minHeight  int
	candidates []childWindowCandidate
}

func CaptureStudioScreenshot(ctx context.Context, studioPID int, outputDir string, filePrefix string) (Result, error) {
	return captureStudioScreenshot(ctx, studioPID, outputDir, filePrefix, true)
}

func CaptureStudioScreenshotForExactPID(ctx context.Context, studioPID int, outputDir string, filePrefix string) (Result, error) {
	return captureStudioScreenshot(ctx, studioPID, outputDir, filePrefix, false)
}

func captureStudioScreenshot(ctx context.Context, studioPID int, outputDir string, filePrefix string, allowFallback bool) (Result, error) {
	if studioPID <= 0 {
		return Result{}, fmt.Errorf("studio pid must be positive: %d", studioPID)
	}
	if strings.TrimSpace(outputDir) == "" {
		dir, err := defaultOutputDir()
		if err != nil {
			return Result{}, err
		}
		outputDir = dir
	}
	if strings.TrimSpace(filePrefix) == "" {
		filePrefix = "studio"
	}

	studioWindow, viewport, err := findStudioCaptureTarget(studioPID, allowFallback)
	if err != nil {
		return Result{}, err
	}
	width, height, err := clientSize(viewport.hwnd)
	if err != nil {
		return Result{}, err
	}
	if width <= 0 || height <= 0 {
		return Result{}, errors.New("selected Studio viewport has invalid bounds")
	}

	bytes, err := captureWindowPNG(ctx, studioWindow.hwnd, viewport.hwnd, width, height)
	if err != nil {
		return Result{}, err
	}
	if len(bytes) == 0 {
		return Result{}, errors.New("screenshot command returned empty png")
	}
	if err := os.MkdirAll(outputDir, 0o755); err != nil {
		return Result{}, err
	}
	path := filepath.Join(outputDir, fmt.Sprintf("%s-%s.png", filePrefix, timestamp()))
	if err := os.WriteFile(path, bytes, 0o644); err != nil {
		return Result{}, err
	}

	return Result{
		Path:        path,
		StudioPID:   studioWindow.processID,
		WindowTitle: studioWindow.title,
		Width:       width,
		Height:      height,
		Bytes:       int64(len(bytes)),
		Fallback:    studioWindow.isFallback,
	}, nil
}

func defaultOutputDir() (string, error) {
	appData := os.Getenv("APPDATA")
	if strings.TrimSpace(appData) == "" {
		home := os.Getenv("USERPROFILE")
		if strings.TrimSpace(home) == "" {
			return "", errors.New("APPDATA and USERPROFILE are not set")
		}
		return filepath.Join(home, ".dev.clock-p.com", "studio-helper2", "screenshots"), nil
	}
	return filepath.Join(appData, "dev.clock-p.com", "studio-helper2", "screenshots"), nil
}

func timestamp() string {
	return time.Now().Format("20060102-150405.000")
}

func findStudioCaptureTarget(studioPID int, allowFallback bool) (windowCandidate, childWindowCandidate, error) {
	allCandidates := collectTopWindows()
	exactCandidates := make([]windowCandidate, 0)
	for _, candidate := range allCandidates {
		if candidate.processID == studioPID {
			exactCandidates = append(exactCandidates, candidate)
		}
	}
	var studioWindow windowCandidate
	if len(exactCandidates) > 0 {
		sort.Slice(exactCandidates, func(i, j int) bool {
			return topWindowScore(exactCandidates[i]) > topWindowScore(exactCandidates[j])
		})
		studioWindow = exactCandidates[0]
	} else if allowFallback {
		studioNamed := make([]windowCandidate, 0)
		for _, candidate := range allCandidates {
			if strings.Contains(candidate.title, "Roblox Studio") {
				studioNamed = append(studioNamed, candidate)
			}
		}
		if len(studioNamed) != 1 {
			if len(studioNamed) == 0 {
				return windowCandidate{}, childWindowCandidate{}, fmt.Errorf("could not find any visible Roblox Studio window while resolving pid %d", studioPID)
			}
			parts := make([]string, 0, len(studioNamed))
			for _, candidate := range studioNamed {
				parts = append(parts, fmt.Sprintf("pid=%d title=%s", candidate.processID, candidate.title))
			}
			return windowCandidate{}, childWindowCandidate{}, fmt.Errorf("could not find exact visible Roblox Studio window for pid %d; visible Studio windows: %s", studioPID, strings.Join(parts, " | "))
		}
		studioWindow = studioNamed[0]
		studioWindow.isFallback = true
	} else {
		return windowCandidate{}, childWindowCandidate{}, fmt.Errorf("could not find exact visible Roblox Studio window for pid %d", studioPID)
	}

	descendants := collectVisibleChildWindows(studioWindow.hwnd, 500, 400)
	viewport := selectViewportWindow(studioWindow, descendants)
	return studioWindow, viewport, nil
}

func selectViewportWindow(studioWindow windowCandidate, candidates []childWindowCandidate) childWindowCandidate {
	viewport := childWindowCandidate{
		hwnd:   studioWindow.hwnd,
		x:      studioWindow.x,
		y:      studioWindow.y,
		width:  studioWindow.width,
		height: studioWindow.height,
	}
	fullWidthThreshold := maxInt(studioWindow.width-200, 1)
	fullHeightThreshold := maxInt(studioWindow.height-200, 1)
	centeredCandidates := make([]childWindowCandidate, 0)
	legacyCandidates := make([]childWindowCandidate, 0)
	generalCandidates := make([]childWindowCandidate, 0)
	for _, candidate := range candidates {
		if candidate.width <= 1000 || candidate.height <= 500 {
			continue
		}
		if candidate.width < 1660 && candidate.height < 680 {
			legacyCandidates = append(legacyCandidates, candidate)
		}
		if candidate.width < fullWidthThreshold && candidate.height < fullHeightThreshold {
			generalCandidates = append(generalCandidates, candidate)
			if candidate.x > studioWindow.x+100 && candidate.y > studioWindow.y+80 {
				centeredCandidates = append(centeredCandidates, candidate)
			}
		}
	}
	if len(centeredCandidates) > 0 {
		return largestChildWindow(centeredCandidates)
	}
	if len(legacyCandidates) > 0 {
		return largestChildWindow(legacyCandidates)
	}
	if len(generalCandidates) > 0 {
		return largestChildWindow(generalCandidates)
	}
	return viewport
}

func largestChildWindow(candidates []childWindowCandidate) childWindowCandidate {
	sort.Slice(candidates, func(i, j int) bool {
		return area(candidates[i].width, candidates[i].height) > area(candidates[j].width, candidates[j].height)
	})
	return candidates[0]
}

func topWindowScore(candidate windowCandidate) int64 {
	titleScore := int64(0)
	if strings.Contains(candidate.title, "Roblox Studio") {
		titleScore = 1
	}
	return (titleScore << 62) + area(candidate.width, candidate.height)
}

func area(width int, height int) int64 {
	return int64(width) * int64(height)
}

func collectTopWindows() []windowCandidate {
	search := &topWindowSearch{}
	procEnumWindows.Call(syscall.NewCallback(enumTopWindowsCallback), uintptr(unsafe.Pointer(search)))
	return search.candidates
}

func enumTopWindowsCallback(hwnd uintptr, lparam uintptr) uintptr {
	search := (*topWindowSearch)(unsafe.Pointer(lparam))
	visible, _, _ := procIsWindowVisible.Call(hwnd)
	if visible == 0 {
		return 1
	}

	var pid uint32
	procGetWindowThreadPID.Call(hwnd, uintptr(unsafe.Pointer(&pid)))
	width, height := windowRectSize(hwnd)
	title := windowText(hwnd)
	search.candidates = append(search.candidates, windowCandidate{
		hwnd:      hwnd,
		processID: int(pid),
		title:     title,
		x:         int(rectLeft(hwnd)),
		y:         int(rectTop(hwnd)),
		width:     width,
		height:    height,
	})
	return 1
}

func collectVisibleChildWindows(parent uintptr, minWidth int, minHeight int) []childWindowCandidate {
	search := &childWindowSearch{
		minWidth:  minWidth,
		minHeight: minHeight,
	}
	procEnumChildWindows.Call(parent, syscall.NewCallback(enumChildWindowsCallback), uintptr(unsafe.Pointer(search)))
	return search.candidates
}

func enumChildWindowsCallback(hwnd uintptr, lparam uintptr) uintptr {
	search := (*childWindowSearch)(unsafe.Pointer(lparam))
	visible, _, _ := procIsWindowVisible.Call(hwnd)
	if visible == 0 {
		return 1
	}
	width, height, err := clientSize(hwnd)
	if err != nil {
		return 1
	}
	x, y := windowPosition(hwnd)
	if width < search.minWidth || height < search.minHeight {
		return 1
	}
	search.candidates = append(search.candidates, childWindowCandidate{
		hwnd:   hwnd,
		x:      x,
		y:      y,
		width:  width,
		height: height,
	})
	return 1
}

func windowRectSize(hwnd uintptr) (int, int) {
	var rect rect
	ok, _, _ := procGetWindowRect.Call(hwnd, uintptr(unsafe.Pointer(&rect)))
	if ok == 0 {
		ok, _, _ = procGetClientRect.Call(hwnd, uintptr(unsafe.Pointer(&rect)))
	}
	if ok == 0 {
		return 0, 0
	}
	return maxInt(int(rect.Right-rect.Left), 0), maxInt(int(rect.Bottom-rect.Top), 0)
}

func windowPosition(hwnd uintptr) (int, int) {
	var rect rect
	ok, _, _ := procGetWindowRect.Call(hwnd, uintptr(unsafe.Pointer(&rect)))
	if ok == 0 {
		return 0, 0
	}
	return int(rect.Left), int(rect.Top)
}

func rectLeft(hwnd uintptr) int {
	x, _ := windowPosition(hwnd)
	return x
}

func rectTop(hwnd uintptr) int {
	_, y := windowPosition(hwnd)
	return y
}

func clientSize(hwnd uintptr) (int, int, error) {
	var rect rect
	ok, _, _ := procGetClientRect.Call(hwnd, uintptr(unsafe.Pointer(&rect)))
	if ok == 0 {
		return 0, 0, errors.New("GetClientRect failed")
	}
	return maxInt(int(rect.Right-rect.Left), 0), maxInt(int(rect.Bottom-rect.Top), 0), nil
}

func windowText(hwnd uintptr) string {
	length, _, _ := procGetWindowTextLength.Call(hwnd)
	if length == 0 {
		return ""
	}
	buffer := make([]uint16, int(length)+1)
	copied, _, _ := procGetWindowText.Call(hwnd, uintptr(unsafe.Pointer(&buffer[0])), uintptr(len(buffer)))
	if copied == 0 {
		return ""
	}
	return syscall.UTF16ToString(buffer[:copied])
}

func captureWindowPNG(ctx context.Context, studioHwnd uintptr, viewportHwnd uintptr, width int, height int) ([]byte, error) {
	script := fmt.Sprintf(`$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class Win32 {
    [DllImport("user32.dll")]
    public static extern bool PrintWindow(IntPtr hwnd, IntPtr hdcBlt, int nFlags);

    [DllImport("user32.dll")]
    public static extern bool IsIconic(IntPtr hWnd);

    [DllImport("user32.dll")]
    public static extern bool ShowWindow(IntPtr hWnd, int nCmdShow);

    [DllImport("user32.dll")]
    public static extern bool SetForegroundWindow(IntPtr hWnd);
}
"@

$studioHwnd = [IntPtr]::new(%d)
$viewportHwnd = [IntPtr]::new(%d)
$width = %d
$height = %d

function Capture-Window {
    param($windowHandle, $w, $h)
    $bitmap = New-Object System.Drawing.Bitmap $w, $h
    $graphics = [System.Drawing.Graphics]::FromImage($bitmap)
    $hdc = $graphics.GetHdc()
    $ok = [Win32]::PrintWindow($windowHandle, $hdc, 2)
    $graphics.ReleaseHdc($hdc)
    return @{
        ok = $ok
        bitmap = $bitmap
        graphics = $graphics
    }
}

$result = Capture-Window $viewportHwnd $width $height
$sampleX = [Math]::Min(10, [Math]::Max($width - 1, 0))
$sampleY = [Math]::Min(10, [Math]::Max($height - 1, 0))
$black = [System.Drawing.Color]::Black.ToArgb()
if (-not $result.ok -or $result.bitmap.GetPixel($sampleX, $sampleY).ToArgb() -eq $black) {
    if ([Win32]::IsIconic($studioHwnd)) {
        [Win32]::ShowWindow($studioHwnd, %d) | Out-Null
    }
    [Win32]::SetForegroundWindow($studioHwnd) | Out-Null
    Start-Sleep -Milliseconds 300
    $result.graphics.Dispose()
    $result.bitmap.Dispose()
    $result = Capture-Window $viewportHwnd $width $height
}

if (-not $result.ok) {
    throw 'PrintWindow failed for the selected Studio viewport'
}

$bitmap = New-Object System.Drawing.Bitmap $width, $height
$graphics = [System.Drawing.Graphics]::FromImage($bitmap)
$graphics.DrawImage($result.bitmap, 0, 0)
$memory = New-Object System.IO.MemoryStream
$bitmap.Save($memory, [System.Drawing.Imaging.ImageFormat]::Png)
$bytes = $memory.ToArray()
$encoded = [Convert]::ToBase64String($bytes)
[Console]::OutputEncoding = [System.Text.Encoding]::ASCII
[Console]::Write($encoded)
$result.graphics.Dispose()
$result.bitmap.Dispose()
$graphics.Dispose()
$bitmap.Dispose()
$memory.Dispose()
`, studioHwnd, viewportHwnd, width, height, swmRestore)

	runCtx, cancel := context.WithTimeout(ctx, 20*time.Second)
	defer cancel()
	output, err := exec.CommandContext(runCtx, "powershell.exe", "-NoProfile", "-NonInteractive", "-Command", script).CombinedOutput()
	if runCtx.Err() != nil {
		return nil, runCtx.Err()
	}
	if err != nil {
		return nil, fmt.Errorf("screenshot command failed: %w: %s", err, strings.TrimSpace(string(output)))
	}
	decoded, err := base64.StdEncoding.DecodeString(strings.TrimSpace(string(output)))
	if err != nil {
		return nil, fmt.Errorf("screenshot command returned invalid base64: %w", err)
	}
	return decoded, nil
}

func maxInt(a int, b int) int {
	if a > b {
		return a
	}
	return b
}
