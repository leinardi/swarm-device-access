//go:build linux

/*
 * Copyright 2026 Roberto Leinardi
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package daemon

import (
	"context"
	"fmt"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/client"

	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/processor"
	"github.com/leinardi/swarm-device-access/internal/systemd"
)

// Options bundles the dependencies required by Run.
type Options struct {
	Docker  *client.Client
	Proc    *processor.Processor
	Metrics *observability.Recorder
}

// Run enumerates existing containers, subscribes to the systemd reload signal,
// and then blocks on the Docker event stream until ctx is done.
func Run(ctx context.Context, opts Options) error {
	log := logger.L()

	processed := make(map[string]struct{})

	processErr := processExistingContainers(ctx, opts.Docker, processed, opts.Proc)
	if processErr != nil {
		log.Warn("could not enumerate existing containers", "err", processErr)
	}

	startReloadWatcher(ctx, opts)

	listenEvents(ctx, opts, processed)

	return nil
}

// processExistingContainers iterates the currently running containers and
// applies device rules to each one that bind-mounts /dev/... paths.
func processExistingContainers(
	ctx context.Context,
	cli *client.Client,
	processed map[string]struct{},
	proc *processor.Processor,
) error {
	log := logger.L()

	containers, err := cli.ContainerList(ctx, container.ListOptions{})
	if err != nil {
		return fmt.Errorf("list containers: %w", err)
	}

	log.Debug("enumerating running containers", "count", len(containers))

	for idx := range containers {
		processErr := proc.ProcessContainer(ctx, containers[idx].ID)
		if processErr != nil {
			log.Warn("could not process running container",
				"id", containers[idx].ID, "err", processErr)

			continue
		}

		processed[containers[idx].ID] = struct{}{}
	}

	return nil
}

// startReloadWatcher tries to subscribe to systemd's DBus Reloading signal so
// that when daemon-reload wipes the cgroup BPF programs, we re-apply rules to
// every running container. DBus is optional — on hosts without systemd or
// without the DBus socket mounted, this logs a warning and returns.
func startReloadWatcher(ctx context.Context, opts Options) {
	log := logger.L()

	watcher, err := systemd.Open()
	if err != nil {
		log.Warn("systemd reload handling disabled", "err", err)

		return
	}

	go func() {
		defer func() {
			closeErr := watcher.Close()
			if closeErr != nil {
				log.Warn("close systemd watcher", "err", closeErr)
			}
		}()

		watcher.Watch(ctx, func() {
			opts.Metrics.IncReloadReapply()

			fresh := make(map[string]struct{})

			processErr := processExistingContainers(ctx, opts.Docker, fresh, opts.Proc)
			if processErr != nil {
				log.Warn("could not re-apply rules after systemd reload",
					"err", processErr)
			}
		})
	}()
}
