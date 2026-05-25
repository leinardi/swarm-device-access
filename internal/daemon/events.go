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

// listenEvents subscribes to Docker container events and applies device rules.
// "start" covers fresh starts and restart's second phase; "unpause" covers
// resume from a paused state if the cgroup state was cleared. On stream error
// it reconnects with exponential backoff (capped) rather than terminating the
// daemon — replaces the upstream log.Fatal(err) pattern.
//
// /readyz reflects live Docker event-stream health via observability.SetReady.
func listenEvents(
	ctx context.Context,
	opts Options,
	processed map[string]struct{},
) {
	log := logger.L()
	backoff := minBackoff

	// The processed map guards the overlap window between startup enumeration
	// and the live event stream. After 2×maxBackoff (60s) the window has
	// certainly passed; any remaining entries are from containers that exited
	// before producing a start event and will never be drained normally.
	clearProcessed := time.After(2 * maxBackoff)

	eventFilters := filters.NewArgs(
		filters.Arg("event", "start"),
		filters.Arg("event", "unpause"),
	)

	for {
		if ctx.Err() != nil {
			return
		}

		msgs, errs := opts.Docker.Events(
			ctx,
			events.ListOptions{Filters: eventFilters},
		)

		observability.SetReady(true)
		log.Debug("subscribed to docker events")

		disconnected := consumeEvents(ctx, msgs, errs, processed, &backoff, clearProcessed,
			opts.Metrics,
			func(ctx context.Context, id string) error {
				return opts.Proc.ProcessContainer(ctx, id)
			})
		if !disconnected {
			return
		}

		observability.SetReady(false)
	}
}

// consumeEvents drains one events.Subscribe lifecycle. Returns true if the
// caller should reconnect, false on context cancellation.
func consumeEvents(
	ctx context.Context,
	msgs <-chan events.Message,
	errs <-chan error,
	processed map[string]struct{},
	backoff *time.Duration,
	clearProcessed <-chan time.Time,
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

			metrics.RecordEvent(string(msg.Action))

			if _, alreadyProcessed := processed[msg.Actor.ID]; alreadyProcessed {
				delete(processed, msg.Actor.ID)

				continue
			}

			start := time.Now()

			applyErr := apply(ctx, msg.Actor.ID)
			if applyErr != nil {
				log.Warn("could not process container",
					"id", msg.Actor.ID, "err", applyErr)
				metrics.RecordRuleApplied(false)
			} else {
				metrics.RecordRuleApplied(true)
				metrics.SetLastEvent(time.Now())
			}

			metrics.ObserveApplyDuration(time.Since(start))
		}
	}
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
