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
	"bytes"
	"context"
	"errors"
	"log/slog"
	"slices"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/swarm"
	"github.com/prometheus/client_golang/prometheus"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

// fakeDocker is a dockerAPI test double. The first Events call returns
// firstMsgs/firstErrs; later calls return streams that stay open until ctx is
// done. onList, when set, runs inside ContainerList (i.e. during enumeration).
type fakeDocker struct {
	mu         sync.Mutex
	calls      []string
	sinces     []string
	listTime   time.Time
	containers []container.Summary
	firstMsgs  chan events.Message
	firstErrs  chan error
	onList     func()
}

func (f *fakeDocker) ContainerList(
	_ context.Context,
	_ container.ListOptions,
) ([]container.Summary, error) {
	f.mu.Lock()
	f.calls = append(f.calls, "list")
	f.listTime = time.Now()
	f.mu.Unlock()

	if f.onList != nil {
		f.onList()
	}

	return f.containers, nil
}

func (f *fakeDocker) Events(
	_ context.Context,
	options events.ListOptions,
) (msgs <-chan events.Message, errs <-chan error) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.calls = append(f.calls, "events")
	f.sinces = append(f.sinces, options.Since)

	if len(f.sinces) == 1 && f.firstMsgs != nil {
		return f.firstMsgs, f.firstErrs
	}

	return make(chan events.Message), make(chan error)
}

func (f *fakeDocker) snapshot() (calls, sinces []string, listTime time.Time) {
	f.mu.Lock()
	defer f.mu.Unlock()

	return slices.Clone(f.calls), slices.Clone(f.sinces), f.listTime
}

// recordingInspector is a processor.DockerInspector that records inspected IDs
// and reports containers without a live pid, so ProcessContainer succeeds
// without touching cgroups.
type recordingInspector struct {
	mu  sync.Mutex
	ids []string
}

func (r *recordingInspector) ContainerInspect(
	_ context.Context,
	containerID string,
) (container.InspectResponse, error) {
	r.mu.Lock()
	r.ids = append(r.ids, containerID)
	r.mu.Unlock()

	return container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: nil},
	}, nil
}

func (*recordingInspector) ServiceInspectWithRaw(
	_ context.Context,
	_ string,
	_ swarm.ServiceInspectOptions,
) (swarm.Service, []byte, error) {
	return swarm.Service{}, nil, nil
}

func (r *recordingInspector) inspected() []string {
	r.mu.Lock()
	defer r.mu.Unlock()

	return slices.Clone(r.ids)
}

func waitFor(t *testing.T, cond func() bool) {
	t.Helper()

	deadline := time.Now().Add(2 * time.Second)
	for time.Now().Before(deadline) {
		if cond() {
			return
		}

		time.Sleep(5 * time.Millisecond)
	}

	t.Fatal("condition not met before deadline")
}

func noopWatcher(context.Context, Options) {}

// TestRun_SubscribesBeforeEnumerating checks that the event stream is opened
// (with Since not after the list time) before containers are listed, and that
// an event for a container started during enumeration is applied.
func TestRun_SubscribesBeforeEnumerating(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs := make(chan events.Message, 1)
	docker := &fakeDocker{
		containers: []container.Summary{{ID: "listed"}},
		firstMsgs:  msgs,
		firstErrs:  make(chan error),
	}
	docker.onList = func() {
		msgs <- events.Message{Actor: events.Actor{ID: "unlisted"}, TimeNano: time.Now().UnixNano()}
	}

	insp := &recordingInspector{}
	opts := Options{
		Docker: docker,
		Proc:   &processor.Processor{Inspector: insp, Cfg: config.NewStore()},
	}

	done := make(chan struct{})

	go func() {
		defer close(done)

		_ = run(ctx, opts, noopWatcher)
	}()

	waitFor(t, func() bool { return slices.Contains(insp.inspected(), "unlisted") })
	cancel()
	<-done

	calls, sinces, listTime := docker.snapshot()
	if len(calls) < 2 || calls[0] != "events" || calls[1] != "list" {
		t.Fatalf("calls = %v, want events before list", calls)
	}

	since, err := time.Parse(time.RFC3339Nano, sinces[0])
	if err != nil {
		t.Fatalf("parse Since %q: %v", sinces[0], err)
	}

	if since.After(listTime) {
		t.Errorf("Since %v is after list time %v", since, listTime)
	}

	if got := insp.inspected(); !slices.Equal(got, []string{"listed", "unlisted"}) {
		t.Errorf("inspected = %v, want [listed unlisted]", got)
	}
}

// TestListenEvents_ReconnectSinceAfterLastEvent checks that a re-subscription
// starts one nanosecond after the last received event.
func TestListenEvents_ReconnectSinceAfterLastEvent(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	lastEvent := time.Now().Add(-time.Minute)

	msgs := make(chan events.Message, 1)
	msgs <- events.Message{Actor: events.Actor{ID: "c1"}, TimeNano: lastEvent.UnixNano()}

	close(msgs)

	docker := &fakeDocker{}
	insp := &recordingInspector{}
	opts := Options{
		Docker: docker,
		Proc:   &processor.Processor{Inspector: insp, Cfg: config.NewStore()},
	}

	done := make(chan struct{})

	go func() {
		defer close(done)

		listenEvents(
			ctx,
			opts,
			map[string]time.Time{},
			time.Now().Add(-time.Hour),
			msgs,
			make(chan error),
		)
	}()

	waitFor(t, func() bool {
		_, sinces, _ := docker.snapshot()

		return len(sinces) == 1
	})
	cancel()
	<-done

	_, sinces, _ := docker.snapshot()

	since, err := time.Parse(time.RFC3339Nano, sinces[0])
	if err != nil {
		t.Fatalf("parse Since %q: %v", sinces[0], err)
	}

	if !since.Equal(lastEvent.Add(time.Nanosecond)) {
		t.Errorf("reconnect Since = %v, want %v", since, lastEvent.Add(time.Nanosecond))
	}
}

// TestListenEvents_ReconnectBeforeAnyEventUsesInitialSince checks the
// fallback when the stream drops before delivering any event.
func TestListenEvents_ReconnectBeforeAnyEventUsesInitialSince(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs := make(chan events.Message)
	close(msgs)

	docker := &fakeDocker{}
	initial := time.Now().Add(-time.Hour)
	opts := Options{
		Docker: docker,
		Proc:   &processor.Processor{Inspector: &recordingInspector{}, Cfg: config.NewStore()},
	}

	done := make(chan struct{})

	go func() {
		defer close(done)

		listenEvents(ctx, opts, map[string]time.Time{}, initial, msgs, make(chan error))
	}()

	waitFor(t, func() bool {
		_, sinces, _ := docker.snapshot()

		return len(sinces) == 1
	})
	cancel()
	<-done

	_, sinces, _ := docker.snapshot()
	if sinces[0] != formatSince(initial) {
		t.Errorf("reconnect Since = %q, want %q", sinces[0], formatSince(initial))
	}
}

var (
	errApplyFailed = errors.New("apply failed")

	recorderOnce sync.Once
	testRecorder *observability.Recorder
)

// sharedRecorder returns a Recorder registered once against the default
// Prometheus registry (registering twice panics).
func sharedRecorder() *observability.Recorder {
	recorderOnce.Do(func() { testRecorder = observability.NewRecorder() })

	return testRecorder
}

// metricsSnapshot holds the values processOne is responsible for.
type metricsSnapshot struct {
	appliedOK     float64
	appliedError  float64
	durationCount uint64
	lastEvent     float64
}

func readMetrics(t *testing.T) metricsSnapshot {
	t.Helper()

	families, err := prometheus.DefaultGatherer.Gather()
	if err != nil {
		t.Fatalf("gather metrics: %v", err)
	}

	var snap metricsSnapshot

	for _, family := range families {
		for _, metric := range family.GetMetric() {
			switch family.GetName() {
			case "sda_rules_applied_total":
				for _, label := range metric.GetLabel() {
					if label.GetName() != "result" {
						continue
					}

					if label.GetValue() == "ok" {
						snap.appliedOK = metric.GetCounter().GetValue()
					} else {
						snap.appliedError = metric.GetCounter().GetValue()
					}
				}
			case "sda_apply_duration_seconds":
				snap.durationCount = metric.GetHistogram().GetSampleCount()
			case "sda_last_event_timestamp_seconds":
				snap.lastEvent = metric.GetGauge().GetValue()
			}
		}
	}

	return snap
}

func captureLogs(t *testing.T) *bytes.Buffer {
	t.Helper()

	prev := logger.L()

	var buf bytes.Buffer
	logger.Set(slog.New(slog.NewTextHandler(&buf, &slog.HandlerOptions{Level: slog.LevelDebug})))
	t.Cleanup(func() { logger.Set(prev) })

	return &buf
}

func assertFailureReported(t *testing.T, before, after metricsSnapshot, logOutput, wantMsg string) {
	t.Helper()

	if after.appliedError-before.appliedError != 1 {
		t.Errorf(
			"rules_applied{result=error} delta = %v, want 1",
			after.appliedError-before.appliedError,
		)
	}

	if after.appliedOK != before.appliedOK {
		t.Errorf(
			"rules_applied{result=ok} changed on failure: %v -> %v",
			before.appliedOK,
			after.appliedOK,
		)
	}

	if after.durationCount-before.durationCount != 1 {
		t.Errorf(
			"apply_duration sample delta = %d, want 1",
			after.durationCount-before.durationCount,
		)
	}

	if after.lastEvent != before.lastEvent {
		t.Errorf(
			"last_event_timestamp changed on failure: %v -> %v",
			before.lastEvent,
			after.lastEvent,
		)
	}

	wantLine := false

	for line := range strings.SplitSeq(logOutput, "\n") {
		if strings.Contains(line, "level=WARN") &&
			strings.Contains(line, `msg="`+wantMsg+`"`) &&
			strings.Contains(line, "id=bad") &&
			strings.Contains(line, `err="apply failed"`) {
			wantLine = true
		}
	}

	if !wantLine {
		t.Errorf("expected WARN %q with id and err, got: %s", wantMsg, logOutput)
	}
}

// TestProcessOne_StartupFailureMatchesEventPath checks that a failing apply
// during the startup enumeration records the same metrics and WARN log as the
// same failure on the event path.
func TestProcessOne_StartupFailureMatchesEventPath(t *testing.T) {
	metrics := sharedRecorder()
	failingApply := func(_ context.Context, _ string) error { return errApplyFailed }

	buf := captureLogs(t)
	before := readMetrics(t)

	docker := &fakeDocker{containers: []container.Summary{{ID: "bad"}}}
	processed := map[string]time.Time{}

	err := processExistingContainers(context.Background(), docker, processed, metrics, failingApply)
	if err != nil {
		t.Fatalf("processExistingContainers: %v", err)
	}

	if _, recorded := processed["bad"]; recorded {
		t.Error("failed container must not be recorded as processed")
	}

	assertFailureReported(
		t,
		before,
		readMetrics(t),
		buf.String(),
		"could not process running container",
	)

	buf.Reset()

	before = readMetrics(t)

	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := minBackoff

	msgs <- events.Message{Actor: events.Actor{ID: "bad"}, TimeNano: time.Now().UnixNano()}

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(
		ctx,
		msgs,
		errs,
		map[string]time.Time{},
		&backoff,
		nil,
		new(int64),
		metrics,
		failingApply,
	)

	assertFailureReported(t, before, readMetrics(t), buf.String(), "could not process container")
}

// TestProcessOne_StartupSuccessRecordsMetrics checks the success path at
// startup: ok result, duration sample and last-event timestamp.
func TestProcessOne_StartupSuccessRecordsMetrics(t *testing.T) {
	metrics := sharedRecorder()
	before := readMetrics(t)

	docker := &fakeDocker{containers: []container.Summary{{ID: "good"}}}
	processed := map[string]time.Time{}

	err := processExistingContainers(context.Background(), docker, processed, metrics, noopApply)
	if err != nil {
		t.Fatalf("processExistingContainers: %v", err)
	}

	after := readMetrics(t)

	if after.appliedOK-before.appliedOK != 1 {
		t.Errorf("rules_applied{result=ok} delta = %v, want 1", after.appliedOK-before.appliedOK)
	}

	if after.durationCount-before.durationCount != 1 {
		t.Errorf(
			"apply_duration sample delta = %d, want 1",
			after.durationCount-before.durationCount,
		)
	}

	if after.lastEvent == 0 {
		t.Error("last_event_timestamp not set after successful startup apply")
	}

	if _, recorded := processed["good"]; !recorded {
		t.Error("successful container should be recorded as processed")
	}
}
