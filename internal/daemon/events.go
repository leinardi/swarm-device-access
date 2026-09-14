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
	"time"

	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/filters"

	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
)

const (
	minBackoff = 1 * time.Second
	maxBackoff = 30 * time.Second
)

// applyFn is the per-container rule-application callback injected into consumeEvents.
// In production this wraps Processor.ProcessContainer; in tests it is replaced by a fake.
type applyFn func(ctx context.Context, id string) error

// eventListOptions returns the Docker event subscription options: "start"
// and "unpause" container events at or after since.
func eventListOptions(since string) events.ListOptions {
	return events.ListOptions{
		Since: since,
		Filters: filters.NewArgs(
			filters.Arg("event", "start"),
			filters.Arg("event", "unpause"),
		),
	}
}

// formatSince formats a timestamp for events.ListOptions.Since.
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
// "start" covers fresh starts and restart's second phase; "unpause" covers
// resume from a paused state if the cgroup state was cleared. msgs and errs
// are the stream already opened by Run before the startup enumeration. On
// stream error it reconnects with exponential backoff (capped) rather than
// terminating the daemon — replaces the upstream log.Fatal(err) pattern.
//
// /readyz reflects live Docker event-stream health via observability.SetReady.
func listenEvents(
	ctx context.Context,
	opts Options,
	processed map[string]time.Time,
	since time.Time,
	msgs <-chan events.Message,
	errs <-chan error,
) {
	log := logger.L()
	backoff := minBackoff

	var lastEventNano int64

	// The processed map guards the overlap window between startup enumeration
	// and the live event stream. After 2×maxBackoff (60s) the window has
	// certainly passed; any remaining entries are from containers that exited
	// before producing a start event and will never be drained normally.
	clearProcessed := time.After(2 * maxBackoff)

	for {
		if ctx.Err() != nil {
			return
		}

		if msgs == nil {
			msgs, errs = opts.Docker.Events(
				ctx,
				eventListOptions(resubscribeSince(since, lastEventNano)),
			)
		}

		observability.SetReady(true)
		log.Debug("subscribed to docker events")

		disconnected := consumeEvents(ctx, msgs, errs, processed, &backoff, clearProcessed,
			&lastEventNano,
			opts.Metrics,
			processorApply(opts.Proc))
		if !disconnected {
			return
		}

		msgs, errs = nil, nil

		observability.SetReady(false)
	}
}

// consumeEvents drains one events.Subscribe lifecycle. Returns true if the
// caller should reconnect, false on context cancellation. lastEventNano is
// updated with the timestamp of every received event.
//
// An event is skipped only when processed holds an entry for the container
// and the event is not newer than that entry: Docker emits "start" after the
// container is running, so an earlier event was already visible to the
// startup inspect. A newer event (restart, unpause) has a new cgroup and is
// applied. The entry is removed on the first event for that ID either way.
func consumeEvents(
	ctx context.Context,
	msgs <-chan events.Message,
	errs <-chan error,
	processed map[string]time.Time,
	backoff *time.Duration,
	clearProcessed <-chan time.Time,
	lastEventNano *int64,
	metrics *observability.Recorder,
	apply applyFn,
) bool {
	log := logger.L()

	for {
		select {
		case <-ctx.Done():
			return false

		case <-clearProcessed:
			// Overlap window expired; discard any startup entries that were
			// never matched by a live event (containers that exited during the
			// window). Reassign to a nil channel so the case never fires again.
			for key := range processed {
				delete(processed, key)
			}

			clearProcessed = nil

		case streamErr := <-errs:
			if streamErr == nil {
				continue
			}

			if errors.Is(streamErr, context.Canceled) ||
				errors.Is(streamErr, context.DeadlineExceeded) {
				return false
			}

			log.Error("docker events stream error, reconnecting",
				"err", streamErr, "backoff", *backoff)
			metrics.IncDockerReconnect()
			observability.SetReady(false)
			sleepCtx(ctx, *backoff)
			*backoff = nextBackoff(*backoff)

			return ctx.Err() == nil

		case msg, ok := <-msgs:
			if !ok {
				log.Warn("docker events channel closed, reconnecting",
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

			recordedAt, alreadyProcessed := processed[msg.Actor.ID]
			if alreadyProcessed {
				delete(processed, msg.Actor.ID)

				if !time.Unix(0, msg.TimeNano).After(recordedAt) {
					continue
				}
			}

			_ = processOne(ctx, msg.Actor.ID, metrics, apply, "could not process container")
		}
	}
}

// processOne applies device rules to one container and records the outcome:
// rules-applied result, apply duration, last-event timestamp on success, and
// a WARN with logMsg on failure. It is shared by the startup enumeration and
// the event stream so both paths report identically.
func processOne(
	ctx context.Context,
	containerID string,
	metrics *observability.Recorder,
	apply applyFn,
	logMsg string,
) error {
	start := time.Now()

	applyErr := apply(ctx, containerID)
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
