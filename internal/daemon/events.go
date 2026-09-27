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
	"errors"
	"fmt"
	"time"

	"github.com/moby/moby/api/types/events"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
)

const (
	minBackoff = 1 * time.Second
	maxBackoff = 30 * time.Second

	// DockerCallTimeout bounds every request/response Docker API call and the
	// establishment of the event stream. Without it a wedged dockerd blocks the
	// caller forever, and nothing behind it (retries, reconnects, shutdown)
	// ever runs.
	DockerCallTimeout = 10 * time.Second
)

var errSubscribeTimeout = errors.New("docker event stream not established before timeout")

// applyFn is the per-container reconcile the coordinator runs. In
// production it wraps Processor.Reconcile; in tests it is replaced by a fake.
type applyFn func(ctx context.Context, id string) error

// eventListOptions returns the Docker event subscription options: "start"
// and "unpause" container events, which reconcile a container, and "die"
// and "destroy", which end a run and clean up after it, at or after since.
func eventListOptions(since string) client.EventsListOptions {
	return client.EventsListOptions{
		Since: since,
		// make, not the zero value: a nil client.Filters panics on Add.
		Filters: make(client.Filters).Add("event", "start", "unpause", "die", "destroy"),
	}
}

// formatSince formats a timestamp for client.EventsListOptions.Since.
func formatSince(since time.Time) string {
	return since.UTC().Format(time.RFC3339Nano)
}

// resubscribeSince returns the Since value for a re-subscription: one
// nanosecond after the last event received, so neither the events emitted
// during the disconnect nor the last event itself are replayed (a replayed
// event would re-apply rules, and cgroup v2 programs grow on every apply).
// Before any event has been received it falls back to the initial since.
func resubscribeSince(initial time.Time, lastEventNano int64) string {
	if lastEventNano == 0 {
		return formatSince(initial)
	}

	return formatSince(time.Unix(0, lastEventNano+1))
}

// listenEvents consumes Docker container events and applies device rules.
// Every re-subscription after the initial stream requests a coordinator
// pass (see requestReenumeration).
// "start" covers fresh starts and restart's second phase; "unpause" covers
// resume from a paused state if the cgroup state was cleared. stream is the
// stream already opened by Run before the startup enumeration (zero value when
// that subscription failed) and cancelStream releases it. On stream error it
// reconnects with exponential backoff (capped) rather than terminating the
// daemon — replaces the upstream log.Fatal(err) pattern.
//
// /readyz reflects live Docker event-stream health via observability.SetReady.
func listenEvents(
	ctx context.Context,
	opts Options,
	coord *coordinator,
	since time.Time,
	stream client.EventsResult,
	cancelStream context.CancelFunc,
) {
	backoff := minBackoff
	timeout := opts.timeout()

	var lastEventNano int64

	for {
		if ctx.Err() != nil {
			cancelStream()

			return
		}

		if stream.Messages == nil {
			var subErr error

			stream, cancelStream, subErr = subscribe(
				ctx,
				opts.Docker,
				resubscribeSince(since, lastEventNano),
				timeout,
			)
			if subErr != nil {
				if ctx.Err() != nil {
					return
				}

				logger.L().Error("could not subscribe to docker events, retrying",
					"err", subErr, "backoff", backoff)
				opts.Metrics.IncDockerReconnect()
				observability.SetReady(false)
				sleepCtx(ctx, backoff)
				backoff = nextBackoff(backoff)

				continue
			}

			// Since only replays what dockerd still buffers, and a
			// restarted dockerd has nothing buffered, so events of the
			// gap can be lost: re-list and reconcile every running
			// container. The coordinator's processed map skips replayed
			// start events the pass already covered.
			logger.L().Info("docker event stream re-subscribed; reconciling running containers")
			coord.requestReenumeration()
		}

		observability.SetReady(true)
		logger.L().Debug("subscribed to docker events")

		disconnected := consumeEvents(
			ctx,
			stream.Messages,
			stream.Err,
			coord,
			&backoff,
			&lastEventNano,
			opts.Metrics,
		)

		// The stream context lives exactly as long as this subscription: a
		// reconnect opens a new one and shutdown must release the request.
		cancelStream()

		if !disconnected {
			return
		}

		stream = client.EventsResult{}

		observability.SetReady(false)
	}
}

// subscribe opens the Docker event stream with a bounded establishment.
//
// The client's Events call does not return until dockerd has sent the
// response headers, so a hung dockerd would block the caller with nothing
// able to act as a watchdog, and a deadline on the stream context would also
// kill the stream once established. Events therefore runs in a goroutine on a
// cancelable context and is raced against a timer here; on timeout the
// request is canceled and the goroutine is waited for, so it never leaks. On
// success the returned cancel func owns the stream context for its lifetime.
func subscribe(
	ctx context.Context,
	docker dockerAPI,
	since string,
	timeout time.Duration,
) (client.EventsResult, context.CancelFunc, error) {
	pingCtx, cancelPing := context.WithTimeout(ctx, timeout)
	_, pingErr := docker.Ping(pingCtx, client.PingOptions{})

	cancelPing()

	if pingErr != nil {
		return client.EventsResult{}, func() {}, fmt.Errorf("ping docker: %w", pingErr)
	}

	streamCtx, cancel := context.WithCancel(ctx)
	established := make(chan client.EventsResult, 1)

	go func() {
		established <- docker.Events(streamCtx, eventListOptions(since))
	}()

	timer := time.NewTimer(timeout)
	defer timer.Stop()

	select {
	case stream := <-established:
		return stream, cancel, nil
	case <-timer.C:
		cancel()
		<-established

		return client.EventsResult{}, func() {}, errSubscribeTimeout
	case <-ctx.Done():
		cancel()
		<-established

		return client.EventsResult{}, func() {}, fmt.Errorf(
			"subscribe to docker events: %w",
			ctx.Err(),
		)
	}
}

// consumeEvents drains one events.Subscribe lifecycle. Returns true if the
// caller should reconnect, false on context cancellation. lastEventNano is
// updated with the timestamp of every received event.
//
// An event already covered by an enumeration is skipped (see
// coordinator.skipEvent); a newer one (restart, unpause) has a new cgroup and
// is applied. A failed apply leaves the container to the coordinator's
// retries.
func consumeEvents(
	ctx context.Context,
	msgs <-chan events.Message,
	errs <-chan error,
	coord *coordinator,
	backoff *time.Duration,
	lastEventNano *int64,
	metrics *observability.Recorder,
) bool {
	for {
		select {
		case <-ctx.Done():
			return false

		case streamErr := <-errs:
			if streamErr == nil {
				continue
			}

			// Checked before the error itself: once ctx is canceled the client
			// can end the stream with a closed-body read error rather than
			// context.Canceled, and that is a shutdown, not a stream failure.
			if ctx.Err() != nil {
				return false
			}

			if errors.Is(streamErr, context.Canceled) ||
				errors.Is(streamErr, context.DeadlineExceeded) {
				return false
			}

			logger.L().Error("docker events stream error, reconnecting",
				"err", streamErr, "backoff", *backoff)
			metrics.IncDockerReconnect()
			observability.SetReady(false)
			sleepCtx(ctx, *backoff)
			*backoff = nextBackoff(*backoff)

			return ctx.Err() == nil

		case msg, ok := <-msgs:
			if !ok {
				logger.L().Warn("docker events channel closed, reconnecting",
					"backoff", *backoff)
				metrics.IncDockerReconnect()
				observability.SetReady(false)
				sleepCtx(ctx, *backoff)
				*backoff = nextBackoff(*backoff)

				return ctx.Err() == nil
			}

			*backoff = minBackoff

			if msg.TimeNano > *lastEventNano {
				*lastEventNano = msg.TimeNano
			}

			metrics.RecordEvent(string(msg.Action))

			coord.handleEvent(ctx, &msg)
		}
	}
}

// processOne applies device rules to one container and records the outcome:
// rules-applied result, apply duration, last-event timestamp on success, and
// a WARN with logMsg on failure. It is shared by the startup enumeration and
// the event stream so both paths report identically. The apply runs under a
// per-container timeout so a hung Docker call made while processing (inspect)
// cannot stall the enumeration or the event loop behind it.
func processOne(
	ctx context.Context,
	containerID string,
	metrics *observability.Recorder,
	apply applyFn,
	timeout time.Duration,
	logMsg string,
) error {
	start := time.Now()

	applyCtx, cancel := context.WithTimeout(ctx, timeout)
	defer cancel()

	applyErr := apply(applyCtx, containerID)
	if applyErr != nil {
		logger.L().Warn(logMsg, "id", containerID, "err", applyErr)
		metrics.RecordRuleApplied(false)
	} else {
		metrics.RecordRuleApplied(true)
		metrics.SetLastEvent(time.Now())
	}

	metrics.ObserveApplyDuration(time.Since(start))

	return applyErr
}

func nextBackoff(current time.Duration) time.Duration {
	next := current * 2
	if next > maxBackoff {
		return maxBackoff
	}

	return next
}

func sleepCtx(ctx context.Context, duration time.Duration) {
	select {
	case <-ctx.Done():
	case <-time.After(duration):
	}
}
