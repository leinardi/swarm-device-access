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
	"maps"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/moby/moby/api/types/events"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/observability"
)

var errTransportEOF = errors.New("transport EOF")

// makeChans returns buffered event/error channels for driving consumeEvents in tests.
func makeChans(msgBuf, errBuf int) (msgs chan events.Message, errs chan error) {
	return make(chan events.Message, msgBuf), make(chan error, errBuf)
}

func noopApply(_ context.Context, _ string) error { return nil }

func TestNextBackoff_Doubles(t *testing.T) {
	got := nextBackoff(1 * time.Second)
	if got != 2*time.Second {
		t.Errorf("nextBackoff(1s) = %v, want 2s", got)
	}
}

func TestNextBackoff_Caps(t *testing.T) {
	got := nextBackoff(maxBackoff)
	if got != maxBackoff {
		t.Errorf("nextBackoff(maxBackoff) = %v, want %v (capped)", got, maxBackoff)
	}

	got = nextBackoff(maxBackoff - 1*time.Second)
	if got != maxBackoff {
		t.Errorf("nextBackoff(maxBackoff-1s) = %v, want %v (capped)", got, maxBackoff)
	}
}

func TestSleepCtx_RespectsCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	start := time.Now()

	sleepCtx(ctx, 5*time.Second)

	elapsed := time.Since(start)
	if elapsed > 100*time.Millisecond {
		t.Errorf("sleepCtx took %v with canceled ctx; want immediate return", elapsed)
	}
}

func TestSleepCtx_WaitsForDuration(t *testing.T) {
	ctx := context.Background()
	target := 50 * time.Millisecond

	start := time.Now()

	sleepCtx(ctx, target)

	elapsed := time.Since(start)
	if elapsed < target {
		t.Errorf("sleepCtx returned in %v; want at least %v", elapsed, target)
	}
}

func TestConsumeEvents_ContextCancelledReturnsNoReconnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	got := consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(noopApply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)
	if got {
		t.Error(
			"consumeEvents should return false (no reconnect) when context is already canceled",
		)
	}
}

func TestConsumeEvents_StreamErrorReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 1)
	backoff := minBackoff

	errs <- errTransportEOF

	got := consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(noopApply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)
	if !got {
		t.Error("consumeEvents should return true (reconnect) on stream error")
	}

	if backoff != nextBackoff(minBackoff) {
		t.Errorf("backoff = %v, want %v after one error", backoff, nextBackoff(minBackoff))
	}
}

func TestConsumeEvents_ContextErrFromStreamErrorNoReconnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	msgs, errs := makeChans(0, 1)
	backoff := minBackoff

	errs <- context.Canceled

	cancel()

	got := consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(noopApply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)
	if got {
		t.Error("consumeEvents should return false when stream error is context.Canceled")
	}
}

// cancelledCtx reports itself canceled through Err but never closes Done, so a
// select in consumeEvents can only take the errs case. That pins the race where
// the stream error and the cancellation arrive together and select picks errs.
type cancelledCtx struct{}

func (cancelledCtx) Deadline() (time.Time, bool) { return time.Time{}, false }

func (cancelledCtx) Done() <-chan struct{} { return nil }

func (cancelledCtx) Err() error { return context.Canceled }

func (cancelledCtx) Value(any) any { return nil }

func TestConsumeEvents_ArbitraryStreamErrorAfterCancelNoReconnect(t *testing.T) {
	logs := captureLogs(t)

	ctx := cancelledCtx{}
	msgs, errs := makeChans(0, 1)
	backoff := minBackoff

	// A canceled stream can end with a closed-body read error rather than
	// context.Canceled; it must be treated as shutdown, not a stream failure.
	errs <- errTransportEOF

	got := consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(noopApply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)
	if got {
		t.Error("consumeEvents should return false when the context is already canceled")
	}

	if backoff != minBackoff {
		t.Errorf(
			"backoff = %v, want %v: a shutdown must not count as a failure",
			backoff,
			minBackoff,
		)
	}

	if strings.Contains(logs.String(), "docker events stream error") {
		t.Errorf("unexpected stream-error log at shutdown:\n%s", logs.String())
	}
}

func TestEventListOptions(t *testing.T) {
	t.Parallel()

	const since = "2026-01-02T03:04:05.000000006Z"

	got := eventListOptions(since)

	if got.Since != since {
		t.Errorf("Since = %q, want %q", got.Since, since)
	}

	want := client.Filters{"event": {"start": true, "unpause": true, "die": true, "destroy": true}}
	if !maps.EqualFunc(got.Filters, want, maps.Equal) {
		t.Errorf("Filters = %v, want %v", got.Filters, want)
	}
}

func TestConsumeEvents_ChannelCloseReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	close(msgs)

	got := consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(noopApply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)
	if !got {
		t.Error("consumeEvents should return true (reconnect) on channel close")
	}
}

func TestConsumeEvents_EventCallsApply(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(2, 0)
	backoff := minBackoff

	apply, called := cancelAfter(2, cancel, nil)

	msgs <- events.Message{Actor: events.Actor{ID: "container-1"}}

	msgs <- events.Message{Actor: events.Actor{ID: "container-2"}}

	consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(apply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)

	if called.Load() != 2 {
		t.Errorf("apply called %d times, want 2", called.Load())
	}
}

func TestConsumeEvents_DeduplicatesProcessedIDs(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := minBackoff
	processed := map[string]time.Time{
		"already-seen": time.Now(),
	}

	apply, called := cancelAfter(1, cancel, nil)

	msgs <- events.Message{Actor: events.Actor{ID: "already-seen"}}

	consumeUntilSkipped(t, ctx, cancel, func() {
		consumeEvents(
			ctx,
			msgs,
			errs,
			testCoordinatorWith(apply, nil, processed),
			&backoff,
			new(int64),
			nil,
		)
	})

	if called.Load() != 0 {
		t.Errorf("apply called %d times for deduplicated ID, want 0", called.Load())
	}

	if _, stillPresent := processed["already-seen"]; stillPresent {
		t.Error("processed map should have entry removed after deduplication")
	}
}

// TestCoordinator_ProcessedEntriesExpire checks that an enumeration entry
// older than processedTTL neither suppresses an event nor survives a prune.
func TestCoordinator_ProcessedEntriesExpire(t *testing.T) {
	inspectedAt := time.Now()
	coord := testCoordinator(map[string]time.Time{
		"stale-a": inspectedAt,
		"stale-b": inspectedAt,
		"fresh":   inspectedAt.Add(processedTTL),
	})
	coord.now = func() time.Time { return inspectedAt.Add(processedTTL + time.Second) }

	if coord.skipEvent("stale-a", inspectedAt.Add(-time.Second)) {
		t.Error("an expired entry must not suppress an event")
	}

	coord.pruneProcessed()

	if _, ok := coord.processed["stale-b"]; ok || len(coord.processed) != 1 {
		t.Errorf("processed = %v after prune, want only the fresh entry", coord.processed)
	}
}

// TestCoordinator_PruneArmedOnlyWithEntries checks that the prune timer is
// armed only while the processed map has entries, and that an armed timer
// is kept rather than restarted on every loop iteration.
func TestCoordinator_PruneArmedOnlyWithEntries(t *testing.T) {
	inspectedAt := time.Now()
	coord := testCoordinator(map[string]time.Time{})

	if coord.armPrune(nil) != nil {
		t.Fatal("prune timer armed for an empty processed map")
	}

	coord.markProcessed("c1", inspectedAt)

	armed := coord.armPrune(nil)
	if armed == nil {
		t.Fatal("prune timer not armed with an entry to expire")
	}

	if coord.armPrune(armed) != armed {
		t.Error("an armed prune timer was replaced")
	}

	coord.now = func() time.Time { return inspectedAt.Add(processedTTL + time.Second) }
	coord.pruneProcessed()

	if coord.armPrune(nil) != nil {
		t.Errorf("prune timer re-armed after the prune emptied the map (%v)", coord.processed)
	}
}

func TestConsumeEvents_BackoffResetsOnSuccessfulEvent(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := maxBackoff // start with a high backoff

	msgs <- events.Message{Actor: events.Actor{ID: "c1"}}

	apply, _ := cancelAfter(1, cancel, nil)

	consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(apply, nil, nil),
		&backoff,
		new(int64),
		nil,
	)

	if backoff != minBackoff {
		t.Errorf("backoff = %v after successful event, want %v (reset)", backoff, minBackoff)
	}
}

// TestConsumeEvents_RestartWithinWindow checks the timestamp dedupe: an event
// newer than the startup inspect is a new run (restart/unpause) and is applied;
// an older one was already seen by the inspect and is skipped. The entry is
// removed after either.
func TestConsumeEvents_RestartWithinWindow(t *testing.T) {
	recordedAt := time.Now()

	cases := []struct {
		name      string
		eventTime time.Time
		wantApply int32
	}{
		{"newer event applied", recordedAt.Add(time.Second), 1},
		{"older event skipped", recordedAt.Add(-time.Millisecond), 0},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			ctx, cancel := context.WithCancel(context.Background())
			defer cancel()

			msgs, errs := makeChans(1, 0)
			backoff := minBackoff
			processed := map[string]time.Time{"c1": recordedAt}

			apply, called := cancelAfter(1, cancel, nil)

			msgs <- events.Message{Actor: events.Actor{ID: "c1"}, TimeNano: tc.eventTime.UnixNano()}

			consume := func() {
				consumeEvents(
					ctx,
					msgs,
					errs,
					testCoordinatorWith(apply, nil, processed),
					&backoff,
					new(int64),
					nil,
				)
			}

			// A positive case ends from inside apply; a skipped event has
			// no apply to end it, so it waits for the skip log instead.
			if tc.wantApply > 0 {
				consume()
			} else {
				consumeUntilSkipped(t, ctx, cancel, consume)
			}

			if called.Load() != tc.wantApply {
				t.Errorf("apply called %d times, want %d", called.Load(), tc.wantApply)
			}

			if _, stillPresent := processed["c1"]; stillPresent {
				t.Error("processed entry should be removed after the first event")
			}
		})
	}
}

func TestConsumeEvents_TracksLastEventNano(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(2, 0)
	backoff := minBackoff

	var lastEventNano int64

	msgs <- events.Message{Actor: events.Actor{ID: "c1"}, TimeNano: 200}

	msgs <- events.Message{Actor: events.Actor{ID: "c2"}, TimeNano: 300}

	apply, _ := cancelAfter(2, cancel, nil)

	consumeEvents(
		ctx,
		msgs,
		errs,
		testCoordinatorWith(apply, nil, nil),
		&backoff,
		&lastEventNano,
		nil,
	)

	if lastEventNano != 300 {
		t.Errorf("lastEventNano = %d, want 300", lastEventNano)
	}
}

func TestResubscribeSince(t *testing.T) {
	initial := time.Date(2026, 9, 14, 10, 0, 0, 0, time.UTC)

	got := resubscribeSince(initial, 0)
	if got != initial.Format(time.RFC3339Nano) {
		t.Errorf(
			"resubscribeSince before any event = %q, want initial %q",
			got,
			initial.Format(time.RFC3339Nano),
		)
	}

	last := time.Date(2026, 9, 14, 10, 0, 5, 123456789, time.UTC)

	got = resubscribeSince(initial, last.UnixNano())

	parsed, err := time.Parse(time.RFC3339Nano, got)
	if err != nil {
		t.Fatalf("parse %q: %v", got, err)
	}

	if !parsed.Equal(last.Add(time.Nanosecond)) {
		t.Errorf("resubscribeSince = %v, want %v", parsed, last.Add(time.Nanosecond))
	}
}

// testCoordinator returns a coordinator for consumer tests, with processed
// as its enumeration entries when non-nil.
// cancelAfter returns an apply that counts its calls and returns result;
// the n-th call cancels the context, so a test expecting n applies ends as
// soon as the last one is observed and never earlier.
func cancelAfter(n int32, cancel context.CancelFunc, result error) (applyFn, *atomic.Int32) {
	var calls atomic.Int32

	return func(_ context.Context, _ string) error {
		if calls.Add(1) == n {
			cancel()
		}

		return result
	}, &calls
}

// skipSignalTimeout is how long a test expecting a skipped event waits for
// the skip log before giving up. It is only an upper bound: the test goes on
// as soon as the log line appears.
const skipSignalTimeout = 2 * time.Second

// consumeUntilSkipped runs consume in a goroutine, waits for the coordinator's
// skip log (the positive signal that the event was handled), then cancels ctx
// and waits for consume to return. A test expecting zero applies cannot end
// from inside apply, and canceling before the event is handled would pass
// without checking anything.
func consumeUntilSkipped(
	t *testing.T,
	ctx context.Context,
	cancel context.CancelFunc,
	consume func(),
) {
	t.Helper()

	skipped := logSignal(t, "event already covered by a pass; skipped")
	done := make(chan struct{})

	go func() {
		defer close(done)

		consume()
	}()

	select {
	case <-skipped:
	case <-ctx.Done():
		t.Error("context ended before the event was skipped")
	case <-time.After(skipSignalTimeout):
		t.Error("the event was not reported as skipped")
	}

	cancel()
	<-done
}

// logSignal sends logger.L() to a writer that closes the returned channel
// the first time a line contains msg. The writer is safe for the concurrent
// use a running consumer makes of it.
func logSignal(t *testing.T, msg string) <-chan struct{} {
	t.Helper()

	sink := &signalWriter{want: msg, seen: make(chan struct{})}
	setTestLogger(t, sink)

	return sink.seen
}

type signalWriter struct {
	mu   sync.Mutex
	want string
	seen chan struct{}
	done bool
}

func (w *signalWriter) Write(line []byte) (int, error) {
	w.mu.Lock()
	defer w.mu.Unlock()

	if !w.done && strings.Contains(string(line), w.want) {
		w.done = true
		close(w.seen)
	}

	return len(line), nil
}

func testCoordinator(processed map[string]time.Time) *coordinator {
	return testCoordinatorWith(noopApply, nil, processed)
}

// testCoordinatorWith is testCoordinator with the reconcile the events
// trigger and the recorder it reports to.
func testCoordinatorWith(
	apply applyFn,
	metrics *observability.Recorder,
	processed map[string]time.Time,
) *coordinator {
	coord := newCoordinator(nil, apply, metrics, DockerCallTimeout)
	if processed != nil {
		coord.processed = processed
	}

	return coord
}
