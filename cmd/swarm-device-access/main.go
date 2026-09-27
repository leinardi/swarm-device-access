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

// Package main wires and runs the swarm-device-access daemon.
// It listens to Docker container-start events and injects cgroup v2 BPF
// device-allow rules for any container that bind-mounts a /dev/... path.
package main

import (
	"context"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"syscall"

	"github.com/cilium/ebpf/rlimit"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/daemon"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

const hostRootPath = "/host"

func main() {
	os.Exit(run())
}

// startupSettings loads the config file, merges it with the flags and checks
// the result and the kernel before anything uses them.
func startupSettings(flags *settings, cliSet map[string]bool) (settings, error) {
	// CLI flags override file values.
	fileCfg, err := config.LoadFile(*configFile)
	if err != nil {
		return settings{}, fmt.Errorf("config file error: %w", err)
	}

	effective := mergeSettings(*flags, cliSet, fileCfg)

	// The file's values were checked when it was loaded; the effective
	// values (flags included) are checked here, before anything uses them.
	err = effective.validate()
	if err != nil {
		return settings{}, fmt.Errorf("invalid config: %w", err)
	}

	// Device paths are resolved beneath /dev with openat2; without it no
	// path can be contained, so refuse to start rather than fall back.
	err = processor.ProbeOpenat2()
	if err != nil {
		return settings{}, fmt.Errorf("unsupported kernel: %w", err)
	}

	return effective, nil
}

// startServers starts the configured metrics and debug servers. A busy or
// invalid address is an error; if the second server fails, the first is
// stopped again. The returned stop shuts down whatever was started.
func startServers(ctx context.Context, effective *settings) (stop func(), err error) {
	stopMetrics := func() {}

	if effective.MetricsAddr != "" {
		stopMetrics, err = observability.StartMetricsServer(ctx, effective.MetricsAddr)
		if err != nil {
			return nil, fmt.Errorf("start metrics server: %w", err)
		}
	}

	stopDebug := func() {}

	if effective.DebugAddr != "" {
		stopDebug, err = observability.StartDebugServer(ctx, effective.DebugAddr)
		if err != nil {
			stopMetrics()

			return nil, fmt.Errorf("start debug server: %w", err)
		}
	}

	return func() {
		stopDebug()
		stopMetrics()
	}, nil
}

func run() int {
	if len(os.Args) > 1 && os.Args[1] == launchCommand {
		return runLaunch(os.Args[2:])
	}

	flag.Parse()

	if *help {
		fmt.Fprintln(flag.CommandLine.Output(),
			"Usage: swarm-device-access [flags]\n"+
				"       swarm-device-access launch [launch flags] -- [daemon flags]  (see launch -help)")
		flag.PrintDefaults()

		return 0
	}

	// Registered before anything slow, so a SIGHUP during startup is queued
	// for watchSIGHUP instead of terminating the process (its default).
	sighup := make(chan os.Signal, 1)

	signal.Notify(sighup, syscall.SIGHUP)
	defer signal.Stop(sighup)

	flags, cliSet := flagSettings()

	effective, startupErr := startupSettings(&flags, cliSet)
	if startupErr != nil {
		fmt.Fprintln(os.Stderr, startupErr)

		return 1
	}

	logger.Configure(effective.LogFormat, effective.LogLevel, effective.LogTime)

	store, publisher := config.NewStore(effective.runtime())

	cfg := store.Load()
	logger.L().Info("swarm-device-access starting",
		"version", version,
		"commit", commit,
		"date", date,
		"config_file", *configFile,
		"dry_run", cfg.DryRun,
		"policy_mode", cfg.Policy.Mode,
		"device_allow", cfg.Policy.DeviceAllow,
		"device_deny", cfg.Policy.DeviceDeny,
	)

	if cfg.DryRun {
		// Dry-run has no cgroup view and no saved ownership state, so it
		// cannot even count what a previous live run left behind.
		logger.L().Warn(
			"dry-run: grants left by a previous live run cannot be detected or cleaned in dry-run",
		)
	}

	// Lift RLIMIT_MEMLOCK once for the process so BPF_PROG_LOAD does not fail
	// on kernels that still charge BPF memory to it (no-op from Linux 5.11,
	// where memcg accounting replaced it). The limit is not inherited by
	// containers.
	memlockErr := rlimit.RemoveMemlock()
	if memlockErr != nil {
		logger.L().Warn(
			"could not remove RLIMIT_MEMLOCK; loading BPF device filters may fail",
			"err",
			memlockErr,
		)
	}

	rootCtx, cancelRoot := signal.NotifyContext(
		context.Background(),
		syscall.SIGINT,
		syscall.SIGTERM,
	)
	defer cancelRoot()

	// API version negotiation is the client default; it runs lazily on the first request.
	cli, err := client.New(client.WithHost("unix://" + effective.DockerSocket))
	if err != nil {
		logger.L().Error("docker client init failed", "err", err)

		return 1
	}
	defer cli.Close()

	recorder := observability.NewRecorder()

	// Start optional observability servers before the main loop so they are
	// reachable during startup enumeration.
	stopServers, err := startServers(rootCtx, &effective)
	if err != nil {
		logger.L().Error("could not start observability server", "err", err)

		return 1
	}
	defer stopServers()

	proc := &processor.Processor{
		Inspector:   cli,
		Cfg:         store,
		Publisher:   publisher,
		Metrics:     recorder,
		HostRoot:    hostRootPath,
		ProcRoot:    "/",
		CallTimeout: daemon.DockerCallTimeout,
	}

	// SIGHUP: reload the config file, update the logger and publish the
	// new config through the processor.
	go watchSIGHUP(rootCtx, sighup, &reloader{
		path:            *configFile,
		flags:           flags,
		cliSet:          cliSet,
		current:         effective,
		configureLogger: logger.Configure,
		publish:         proc.PublishAndReconcile,
	})

	runErr := daemon.Run(rootCtx, daemon.Options{
		Docker:  cli,
		Proc:    proc,
		Metrics: recorder,
	})
	if runErr != nil {
		logger.L().Error("daemon error", "err", runErr)

		return 1
	}

	logger.L().Info("swarm-device-access shutting down")

	return 0
}
