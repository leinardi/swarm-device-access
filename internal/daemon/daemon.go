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
	"sync"
	"time"

	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/processor"
	"github.com/leinardi/swarm-device-access/internal/systemd"
)

// dockerAPI is the subset of *client.Client used by the daemon loop. It exists
// so tests can inject a fake event stream and container list.
type dockerAPI interface {
	ContainerList(
		ctx context.Context,
		options client.ContainerListOptions,
	) (client.ContainerListResult, error)
	Events(ctx context.Context, options client.EventsListOptions) client.EventsResult
	Ping(ctx context.Context, options client.PingOptions) (client.PingResult, error)
}

// Options bundles the dependencies required by Run.
type Options struct {
	Docker  dockerAPI
	Proc    *processor.Processor
	Metrics *observability.Recorder

	// callTimeout overrides DockerCallTimeout; zero means the default. Only
	// tests set it, to exercise hung calls without waiting the full timeout.
	callTimeout time.Duration
}

func (o *Options) timeout() time.Duration {
	if o.callTimeout > 0 {
		return o.callTimeout
	}

	return DockerCallTimeout
}

// Run subscribes to the Docker event stream, reconciles every running
// container, subscribes to the systemd reload signal, and then blocks on the
// event stream until ctx is done.
//
// The event stream is opened before the enumeration, with Since set to the
// time just before subscribing, so a container started while the list is
// being processed is still delivered as an event instead of being missed.
func Run(ctx context.Context, opts Options) error {
	return run(ctx, opts, startReloadWatcher)
}

func run(
	ctx context.Context,
	opts Options,
	startWatcher func(context.Context, Options, func()),
) error {
	log := logger.L()

	since := time.Now()

	// The client delivers messages on an unbuffered channel, so events that
	// arrive during the enumeration below wait (with backpressure on the
	// socket) until listenEvents starts consuming them. A failed subscription
	// leaves stream empty; listenEvents then resubscribes from since, so the
	// events of the enumeration window are still replayed.
	stream, cancelStream, subErr := subscribe(ctx, opts.Docker, formatSince(since), opts.timeout())
	if subErr != nil {
		log.Warn("could not subscribe to docker events; will retry", "err", subErr)
	}

	coord := newCoordinator(opts.Docker, processorApply(opts.Proc), opts.Metrics, opts.timeout())

	// Every later publication goes to the coordinator; installed before
	// the startup request, so no generation published from here on is
	// missed.
	opts.Proc.SetPassRequester(coord.requestPass)

	// Waited for before Run returns, so shutdown does not cut off a
	// reconcile in the middle of its cgroup mutation.
	var coordinator sync.WaitGroup
	defer coordinator.Wait()

	coordinator.Go(func() { coord.run(ctx) })

	// The startup pass reconciles every running container under the
	// startup config. In live mode that replaces or strips grants a
	// previous instance left behind.
	coord.request(opts.Proc.Cfg.Generation())
	coord.awaitFirstPass(ctx)

	startWatcher(ctx, opts, func() {
		opts.Metrics.IncReloadReapply()
		coord.request(opts.Proc.Cfg.Generation())
	})

	listenEvents(ctx, opts, coord, since, stream, cancelStream)

	return nil
}

// processorApply adapts Processor.Reconcile to applyFn.
func processorApply(proc *processor.Processor) applyFn {
	return func(ctx context.Context, id string) error {
		return proc.Reconcile(ctx, id)
	}
}

// startReloadWatcher tries to subscribe to systemd's DBus Reloading signal so
// that when daemon-reload wipes the cgroup BPF programs, trigger requests a
// pass that re-applies rules to every running container. DBus is optional —
// on hosts without systemd or without the DBus socket mounted, this logs a
// warning and returns.
func startReloadWatcher(ctx context.Context, _ Options, trigger func()) {
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

		watcher.Watch(ctx, trigger)
	}()
}
