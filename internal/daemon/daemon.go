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

package daemon

import (
	"context"
	"fmt"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/events"

	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/processor"
	"github.com/leinardi/swarm-device-access/internal/systemd"
)

// dockerAPI is the subset of *client.Client used by the daemon loop. It exists
// so tests can inject a fake event stream and container list.
type dockerAPI interface {
	ContainerList(ctx context.Context, options container.ListOptions) ([]container.Summary, error)
	Events(ctx context.Context, options events.ListOptions) (<-chan events.Message, <-chan error)
}

// Options bundles the dependencies required by Run.
type Options struct {
	Docker  dockerAPI
	Proc    *processor.Processor
	Metrics *observability.Recorder
}

// Run subscribes to the Docker event stream, enumerates existing containers,
// subscribes to the systemd reload signal, and then blocks on the event stream
// until ctx is done.
//
// The event stream is opened before the enumeration, with Since set to the
// time just before subscribing, so a container started while the list is
// being processed is still delivered as an event instead of being missed.
func Run(ctx context.Context, opts Options) error {
	return run(ctx, opts, startReloadWatcher)
}

func run(ctx context.Context, opts Options, startWatcher func(context.Context, Options)) error {
	log := logger.L()

	since := time.Now()

	// The client delivers messages on an unbuffered channel, so events that
	// arrive during the enumeration below wait (with backpressure on the
	// socket) until listenEvents starts consuming them.
	msgs, errs := opts.Docker.Events(ctx, eventListOptions(formatSince(since)))

	processed := make(map[string]time.Time)

	processErr := processExistingContainers(
		ctx,
		opts.Docker,
		processed,
		opts.Metrics,
		processorApply(opts.Proc),
	)
	if processErr != nil {
		log.Warn("could not enumerate existing containers", "err", processErr)
	}

	startWatcher(ctx, opts)

	listenEvents(ctx, opts, processed, since, msgs, errs)

	return nil
}

// processExistingContainers iterates the currently running containers and
// applies device rules to each one that bind-mounts /dev/... paths. For every
// container processed successfully, processed records the time captured just
// before it was inspected, so later events for an earlier run can be skipped.
func processExistingContainers(
	ctx context.Context,
	cli dockerAPI,
	processed map[string]time.Time,
	metrics *observability.Recorder,
	apply applyFn,
) error {
	log := logger.L()

	containers, err := cli.ContainerList(ctx, container.ListOptions{})
	if err != nil {
		return fmt.Errorf("list containers: %w", err)
	}

	log.Debug("enumerating running containers", "count", len(containers))

	for idx := range containers {
		startedAt := time.Now()

		processErr := processOne(
			ctx,
			containers[idx].ID,
			metrics,
			apply,
			"could not process running container",
		)
		if processErr != nil {
			continue
		}

		processed[containers[idx].ID] = startedAt
	}

	return nil
}

// processorApply adapts Processor.ProcessContainer to applyFn.
func processorApply(proc *processor.Processor) applyFn {
	return func(ctx context.Context, id string) error {
		return proc.ProcessContainer(ctx, id)
	}
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

			fresh := make(map[string]time.Time)

			processErr := processExistingContainers(
				ctx,
				opts.Docker,
				fresh,
				opts.Metrics,
				processorApply(opts.Proc),
			)
			if processErr != nil {
				log.Warn("could not re-apply rules after systemd reload",
					"err", processErr)
			}
		})
	}()
}
