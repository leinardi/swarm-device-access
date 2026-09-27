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
	"sync"
	"time"

	"github.com/moby/moby/api/types/events"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

// processedTTL bounds how long an enumerated container's entry suppresses a
// start event that is not newer than its inspect. After 2×maxBackoff the
// overlap window between enumeration and the event stream has certainly
// passed; a remaining entry belongs to a container that exited before its
// event was consumed.
const processedTTL = 2 * maxBackoff

// sweepInterval is how often the lifecycle history is re-checked for runs
// whose cgroup is gone.
const sweepInterval = time.Minute

// passKey is what a pass reconciles for: a config generation and a trigger
// epoch. Every request bumps the epoch, so two systemd reloads under one
// generation are still two passes (the second can wipe what the first
// restored).
type passKey struct {
	generation uint64
	epoch      uint64
}

// pendingEntry is a container whose last reconcile failed.
type pendingEntry struct {
	// key is the request the entry must be reconciled for; it is re-tagged
	// with every newer request, so an older result cannot clear it.
	key     passKey
	backoff time.Duration
	due     time.Time
}

// passOutcome is how a pass ended.
type passOutcome int

const (
	passDone passOutcome = iota
	passListFailed
	passSuperseded
)

// coordinator is the single owner of reconciliation passes. Startup,
// SIGHUP (through Processor.PublishAndReconcile) and systemd reloads only
// request a pass; one goroutine (run) runs them, retries the containers
// that failed, maintains the pending and reload-incomplete gauges and logs
// completion. Requests arriving while a pass runs coalesce: the pass
// finishes its current container, then starts again under the newest
// request, so no result is reported on behalf of an older one.
//
// It also adjudicates and applies container events: the event consumer
// only submits them (handleEvent). Every reconcile or cleanup of a
// container first reserves it, so work requested for a container that is
// already being handled coalesces into one follow-up instead of running
// twice; a start event not newer than the running apply is already covered
// by it. The coordinator keeps the lifecycle history of container runs
// (shared with the processor, which records into it) and sweeps it
// periodically, and owns the processed map that skips start events an
// enumeration already covered.
type coordinator struct {
	docker  dockerAPI
	apply   applyFn
	metrics *observability.Recorder
	timeout time.Duration

	// terminate and sweep are Processor.Terminate and Processor.Sweep;
	// nil in tests that do not exercise them.
	terminate func(containerID string, eventTime time.Time) error
	sweep     func() int
	// lifecycles is the history of container runs, installed into the
	// processor by Run.
	lifecycles *processor.Lifecycles

	minBackoff time.Duration
	maxBackoff time.Duration
	now        func() time.Time

	// wake has room for one signal; a request never blocks on it.
	wake chan struct{}
	// passEnded receives nothing; it is closed after the first pass
	// attempt so Run can start consuming events after the enumeration.
	passEnded chan struct{}

	mu sync.Mutex
	// latest is the newest request; dirty means no pass has started for it.
	latest passKey
	dirty  bool
	// incomplete is set from a request until a pass for the latest request
	// has visited every listed container.
	incomplete bool
	// announced is the request whose completion was last logged.
	announced passKey
	pending   map[string]*pendingEntry
	processed map[string]time.Time
	firstDone bool
	// inFlight holds a reservation for every container being handled.
	inFlight map[string]*reservation
}

// reservation marks a container as being handled and collects the work
// requested for it meanwhile.
type reservation struct {
	// applyStart is when the current reconcile began; a start event not
	// newer than it is covered by that reconcile. Zero while the holder
	// only cleans up, which covers no start event.
	applyStart time.Time
	// running is set while the holder's work includes a reconcile, whose
	// outcome completion must wait for.
	running bool
	// reconcile asks for another reconcile after the current work.
	reconcile bool
	// terminateAt is the latest end-of-run event received meanwhile.
	terminateAt time.Time
}

func newCoordinator(
	docker dockerAPI,
	apply applyFn,
	metrics *observability.Recorder,
	timeout time.Duration,
) *coordinator {
	return &coordinator{
		docker:     docker,
		apply:      apply,
		metrics:    metrics,
		timeout:    timeout,
		minBackoff: minBackoff,
		maxBackoff: maxBackoff,
		now:        time.Now,
		wake:       make(chan struct{}, 1),
		passEnded:  make(chan struct{}),
		pending:    make(map[string]*pendingEntry),
		processed:  make(map[string]time.Time),
		inFlight:   make(map[string]*reservation),
		lifecycles: &processor.Lifecycles{},
	}
}

// request asks for a pass under generation (or the newest generation
// already requested, whichever is higher). It never blocks.
func (c *coordinator) request(generation uint64) {
	c.mu.Lock()

	c.latest.epoch++
	c.latest.generation = max(c.latest.generation, generation)
	c.dirty = true
	c.incomplete = true

	for _, entry := range c.pending {
		entry.key = c.latest
	}

	c.metrics.SetReloadIncomplete(true)
	c.mu.Unlock()

	select {
	case c.wake <- struct{}{}:
	default:
	}
}

// requestPass adapts request to processor.PassRequester.
func (c *coordinator) requestPass(_ context.Context, generation uint64) {
	c.request(generation)
}

// awaitFirstPass blocks until the first pass has ended (completed, failed
// to list, or been superseded) or ctx is done.
func (c *coordinator) awaitFirstPass(ctx context.Context) {
	select {
	case <-c.passEnded:
	case <-ctx.Done():
	}
}

// run executes passes and retries until ctx is done.
func (c *coordinator) run(ctx context.Context) {
	listBackoff := c.minBackoff

	var listRetry <-chan time.Time

	prune := time.NewTicker(processedTTL)
	defer prune.Stop()

	sweep := time.NewTicker(sweepInterval)
	defer sweep.Stop()

	for {
		if ctx.Err() != nil {
			return
		}

		key, start := c.takeRequest()
		if start {
			switch c.pass(ctx, key) {
			case passListFailed:
				listRetry = time.After(listBackoff)
				listBackoff = min(listBackoff*2, c.maxBackoff)
			case passDone:
				listRetry = nil
				listBackoff = c.minBackoff
			case passSuperseded:
			}

			continue
		}

		var retry <-chan time.Time
		if due, ok := c.nextDue(); ok {
			retry = time.After(max(due.Sub(c.now()), 0))
		}

		select {
		case <-ctx.Done():
			return
		case <-c.wake:
		case <-listRetry:
			listRetry = nil

			c.redo()
		case <-retry:
			c.retryDue(ctx)
		case <-prune.C:
			c.pruneProcessed()
		case <-sweep.C:
			c.runSweep()
		}
	}
}

// takeRequest returns the latest request if no pass has started for it.
func (c *coordinator) takeRequest() (passKey, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.dirty {
		return passKey{}, false
	}

	c.dirty = false

	return c.latest, true
}

// redo marks the latest request as needing a pass again (its list failed).
func (c *coordinator) redo() {
	c.mu.Lock()
	c.dirty = true
	c.mu.Unlock()
}

// pass lists the running containers and reconciles each under key.
func (c *coordinator) pass(ctx context.Context, key passKey) passOutcome {
	defer c.markFirstPassEnded()

	log := logger.L()

	listCtx, cancelList := context.WithTimeout(ctx, c.timeout)
	list, err := c.docker.ContainerList(listCtx, client.ContainerListOptions{})

	cancelList()

	if err != nil {
		log.Warn("could not list containers; reconciliation incomplete, retrying",
			"err", fmt.Errorf("list containers: %w", err),
			"generation", key.generation)
		c.warnIncomplete()

		return passListFailed
	}

	log.Debug("reconciling running containers",
		"count", len(list.Items), "generation", key.generation, "epoch", key.epoch)

	for idx := range list.Items {
		if ctx.Err() != nil || c.superseded(key) {
			return passSuperseded
		}

		_ = c.reconcileContainer(
			ctx,
			list.Items[idx].ID,
			time.Time{},
			"could not process running container",
		)
	}

	c.finishPass(key)

	return passDone
}

// retryDue reconciles every pending container whose backoff has elapsed.
func (c *coordinator) retryDue(ctx context.Context) {
	for _, containerID := range c.dueIDs() {
		if ctx.Err() != nil {
			return
		}

		_ = c.reconcileContainer(
			ctx,
			containerID,
			time.Time{},
			"could not reconcile pending container",
		)
	}

	c.mu.Lock()
	c.reportLocked()
	c.mu.Unlock()
}

// handleEvent adjudicates one container event from the stream. start and
// unpause reconcile the container unless an enumeration already covered
// the event; die and destroy end a run and clean up after it.
func (c *coordinator) handleEvent(ctx context.Context, msg *events.Message) {
	eventTime := time.Unix(0, msg.TimeNano)
	containerID := msg.Actor.ID

	switch msg.Action { //nolint:exhaustive // the stream is filtered to these four actions
	case events.ActionDie, events.ActionDestroy:
		c.terminateContainer(ctx, containerID, eventTime)
	default:
		if c.skipEvent(containerID, eventTime) {
			return
		}

		applyErr := c.reconcileContainer(ctx, containerID, eventTime, "could not process container")
		if applyErr != nil {
			// Wake run so the new pending entry's retry is scheduled.
			select {
			case c.wake <- struct{}{}:
			default:
			}
		}
	}
}

// reconcileContainer reconciles one container unless it is already being
// handled, in which case the work is left to the holder (see reserve). It
// returns the error of the last reconcile it ran.
func (c *coordinator) reconcileContainer(
	ctx context.Context,
	containerID string,
	eventTime time.Time,
	logMsg string,
) error {
	if !c.reserve(containerID, eventTime, false) {
		return nil
	}

	// Only an enumeration (no event) marks the container processed: its
	// inspect is what covers start events that are delivered later.
	return c.hold(ctx, containerID, true, time.Time{}, eventTime.IsZero(), logMsg)
}

// terminateContainer cleans up after a run that ended at eventTime.
func (c *coordinator) terminateContainer(
	ctx context.Context,
	containerID string,
	eventTime time.Time,
) {
	if !c.reserve(containerID, eventTime, true) {
		return
	}

	_ = c.hold(ctx, containerID, false, eventTime, false, "could not process container")
}

// hold does the work for a reserved container, then whatever was requested
// for it meanwhile, until nothing is left; it then releases the
// reservation. A cleanup runs before a reconcile, so the reconcile sees
// the final state.
func (c *coordinator) hold(
	ctx context.Context,
	containerID string,
	reconcile bool,
	terminateAt time.Time,
	enumerated bool,
	logMsg string,
) error {
	var lastErr error

	for {
		if !terminateAt.IsZero() && c.terminate != nil {
			termErr := c.terminate(containerID, terminateAt)
			if termErr != nil {
				logger.L().
					Warn("could not clean up after container", "id", containerID, "err", termErr)
			}
		}

		if reconcile {
			key := c.current()
			started := c.now()

			lastErr = processOne(ctx, containerID, c.metrics, c.apply, c.timeout, logMsg)
			c.settle(containerID, key, lastErr)

			if lastErr == nil && enumerated {
				c.markProcessed(containerID, started)
			}
		}

		reconcile, terminateAt = c.next(containerID)
		if !reconcile && terminateAt.IsZero() {
			return lastErr
		}
	}
}

// reserve claims containerID for the caller and reports whether it got
// it. If the container is already being handled, the request is left to
// the holder: an end-of-run event is queued; a reconcile is queued unless
// it is a start event not newer than the running apply, which that apply
// already covers.
func (c *coordinator) reserve(containerID string, eventTime time.Time, terminal bool) bool {
	c.mu.Lock()
	defer c.mu.Unlock()

	held, busy := c.inFlight[containerID]
	if !busy {
		fresh := &reservation{running: !terminal}
		if !terminal {
			fresh.applyStart = c.now()
		}

		c.inFlight[containerID] = fresh

		return true
	}

	switch {
	case terminal:
		if eventTime.After(held.terminateAt) {
			held.terminateAt = eventTime
		}
	case eventTime.IsZero() || held.applyStart.IsZero() || eventTime.After(held.applyStart):
		held.reconcile = true
	}

	return false
}

// next returns the work queued for a reserved container, or releases the
// reservation when there is none.
func (c *coordinator) next(containerID string) (bool, time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()

	held := c.inFlight[containerID]
	if !held.reconcile && held.terminateAt.IsZero() {
		delete(c.inFlight, containerID)

		// Its retries were held back while the container was reserved.
		if _, isPending := c.pending[containerID]; isPending {
			select {
			case c.wake <- struct{}{}:
			default:
			}
		}

		c.announceLocked()

		return false, time.Time{}
	}

	reconcile, terminateAt := held.reconcile, held.terminateAt
	held.reconcile, held.terminateAt = false, time.Time{}
	held.running = reconcile
	held.applyStart = time.Time{}

	if reconcile {
		held.applyStart = c.now()
	}

	return reconcile, terminateAt
}

// runSweep re-checks the lifecycle history (Processor.Sweep).
func (c *coordinator) runSweep() {
	if c.sweep == nil {
		return
	}

	released := c.sweep()
	if released > 0 {
		logger.L().Debug("lifecycle sweep released records", "released", released)
	}
}

// settle records one reconcile that started under key. Success clears the
// container's pending entry only if no newer request re-tagged it since;
// a container Docker no longer knows is dropped; any other failure is
// (re)scheduled with backoff.
func (c *coordinator) settle(containerID string, key passKey, applyErr error) {
	c.mu.Lock()
	defer c.mu.Unlock()

	entry, isPending := c.pending[containerID]

	switch {
	case applyErr == nil || errors.Is(applyErr, processor.ErrContainerGone):
		if isPending && entry.key == key {
			delete(c.pending, containerID)
		} else if isPending {
			entry.due = c.now()
		}
	case isPending:
		entry.backoff = min(entry.backoff*2, c.maxBackoff)
		entry.due = c.now().Add(entry.backoff)
	default:
		c.pending[containerID] = &pendingEntry{
			key:     c.latest,
			backoff: c.minBackoff,
			due:     c.now().Add(c.minBackoff),
		}
	}

	c.metrics.SetPendingContainers(len(c.pending))
}

// finishPass records that a pass for key visited every listed container.
func (c *coordinator) finishPass(key passKey) {
	c.mu.Lock()
	defer c.mu.Unlock()

	if key == c.latest {
		c.incomplete = false
		c.metrics.SetReloadIncomplete(false)
	}

	c.reportLocked()
}

// reportLocked logs completion once per request when nothing is left for
// the latest request, and warns while anything is.
func (c *coordinator) reportLocked() {
	log := logger.L()

	if c.incomplete || len(c.pending) > 0 {
		log.Warn("reconciliation incomplete; retrying",
			"pending_containers", len(c.pending),
			"reload_incomplete", c.incomplete,
			"generation", c.latest.generation)

		return
	}

	c.announceLocked()
}

// announceLocked logs completion once per request, when nothing is left
// for the latest request: no pass outstanding, nothing pending, and no
// follow-up queued behind a reservation.
func (c *coordinator) announceLocked() {
	if c.incomplete || len(c.pending) > 0 || c.announced == c.latest {
		return
	}

	for _, held := range c.inFlight {
		if held.reconcile || held.running {
			return
		}
	}

	c.announced = c.latest
	logger.L().Info("config reload complete",
		"generation", c.latest.generation, "epoch", c.latest.epoch)
}

func (c *coordinator) warnIncomplete() {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.reportLocked()
}

func (c *coordinator) superseded(key passKey) bool {
	c.mu.Lock()
	defer c.mu.Unlock()

	return c.latest != key
}

func (c *coordinator) current() passKey {
	c.mu.Lock()
	defer c.mu.Unlock()

	return c.latest
}

func (c *coordinator) nextDue() (time.Time, bool) {
	c.mu.Lock()
	defer c.mu.Unlock()

	var (
		next  time.Time
		found bool
	)

	for containerID, entry := range c.pending {
		// A reserved container is retried by its holder's settle, or
		// once the reservation ends (next wakes run).
		if _, reserved := c.inFlight[containerID]; reserved {
			continue
		}

		if !found || entry.due.Before(next) {
			next, found = entry.due, true
		}
	}

	return next, found
}

func (c *coordinator) dueIDs() []string {
	c.mu.Lock()
	defer c.mu.Unlock()

	now := c.now()

	var ids []string

	for containerID, entry := range c.pending {
		if _, reserved := c.inFlight[containerID]; reserved {
			continue
		}

		if !entry.due.After(now) {
			ids = append(ids, containerID)
		}
	}

	return ids
}

func (c *coordinator) markFirstPassEnded() {
	c.mu.Lock()
	defer c.mu.Unlock()

	if !c.firstDone {
		c.firstDone = true
		close(c.passEnded)
	}
}

// markProcessed records that containerID was reconciled from an inspect
// made at inspectedAt.
func (c *coordinator) markProcessed(containerID string, inspectedAt time.Time) {
	c.mu.Lock()
	defer c.mu.Unlock()

	c.processed[containerID] = inspectedAt
}

// skipEvent reports whether an event for containerID at eventTime is
// already covered by an enumeration: Docker emits "start" after the
// container is running, so an event not newer than the inspect was visible
// to it. The entry is consumed by the first event for the container either
// way, and ignored once older than processedTTL.
func (c *coordinator) skipEvent(containerID string, eventTime time.Time) bool {
	c.mu.Lock()
	defer c.mu.Unlock()

	inspectedAt, found := c.processed[containerID]
	if !found {
		return false
	}

	delete(c.processed, containerID)

	if c.now().Sub(inspectedAt) > processedTTL {
		return false
	}

	return !eventTime.After(inspectedAt)
}

func (c *coordinator) pruneProcessed() {
	c.mu.Lock()
	defer c.mu.Unlock()

	now := c.now()

	for containerID, inspectedAt := range c.processed {
		if now.Sub(inspectedAt) > processedTTL {
			delete(c.processed, containerID)
		}
	}
}
