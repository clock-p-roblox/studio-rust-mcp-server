//go:build !windows

package studio

import "os/exec"

type processJob struct{}

func newProcessJob() (*processJob, error) {
	return &processJob{}, nil
}

func (j *processJob) assign(cmd *exec.Cmd) error {
	return nil
}

func (j *processJob) close() error {
	return nil
}
