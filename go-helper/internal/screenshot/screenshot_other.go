//go:build !windows

package screenshot

import (
	"context"
	"errors"
)

type Result struct {
	Path        string `json:"path"`
	StudioPID   int    `json:"studio_pid"`
	WindowTitle string `json:"window_title"`
	Width       int    `json:"width"`
	Height      int    `json:"height"`
	Bytes       int64  `json:"bytes"`
	Fallback    bool   `json:"fallback"`
}

func CaptureStudioScreenshot(_ context.Context, _ int, _ string, _ string) (Result, error) {
	return Result{}, errors.New("studio screenshots are only available on Windows")
}
