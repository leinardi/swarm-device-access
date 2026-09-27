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
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/events"
	"github.com/moby/moby/client"
	"github.com/prometheus/client_golang/prometheus"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/policy"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

const testBackoff = 5 * time.Millisecond

var errListFailed = errors.New("list failed")

// recordingApply records the containers it is called for; gate, when set,
// holds the first call until it is closed.
type recordingApply struct {
	mu      sync.Mutex
	ids     []string
	fail    map[string]int // remaining failures per container
	gate    chan struct{}
	entered chan struct{}
}

func (r *recordingApply) apply(_ context.Context, id string) error {
	r.mu.Lock()
	r.ids = append(r.ids, id)
	first := len(r.ids) == 1
	failing := r.fail[id] > 0

	if failing {
		r.fail[id]--
	}
	r.mu.Unlock()

	if first && r.gate != nil {
		close(r.entered)
		<-r.gate
	}

	if failing {
		return errApplyFailed
	}

	return nil
}

func (r *recordingApply) seen() []string {
	r.mu.Lock()
	defer r.mu.Unlock()

	return slices.Clone(r.ids)
}

func gatedApply() *recordingApply {
	return &recordingApply{gate: make(chan struct{}), entered: make(chan struct{})}
}

// startCoordinator runs a coordinator with test backoffs until the test
// ends.
func startCoordinator(t *testing.T, docker dockerAPI, apply applyFn) *coordinator {
	t.Helper()

	coord := newCoordinator(docker, apply, sharedRecorder(), DockerCallTimeout)
	coord.minBackoff = testBackoff
	coord.maxBackoff = 4 * testBackoff

	ctx, cancel := context.WithCancel(context.Background())
	done := make(chan struct{})

	go func() {
		defer close(done)

		coord.run(ctx)
	}()

	t.Cleanup(func() {
		cancel()
		<-done
	})

	return coord
}

func (c *coordinator) state() (pending int, incomplete bool) {
	c.mu.Lock()
	defer c.mu.Unlock()

	return len(c.pending), c.incomplete
}

func (c *coordinator) settled() bool {
	pending, incomplete := c.state()

	return pending == 0 && !incomplete
}

func (f *fakeDocker) listCalls() int {
	f.mu.Lock()
	defer f.mu.Unlock()

	count := 0

	for _, call := range f.calls {
		if call == "list" {
			count++
		}
	}

	return count
}

// completions returns the generation of every "config reload complete" line.
func completions(logOutput string) []string {
	var gens []string

	for line := range strings.SplitSeq(logOutput, "\n") {
		if !strings.Contains(line, `msg="config reload complete"`) {
			continue
		}

		for field := range strings.FieldsSeq(line) {
			if gen, ok := strings.CutPrefix(field, "generation="); ok {
				gens = append(gens, gen)
			}
		}
	}

	return gens
}

func gaugeValue(t *testing.T, name string) float64 {
	t.Helper()

	families, err := prometheus.DefaultGatherer.Gather()
	if err != nil {
		t.Fatalf("gather metrics: %v", err)
	}

	for _, family := range families {
		if family.GetName() == name {
			return family.GetMetric()[0].GetGauge().GetValue()
		}
	}

	t.Fatalf("metric %s not registered", name)

	return 0
}

// logBuffer is a concurrency-safe log sink: the coordinator logs from its
// own goroutine while the test reads.
type logBuffer struct {
	mu  sync.Mutex
	buf strings.Builder
}

func (l *logBuffer) Write(p []byte) (int, error) {
	l.mu.Lock()
	defer l.mu.Unlock()

	written, err := l.buf.Write(p)
	if err != nil {
		return written, fmt.Errorf("log buffer: %w", err)
	}

	return written, nil
}

func (l *logBuffer) String() string {
	l.mu.Lock()
	defer l.mu.Unlock()

	return l.buf.String()
}

func TestCoordinator_ListFailsThenSucceeds(t *testing.T) {
	logs := captureSafeLogs(t)
	docker := &fakeDocker{containers: []container.Summary{{ID: "c1"}}, listFailures: 2}
	recorder := &recordingApply{}
	coord := startCoordinator(t, docker, recorder.apply)

	coord.request(1)
	waitFor(t, func() bool { return coord.settled() && len(completions(logs.String())) == 1 })

	if got := docker.listCalls(); got != 3 {
		t.Errorf("list calls = %d, want 3 (two failures, then the pass)", got)
	}

	if got := recorder.seen(); !slices.Equal(got, []string{"c1"}) {
		t.Errorf("applied = %v, want [c1]", got)
	}

	if !strings.Contains(logs.String(), "reconciliation incomplete") {
		t.Errorf("expected a WARN while the list failed:\n%s", logs.String())
	}

	if gaugeValue(t, "sda_reload_incomplete") != 0 {
		t.Error("sda_reload_incomplete did not return to 0")
	}
}

func TestCoordinator_PendingGaugeReturnsToZero(t *testing.T) {
	logs := captureSafeLogs(t)
	docker := &fakeDocker{containers: []container.Summary{{ID: "c1"}, {ID: "c2"}}}
	recorder := &recordingApply{fail: map[string]int{"c2": 2}}
	coord := startCoordinator(t, docker, recorder.apply)

	coord.request(1)
	waitFor(t, func() bool {
		pending, _ := coord.state()

		return pending == 1
	})

	if gens := completions(logs.String()); len(gens) != 0 {
		t.Fatalf("completion logged with a container pending: %v", gens)
	}

	waitFor(t, func() bool { return coord.settled() && len(completions(logs.String())) == 1 })

	if gaugeValue(t, "sda_reconcile_pending_containers") != 0 {
		t.Error("sda_reconcile_pending_containers did not return to 0")
	}

	if got := recorder.seen(); !slices.Equal(got, []string{"c1", "c2", "c2", "c2"}) {
		t.Errorf("applied = %v, want c2 retried until it succeeds", got)
	}
}

// TestCoordinator_OverlappingPublishesCompleteOnlyNewest: a publication
// arriving while the pass for the previous one runs supersedes it; only
// the newer generation is reported complete.
func TestCoordinator_OverlappingPublishesCompleteOnlyNewest(t *testing.T) {
	logs := captureSafeLogs(t)
	docker := &fakeDocker{containers: []container.Summary{{ID: "c1"}, {ID: "c2"}}}
	recorder := gatedApply()
	coord := startCoordinator(t, docker, recorder.apply)

	coord.request(1)
	<-recorder.entered
	coord.request(2)
	close(recorder.gate)

	waitFor(t, func() bool { return coord.settled() && len(completions(logs.String())) > 0 })

	if gens := completions(logs.String()); !slices.Equal(gens, []string{"2"}) {
		t.Errorf("completed generations = %v, want [2]", gens)
	}

	// The superseded pass stopped after its current container.
	if got := recorder.seen(); !slices.Equal(got, []string{"c1", "c1", "c2"}) {
		t.Errorf("applied = %v, want [c1 c1 c2]", got)
	}
}

// TestCoordinator_OlderResultCannotClearGauges: a success that started
// before a newer request neither clears the re-tagged pending entry nor
// completes the reload.
func TestCoordinator_OlderResultCannotClearGauges(t *testing.T) {
	logs := captureSafeLogs(t)
	coord := newCoordinator(nil, noopApply, sharedRecorder(), DockerCallTimeout)

	coord.request(1)
	older := coord.current()
	coord.settle("c1", older, errApplyFailed)
	coord.request(2)

	coord.settle("c1", older, nil)
	coord.finishPass(older)

	if pending, incomplete := coord.state(); pending != 1 || !incomplete {
		t.Fatalf(
			"pending = %d, incomplete = %v after an older result; want 1, true",
			pending,
			incomplete,
		)
	}

	if gaugeValue(t, "sda_reconcile_pending_containers") != 1 ||
		gaugeValue(t, "sda_reload_incomplete") != 1 {
		t.Error("an older result changed the gauges")
	}

	if gens := completions(logs.String()); len(gens) != 0 {
		t.Fatalf("an older result logged completion: %v", gens)
	}

	newer := coord.current()
	coord.settle("c1", newer, nil)
	coord.finishPass(newer)

	if !coord.settled() || !slices.Equal(completions(logs.String()), []string{"2"}) {
		t.Errorf("newer result did not complete generation 2:\n%s", logs.String())
	}
}

// TestCoordinator_SameGenerationTriggersRunTrailingPass: two systemd reloads
// under one generation while a pass runs coalesce into one trailing pass.
func TestCoordinator_SameGenerationTriggersRunTrailingPass(t *testing.T) {
	docker := &fakeDocker{containers: []container.Summary{{ID: "c1"}}}
	recorder := gatedApply()
	coord := startCoordinator(t, docker, recorder.apply)

	coord.request(1)
	<-recorder.entered
	coord.request(1)
	coord.request(1)
	close(recorder.gate)

	waitFor(t, func() bool { return coord.settled() && len(recorder.seen()) == 2 })

	// Nothing else is pending, so no further pass may start.
	time.Sleep(10 * testBackoff)

	if got := docker.listCalls(); got != 2 {
		t.Errorf("list calls = %d, want 2 (the pass and one trailing pass)", got)
	}
}

// TestPublishAndReconcile_VisitsRunningContainers: a SIGHUP publication
// reaches every running container through the coordinator.
func TestPublishAndReconcile_VisitsRunningContainers(t *testing.T) {
	store, publisher := config.NewStore(config.Runtime{})
	proc := &processor.Processor{Inspector: &recordingInspector{}, Cfg: store, Publisher: publisher}
	docker := &fakeDocker{containers: []container.Summary{{ID: "a"}, {ID: "b"}}}
	recorder := &recordingApply{}
	coord := startCoordinator(t, docker, recorder.apply)

	proc.SetPassRequester(coord.requestPass)

	gen := proc.PublishAndReconcile(context.Background(), config.Runtime{
		Policy: policy.Global{Mode: policy.ModeOptIn},
	})

	waitFor(t, func() bool { return coord.settled() && len(recorder.seen()) == 2 })

	if gen != 2 || coord.current().generation != 2 {
		t.Errorf(
			"published generation %d, coordinator at %d; want 2",
			gen,
			coord.current().generation,
		)
	}

	if got := recorder.seen(); !slices.Equal(got, []string{"a", "b"}) {
		t.Errorf("visited = %v, want [a b]", got)
	}
}

// TestCoordinator_DedupSharedWithConsumer runs passes (which write the
// processed map) while one event consumer reads and consumes it, without a
// reconnect; run under -race.
func TestCoordinator_DedupSharedWithConsumer(t *testing.T) {
	docker := &fakeDocker{containers: []container.Summary{{ID: "a"}, {ID: "b"}, {ID: "c"}}}
	coord := startCoordinator(t, docker, noopApply)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(0, 0)
	consumed := make(chan struct{})

	go func() {
		defer close(consumed)

		backoff := minBackoff

		consumeEvents(
			ctx,
			msgs,
			errs,
			coord,
			&backoff,
			new(int64),
			nil,
			noopApply,
			DockerCallTimeout,
		)
	}()

	for round := range 50 {
		coord.request(1)

		msgs <- events.Message{
			Actor:    events.Actor{ID: []string{"a", "b", "c"}[round%3]},
			TimeNano: time.Now().UnixNano(),
		}
	}

	waitFor(t, coord.settled)
	cancel()
	<-consumed
}

// captureSafeLogs is captureLogs with a sink that tolerates the
// coordinator goroutine writing while the test reads.
func captureSafeLogs(t *testing.T) *logBuffer {
	t.Helper()

	var buf logBuffer

	setTestLogger(t, &buf)

	return &buf
}

// TestCoordinator_GoneContainerLeavesPending: a container Docker no longer
// knows ends its retries instead of keeping the reload incomplete forever.
func TestCoordinator_GoneContainerLeavesPending(t *testing.T) {
	coord := newCoordinator(nil, noopApply, sharedRecorder(), DockerCallTimeout)
	coord.request(1)
	key := coord.current()

	gone := fmt.Errorf("container %q: %w", "c1", processor.ErrContainerGone)

	coord.settle("c1", key, errApplyFailed)
	coord.settle("c1", key, gone)
	coord.settle("c2", key, gone)
	coord.finishPass(key)

	if !coord.settled() {
		pending, incomplete := coord.state()
		t.Fatalf(
			"pending = %d, incomplete = %v; want the gone containers dropped",
			pending,
			incomplete,
		)
	}
}

// TestCoordinator_DedupAcrossReconnectUnderRace drives the event loop
// through a stream close and a re-subscription while passes run, so the
// consumer and the coordinator touch the processed map concurrently on both
// sides of a reconnect; run under -race.
func TestCoordinator_DedupAcrossReconnectUnderRace(t *testing.T) {
	msgs := make(chan events.Message)
	docker := &fakeDocker{
		containers: []container.Summary{{ID: "a"}, {ID: "b"}},
		firstMsgs:  msgs,
		firstErrs:  make(chan error),
	}
	coord := startCoordinator(t, docker, noopApply)
	opts := Options{
		Docker: docker,
		Proc:   &processor.Processor{Inspector: &recordingInspector{}, Cfg: newTestStore()},
	}

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	done := make(chan struct{})

	go func() {
		defer close(done)

		listenEvents(ctx, opts, coord, time.Now(), client.EventsResult{}, func() {})
	}()

	for round := range 20 {
		coord.request(1)

		msgs <- events.Message{
			Actor:    events.Actor{ID: []string{"a", "b"}[round%2]},
			TimeNano: time.Now().UnixNano(),
		}
	}

	close(msgs)

	// While the loop backs off and re-subscribes, keep the passes going.
	waitForWithin(t, 5*minBackoff, func() bool {
		coord.request(1)

		started, _ := docker.eventsCalls()

		return started >= 2
	})

	waitFor(t, coord.settled)
	cancel()
	<-done
}
