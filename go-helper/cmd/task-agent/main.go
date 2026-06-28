package main

import (
	"context"
	"encoding/json"
	"errors"
	"flag"
	"fmt"
	"log/slog"
	"net/http"
	"os"
	"os/signal"
	"strings"
	"syscall"
	"time"

	"github.com/clock-p-roblox/studio-rust-mcp-server/go-helper/internal/taskagent"
)

func main() {
	if err := run(os.Args[1:]); err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
}

func run(args []string) error {
	if len(args) == 0 {
		return errors.New("usage: task-agent start|status|stop")
	}
	switch args[0] {
	case "start":
		return runStart(args[1:])
	case "status":
		return runStatus(args[1:])
	case "stop":
		return runStop(args[1:])
	default:
		return fmt.Errorf("unknown command %q; expected start, status, or stop", args[0])
	}
}

func runStart(args []string) error {
	fs := flag.NewFlagSet("start", flag.ExitOnError)
	workspace := fs.String("workspace", ".", "workspace directory")
	environment := fs.String("environment", "local", "task-agent environment: local or public")
	machineName := fs.String("machine_name", "", "explicit helper machine name")
	userName := fs.String("user", "", "clock-p user name for public helper URL derivation")
	placeID := fs.String("place_id", "", "Roblox place id")
	helperBaseURL := fs.String("helper-base-url", "", "local helper2 base URL")
	rojoBin := fs.String("rojo-bin", "rojo", "Rojo executable")
	projectPath := fs.String("project", "", "Rojo project path")
	statusAddr := fs.String("status-addr", "127.0.0.1:0", "task-agent status listen address")
	clockbridgeBin := fs.String("clockbridge-bin", "clockbridge-cli", "clockbridge CLI executable for public Rojo exposure")
	clockbridgeTokenFile := fs.String("clockbridge-token-file", "", "clockbridge bearer token file for public mode")
	clockbridgeRegisterIP := fs.String("clockbridge-register-ip", "", "optional clockbridge register IP")
	if err := fs.Parse(args); err != nil {
		return err
	}
	if *machineName == "" {
		return errors.New("--machine_name is required")
	}
	if *placeID == "" {
		return errors.New("--place_id is required")
	}
	resolvedEnvironment := strings.TrimSpace(*environment)
	if resolvedEnvironment == "" {
		resolvedEnvironment = "local"
	}
	resolvedHelperBaseURL, resolvedHelperPublicURL, err := taskagent.ResolveHelperBaseURL(taskagent.RouteConfig{
		Environment:   resolvedEnvironment,
		MachineName:   *machineName,
		UserName:      *userName,
		HelperBaseURL: *helperBaseURL,
	})
	if err != nil {
		return err
	}
	resolvedUserName := ""
	resolvedTokenFile := ""
	if resolvedEnvironment == "public" {
		resolvedUserName, err = taskagent.ResolveUserName(*userName)
		if err != nil {
			return err
		}
		resolvedTokenFile, err = taskagent.ResolveTokenFile(*clockbridgeTokenFile)
		if err != nil {
			return err
		}
	}

	client := &http.Client{Timeout: 3 * time.Second}
	if descriptor, err := taskagent.LoadDescriptor(*workspace); err == nil {
		if stopped, err := taskagent.RequestExistingShutdown(client, descriptor, 2*time.Second); err == nil && stopped {
			if err := waitUntilStopped(client, descriptor.TaskAgentStatusURL, 5*time.Second); err != nil {
				return err
			}
		} else if err != nil {
			if errors.Is(err, taskagent.ErrDescriptorIdentityMismatch) {
				return err
			}
			_ = taskagent.RemoveVolatileDescriptor(*workspace)
		}
	} else if !errors.Is(err, os.ErrNotExist) {
		return err
	}

	logger := slog.New(slog.NewTextHandler(os.Stderr, &slog.HandlerOptions{Level: slog.LevelInfo}))
	agent, err := taskagent.New(taskagent.Config{
		Workspace:             *workspace,
		Environment:           resolvedEnvironment,
		MachineName:           *machineName,
		UserName:              resolvedUserName,
		PlaceID:               *placeID,
		HelperBaseURL:         resolvedHelperBaseURL,
		HelperPublicURL:       resolvedHelperPublicURL,
		RojoBin:               *rojoBin,
		ProjectPath:           *projectPath,
		StatusAddr:            *statusAddr,
		ClockbridgeBin:        *clockbridgeBin,
		ClockbridgeTokenFile:  resolvedTokenFile,
		ClockbridgeRegisterIP: *clockbridgeRegisterIP,
	}, logger)
	if err != nil {
		return err
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	return agent.Run(ctx)
}

func runStatus(args []string) error {
	fs := flag.NewFlagSet("status", flag.ExitOnError)
	workspace := fs.String("workspace", ".", "workspace directory")
	if err := fs.Parse(args); err != nil {
		return err
	}
	descriptor, err := taskagent.LoadDescriptor(*workspace)
	if err != nil {
		return err
	}
	status, err := taskagent.FetchStatus(&http.Client{Timeout: 3 * time.Second}, descriptor.TaskAgentStatusURL, 3*time.Second)
	if err != nil {
		return err
	}
	return writePrettyJSON(status)
}

func runStop(args []string) error {
	fs := flag.NewFlagSet("stop", flag.ExitOnError)
	workspace := fs.String("workspace", ".", "workspace directory")
	if err := fs.Parse(args); err != nil {
		return err
	}
	descriptor, err := taskagent.LoadDescriptor(*workspace)
	if err != nil {
		return err
	}
	stopped, err := taskagent.RequestExistingShutdown(&http.Client{Timeout: 3 * time.Second}, descriptor, 3*time.Second)
	if err != nil {
		return err
	}
	return writePrettyJSON(map[string]any{
		"ok":      stopped,
		"task_id": descriptor.TaskID,
	})
}

func writePrettyJSON(value any) error {
	body, err := json.MarshalIndent(value, "", "  ")
	if err != nil {
		return err
	}
	body = append(body, '\n')
	_, err = os.Stdout.Write(body)
	return err
}

func waitUntilStopped(client *http.Client, statusURL string, timeout time.Duration) error {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		if _, err := taskagent.FetchStatus(client, statusURL, time.Second); err != nil {
			return nil
		}
		time.Sleep(100 * time.Millisecond)
	}
	return fmt.Errorf("old task-agent did not stop within %s", timeout)
}
