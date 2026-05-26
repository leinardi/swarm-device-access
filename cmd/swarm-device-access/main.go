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

	"github.com/docker/docker/client"

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

	store := config.NewStore()

	// Load config file; CLI flags override file values.
	fileCfg, fileErr := config.LoadFile(*configFile)
	if fileErr != nil {
		fmt.Fprintf(os.Stderr, "config file error: %v\n", fileErr)

		return 1
	}

	applyFileConfig(&fileCfg, store)

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

	rootCtx, cancelRoot := signal.NotifyContext(
		context.Background(),
		syscall.SIGINT,
		syscall.SIGTERM,
	)
	defer cancelRoot()

	// SIGHUP: reload the config file and update hot settings + logger.
	go watchSIGHUP(rootCtx, store)

	cli, err := client.NewClientWithOpts(
		client.WithHost("unix://"+*dockerSocket),
		client.WithAPIVersionNegotiation(),
	)
	if err != nil {
		log.Error("docker client init failed", "err", err)

		return 1
	}
	defer cli.Close()

	recorder := observability.NewRecorder()

	isSwarmManager := false

	nodeInfo, infoErr := cli.Info(rootCtx)
	if infoErr != nil {
		log.Warn(
			"could not query docker info; assuming worker node (service-label inspection disabled)",
			"err",
			infoErr,
		)
	} else {
		isSwarmManager = nodeInfo.Swarm.ControlAvailable
		log.Info("swarm role detected",
			"manager", isSwarmManager,
			"local_node_state", nodeInfo.Swarm.LocalNodeState,
		)
	}

	// Start optional observability servers before the main loop so they are
	// reachable during startup enumeration.
	if *metricsAddr != "" {
		observability.StartMetricsServer(rootCtx, *metricsAddr)
	}

	if *debugAddr != "" {
		observability.StartDebugServer(rootCtx, *debugAddr)
	}

	proc := &processor.Processor{
		Inspector:      cli,
		Cfg:            store,
		Metrics:        recorder,
		HostRoot:       hostRootPath,
		ProcRoot:       "/",
		IsSwarmManager: isSwarmManager,
	}

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
