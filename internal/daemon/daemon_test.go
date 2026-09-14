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
	"slices"
	"sync"
	"testing"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/swarm"

	"github.com/leinardi/swarm-device-access/internal/config"
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
