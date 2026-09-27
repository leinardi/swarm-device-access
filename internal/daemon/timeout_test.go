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
	"slices"
	"sync"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

// testCallTimeout stands in for DockerCallTimeout so hung calls resolve fast.
const testCallTimeout = 50 * time.Millisecond

// hangingInspector blocks ContainerInspect until the call's context is done,
// and records the context error each hung call observed.
type hangingInspector struct {
	mu      sync.Mutex
	ctxErrs []error
}

func (h *hangingInspector) ContainerInspect(
	ctx context.Context,
	_ string,
	_ client.ContainerInspectOptions,
) (client.ContainerInspectResult, error) {
	return client.ContainerInspectResult{}, h.hang(ctx)
}

func (h *hangingInspector) hang(ctx context.Context) error {
	<-ctx.Done()

	h.mu.Lock()
	h.ctxErrs = append(h.ctxErrs, ctx.Err())
	h.mu.Unlock()

	return fmt.Errorf("hung call: %w", ctx.Err())
}

func (h *hangingInspector) observed() []error {
	h.mu.Lock()
	defer h.mu.Unlock()

	return slices.Clone(h.ctxErrs)
}

// assertBounded fails when fn does not return within a generous multiple of
// testCallTimeout, i.e. when the hung call was not bounded.
func assertBounded(t *testing.T, name string, fn func()) {
	t.Helper()

	done := make(chan struct{})

	go func() {
		defer close(done)

		fn()
	}()

	select {
	case <-done:
	case <-time.After(20 * testCallTimeout):
		t.Fatalf("%s did not return within %v: hung call is not bounded", name, 20*testCallTimeout)
	}
}

func TestProcessExistingContainers_HungContainerListIsBounded(t *testing.T) {
	docker := &fakeDocker{hangList: true}

	var err error

	assertBounded(t, "processExistingContainers", func() {
		err = processExistingContainers(
			context.Background(),
			docker,
			map[string]time.Time{},
			nil,
			noopApply,
			testCallTimeout,
		)
	})

	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("err = %v, want context.DeadlineExceeded", err)
	}
}

func TestProcessOne_HungContainerInspectIsBounded(t *testing.T) {
	insp := &hangingInspector{}
	proc := &processor.Processor{Inspector: insp, Cfg: config.NewStore()}

	var err error

	assertBounded(t, "processOne", func() {
		err = processOne(
			context.Background(),
			"c1",
			nil,
			processorApply(proc),
			testCallTimeout,
			"fail",
		)
	})

	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("err = %v, want context.DeadlineExceeded", err)
	}
}

// TestReconcile_HungInspectIsBoundedPerCall calls the processor
// directly with an unbounded context: its own per-call timeout must bound
// ContainerInspect.
func TestReconcile_HungInspectIsBoundedPerCall(t *testing.T) {
	insp := &hangingInspector{}
	proc := &processor.Processor{
		Inspector:   insp,
		Cfg:         config.NewStore(),
		CallTimeout: testCallTimeout,
	}

	assertBounded(t, "Reconcile", func() {
		_ = proc.Reconcile(context.Background(), "c1")
	})

	got := insp.observed()
	if len(got) != 1 || !errors.Is(got[0], context.DeadlineExceeded) {
		t.Fatalf("hung call context errors = %v, want one DeadlineExceeded", got)
	}
}

func TestSubscribe_HungEventsTimesOutWithoutLeak(t *testing.T) {
	docker := &fakeDocker{hangEvents: map[int]bool{1: true}}

	var err error

	assertBounded(t, "subscribe", func() {
		_, _, err = subscribe(context.Background(), docker, "", testCallTimeout)
	})

	if !errors.Is(err, errSubscribeTimeout) {
		t.Fatalf("err = %v, want errSubscribeTimeout", err)
	}

	// subscribe waits for the canceled Events goroutine before returning, so
	// the call has already returned here rather than eventually.
	started, returned := docker.eventsCalls()
	if started != 1 || returned != 1 {
		t.Fatalf("Events started=%d returned=%d, want 1/1 (goroutine leaked)", started, returned)
	}
}

func TestSubscribe_SuccessKeepsStreamOpenUntilCancel(t *testing.T) {
	docker := &fakeDocker{}

	stream, cancelStream, err := subscribe(context.Background(), docker, "", testCallTimeout)
	if err != nil {
		t.Fatalf("subscribe: %v", err)
	}

	if stream.Messages == nil {
		t.Fatal("stream has no message channel")
	}

	// Real elapsed window: well past the establishment timeout, the stream
	// context must still be live because it must not inherit that deadline.
	time.Sleep(2 * testCallTimeout)

	streamCtx := docker.lastEventsContext()
	if streamCtx.Err() != nil {
		t.Fatalf("stream context ended after establishment: %v", streamCtx.Err())
	}

	cancelStream()

	if streamCtx.Err() == nil {
		t.Fatal("cancelStream did not end the stream context")
	}
}

// TestRun_InitialEventsHangStillEnumeratesAndResubscribes checks that a hung
// initial subscription neither blocks the startup enumeration nor the event
// loop, which resubscribes from the original since.
func TestRun_InitialEventsHangStillEnumeratesAndResubscribes(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	docker := &fakeDocker{
		containers: []container.Summary{{ID: "listed"}},
		hangEvents: map[int]bool{1: true},
	}
	insp := &recordingInspector{}
	opts := Options{
		Docker:      docker,
		Proc:        &processor.Processor{Inspector: insp, Cfg: config.NewStore()},
		callTimeout: testCallTimeout,
	}

	done := make(chan struct{})

	go func() {
		defer close(done)

		_ = run(ctx, opts, noopWatcher)
	}()

	waitFor(t, func() bool {
		started, _ := docker.eventsCalls()

		return started >= 2 && slices.Contains(insp.inspected(), "listed")
	})
	cancel()
	<-done

	_, sinces, _ := docker.snapshot()
	if sinces[1] != sinces[0] {
		t.Errorf("resubscribe Since = %q, want the initial %q", sinces[1], sinces[0])
	}

	started, returned := docker.eventsCalls()
	if started != returned {
		t.Errorf(
			"Events started=%d returned=%d after shutdown (goroutine leaked)",
			started,
			returned,
		)
	}
}

// TestListenEvents_ReconnectEventsHangRetries checks that a hung reconnect is
// abandoned after the timeout and retried through the backoff loop.
func TestListenEvents_ReconnectEventsHangRetries(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	docker := &fakeDocker{hangEvents: map[int]bool{1: true}}
	opts := Options{
		Docker:      docker,
		Proc:        &processor.Processor{Inspector: &recordingInspector{}, Cfg: config.NewStore()},
		callTimeout: testCallTimeout,
	}

	done := make(chan struct{})

	go func() {
		defer close(done)

		// A zero stream makes listenEvents subscribe immediately, as after a
		// dropped connection.
		listenEvents(
			ctx,
			opts,
			map[string]time.Time{},
			time.Now(),
			client.EventsResult{},
			func() {},
		)
	}()

	// First resubscribe hangs and times out, then minBackoff passes before the
	// second attempt: poll until it happens, allowing for minBackoff on a slow
	// runner.
	waitForWithin(t, 5*minBackoff, func() bool {
		started, _ := docker.eventsCalls()

		return started >= 2
	})
	cancel()
	<-done

	started, returned := docker.eventsCalls()
	if started != returned {
		t.Errorf(
			"Events started=%d returned=%d after shutdown (goroutine leaked)",
			started,
			returned,
		)
	}
}
