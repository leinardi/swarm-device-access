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

func run() int {
	flag.Parse()

	if *help {
		flag.PrintDefaults()

		return 0
	}

	// Load config file; CLI flags override file values.
	fileCfg, fileErr := config.LoadFile(*configFile)
	if fileErr != nil {
		fmt.Fprintf(os.Stderr, "config file error: %v\n", fileErr)

		return 1
	}

	store, publisher := config.NewStore(applyFileConfig(&fileCfg))

	// Registered before anything slow, so a SIGHUP during startup is queued
	// for watchSIGHUP instead of terminating the process (its default).
	sighup := make(chan os.Signal, 1)

	signal.Notify(sighup, syscall.SIGHUP)
	defer signal.Stop(sighup)

	startupValidationErr := store.Load().Policy.Validate()
	if startupValidationErr != nil {
		fmt.Fprintf(os.Stderr, "invalid config: %v\n", startupValidationErr)

		return 1
	}

	logger.Configure(*logFormat, *logLevel, *logTime)

	log := logger.L()

	cfg := store.Load()
	log.Info("swarm-device-access starting",
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
		log.Warn(
			"dry-run: grants left by a previous live run cannot be detected or cleaned in dry-run",
		)
	}

	// Lift RLIMIT_MEMLOCK once for the process so BPF_PROG_LOAD does not fail
	// on kernels that still charge BPF memory to it (no-op from Linux 5.11,
	// where memcg accounting replaced it). The limit is not inherited by
	// containers.
	memlockErr := rlimit.RemoveMemlock()
	if memlockErr != nil {
		log.Warn(
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
	cli, err := client.New(client.WithHost("unix://" + *dockerSocket))
	if err != nil {
		log.Error("docker client init failed", "err", err)

		return 1
	}
	defer cli.Close()

	recorder := observability.NewRecorder()

	// Start optional observability servers before the main loop so they are
	// reachable during startup enumeration.
	if *metricsAddr != "" {
		observability.StartMetricsServer(rootCtx, *metricsAddr)
	}

	if *debugAddr != "" {
		observability.StartDebugServer(rootCtx, *debugAddr)
	}

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
	go watchSIGHUP(rootCtx, sighup, proc)

	runErr := daemon.Run(rootCtx, daemon.Options{
		Docker:  cli,
		Proc:    proc,
		Metrics: recorder,
	})
	if runErr != nil {
		log.Error("daemon error", "err", runErr)

		return 1
	}

	log.Info("swarm-device-access shutting down")

	return 0
}
