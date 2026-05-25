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
	"sync/atomic"
	"testing"
	"time"

	"github.com/docker/docker/api/types/events"
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

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, nil, noopApply)
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

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, nil, noopApply)
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

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, nil, noopApply)
	if got {
		t.Error("consumeEvents should return false when stream error is context.Canceled")
	}
}

func TestConsumeEvents_ChannelCloseReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	close(msgs)

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, nil, noopApply)
	if !got {
		t.Error("consumeEvents should return true (reconnect) on channel close")
	}
}

func TestConsumeEvents_EventCallsApply(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(2, 0)
	backoff := minBackoff

	var called atomic.Int32

	apply := func(_ context.Context, _ string) error {
		called.Add(1)

		return nil
	}

	msgs <- events.Message{Actor: events.Actor{ID: "container-1"}}

	msgs <- events.Message{Actor: events.Actor{ID: "container-2"}}

	// Cancel after a brief delay so consumeEvents exits cleanly.
	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, nil, apply)

	if called.Load() != 2 {
		t.Errorf("apply called %d times, want 2", called.Load())
	}
}

func TestConsumeEvents_DeduplicatesProcessedIDs(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := minBackoff
	processed := map[string]struct{}{
		"already-seen": {},
	}

	var called atomic.Int32

	apply := func(_ context.Context, _ string) error {
		called.Add(1)

		return nil
	}

	msgs <- events.Message{Actor: events.Actor{ID: "already-seen"}}

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, processed, &backoff, nil, nil, apply)

	if called.Load() != 0 {
		t.Errorf("apply called %d times for deduplicated ID, want 0", called.Load())
	}

	if _, stillPresent := processed["already-seen"]; stillPresent {
		t.Error("processed map should have entry removed after deduplication")
	}
}

func TestConsumeEvents_ClearProcessedOnTTL(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(0, 0)
	backoff := minBackoff
	processed := map[string]struct{}{
		"stale-a": {},
		"stale-b": {},
	}

	// Fire the TTL immediately via a closed channel.
	clearCh := make(chan time.Time)
	close(clearCh)

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, processed, &backoff, clearCh, nil, noopApply)

	if len(processed) != 0 {
		t.Errorf("processed map has %d entries after TTL, want 0", len(processed))
	}
}

func TestConsumeEvents_BackoffResetsOnSuccessfulEvent(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := maxBackoff // start with a high backoff

	msgs <- events.Message{Actor: events.Actor{ID: "c1"}}

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, nil, noopApply)

	if backoff != minBackoff {
		t.Errorf("backoff = %v after successful event, want %v (reset)", backoff, minBackoff)
	}
}
