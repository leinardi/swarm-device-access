//go:build linux

/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"io"
	"os"
	"os/signal"
	"path/filepath"
	"slices"
	"syscall"

	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/launcher"
	"github.com/leinardi/swarm-device-access/internal/logger"
)

const (
	// launchCommand is the first argument that selects launcher mode.
	launchCommand = "launch"
	// launcherComponent tags the launcher's own log records.
	launcherComponent = "launcher"
)

var (
	errPathNotAbsolute = errors.New("must be an absolute, clean path")
	errUnexpectedArg   = errors.New("unexpected argument; daemon flags go after --")
)

// launchSettings is everything the launch flags control.
type launchSettings struct {
	LogFormat string
	LogLevel  string
	Opts      launcher.Options
}

// parseLaunchArgs parses the launcher's flags. Everything after the first
// "--" is passed to the daemon verbatim. flag.ErrHelp is returned for -help.
func parseLaunchArgs(args []string, output io.Writer) (launchSettings, error) {
	daemonArgs := []string{}

	sep := slices.Index(args, "--")
	if sep >= 0 {
		daemonArgs = slices.Clone(args[sep+1:])
		args = args[:sep]
	}

	flags := flag.NewFlagSet(launchCommand, flag.ContinueOnError)
	flags.SetOutput(output)
	flags.Usage = func() {
		fmt.Fprintln(output, "Usage: swarm-device-access launch [flags] -- [daemon flags]")
		flags.PrintDefaults()
	}

	var parsed launchSettings

	flags.StringVar(
		&parsed.LogFormat,
		"log-format",
		"text",
		"Launcher log format: json, text or plain",
	)
	flags.StringVar(
		&parsed.LogLevel,
		"log-level",
		"info",
		"Launcher log level: debug, info, warn or error",
	)
	flags.StringVar(
		&parsed.Opts.HostDockerSocket,
		"host-docker-socket",
		launcher.DefaultHostDockerSocket,
		"Host path of the Docker socket bound into the daemon",
	)
	flags.BoolVar(&parsed.Opts.DBus, "dbus", false,
		"Bind the host's systemd DBus socket into the daemon, enabling daemon-reload handling")
	flags.StringVar(
		&parsed.Opts.ConfigDir,
		"config-dir",
		"",
		"Host directory bound read-only at "+launcher.ContainerConfigDir+" in the daemon. Empty = none.",
	)
	flags.BoolVar(
		&parsed.Opts.HostNetwork,
		"host-network",
		false,
		"Run the daemon in the host network (needed for -metrics-addr/-debug-addr); default is no network",
	)

	err := flags.Parse(args)
	if err != nil {
		return launchSettings{}, fmt.Errorf("launch: %w", err)
	}

	if flags.NArg() > 0 {
		return launchSettings{}, fmt.Errorf("launch: %q: %w", flags.Arg(0), errUnexpectedArg)
	}

	parsed.Opts.DaemonArgs = daemonArgs

	err = parsed.validate()
	if err != nil {
		return launchSettings{}, err
	}

	return parsed, nil
}

func (s *launchSettings) validate() error {
	err := config.ValidateEnums(s.LogFormat, s.LogLevel, "", "flag")
	if err != nil {
		return fmt.Errorf("launch: %w", err)
	}

	err = checkHostPath("host-docker-socket", s.Opts.HostDockerSocket)
	if err != nil {
		return err
	}

	if s.Opts.ConfigDir != "" {
		return checkHostPath("config-dir", s.Opts.ConfigDir)
	}

	return nil
}

// checkHostPath rejects a host path Docker would resolve differently from
// what the operator reads: a relative one, or one with . or .. elements.
func checkHostPath(name, path string) error {
	if !filepath.IsAbs(path) || filepath.Clean(path) != path {
		return fmt.Errorf("launch: flag %q: %q %w", "-"+name, path, errPathNotAbsolute)
	}

	return nil
}

// runLaunch runs launcher mode and returns the daemon's exit status, or 1
// when the launcher itself fails. It skips everything only the daemon
// needs: the config file, the openat2 probe, RLIMIT_MEMLOCK, SIGHUP and the
// observability servers.
func runLaunch(args []string) int {
	parsed, err := parseLaunchArgs(args, os.Stderr)
	if errors.Is(err, flag.ErrHelp) {
		return 0
	}

	if err != nil {
		fmt.Fprintln(os.Stderr, err)

		return 1
	}

	logger.Configure(parsed.LogFormat, parsed.LogLevel, false)

	logger.L().Info("swarm-device-access launcher starting",
		"component", launcherComponent,
		"version", version,
		"commit", commit,
		"date", date,
		"daemon_args", parsed.Opts.DaemonArgs,
	)

	ctx, cancel := signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)
	defer cancel()

	// API version negotiation is the client default; it runs lazily on the first request.
	cli, err := client.New(client.WithHost(launcher.DockerHost))
	if err != nil {
		logger.L().Error("docker client init failed", "component", launcherComponent, "err", err)

		return 1
	}
	defer cli.Close()

	status, err := launcher.Run(ctx, cli, &parsed.Opts)
	if err != nil {
		logger.L().Error("launcher failed", "component", launcherComponent, "err", err)

		return 1
	}

	return status
}
