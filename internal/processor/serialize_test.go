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

package processor

import (
	"context"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// countingInspector is a concurrency-safe DockerInspector that counts calls.
type countingInspector struct {
	result container.InspectResponse
	calls  atomic.Int32
}

func (c *countingInspector) ContainerInspect(
	_ context.Context,
	_ string,
	_ client.ContainerInspectOptions,
) (client.ContainerInspectResult, error) {
	c.calls.Add(1)

	return client.ContainerInspectResult{Container: c.result}, nil
}

// TestReconcile_SerializesConfigLoadThroughApply holds one worker
// between computing its rules and applying them, then starts a second worker
// for the same container and publishes a new config. The second worker must
// not load the config or compute anything until the first has finished, so
// no computation can be overtaken by an older one, and must then compute
// under the new config.
func TestReconcile_SerializesConfigLoadThroughApply(t *testing.T) {
	const pid = 60

	insp := &countingInspector{
		result: container.InspectResponse{State: &container.State{Running: true, Pid: pid}},
	}
	store, publisher := newPublishedStore(policy.ModeAll, true)
	proc := &Processor{
		Inspector: insp,
		Cfg:       store,
		HostRoot:  "/host",
		ProcRoot:  buildProcRoot(t, pid),
	}

	var (
		computed    atomic.Int32
		modes       []policy.Mode
		modesMu     sync.Mutex
		firstInside = make(chan struct{})
		release     = make(chan struct{})
	)

	proc.afterCompute = func() {
		modesMu.Lock()

		modes = append(modes, store.Load().Policy.Mode)
		modesMu.Unlock()

		if computed.Add(1) == 1 {
			close(firstInside)
			<-release
		}
	}

	var workers sync.WaitGroup

	workers.Go(func() { _ = proc.Reconcile(context.Background(), "c1") })

	<-firstInside

	workers.Go(func() { _ = proc.Reconcile(context.Background(), "c1") })

	// Positive signal first: the second worker has finished its inspect, so it
	// is at (or past) the lock.
	deadline := time.Now().Add(2 * time.Second)
	for insp.calls.Load() < 2 {
		if time.Now().After(deadline) {
			t.Fatal("second worker never inspected the container")
		}

		time.Sleep(time.Millisecond)
	}

	// Published directly, not through PublishAndReconcile: this test is
	// about the lock ordering between workers, and the publication must
	// land while the first worker holds the lock.
	publisher.Publish(config.Runtime{Policy: policy.Global{Mode: policy.ModeOptIn}, DryRun: true})

	// Negative assertion: nothing can be polled for "did not happen", so give
	// the second worker a bounded window to (wrongly) get past the lock.
	time.Sleep(50 * time.Millisecond)

	if got := computed.Load(); got != 1 {
		t.Fatalf("second worker computed while the first held the lock (computed=%d)", got)
	}

	close(release)
	workers.Wait()

	// The second worker computes (the empty set, for an unlabeled container
	// under opt-in) from the config published while it waited.
	if len(modes) != 2 || modes[0] != policy.ModeAll || modes[1] != policy.ModeOptIn {
		t.Fatalf("modes seen by computing workers = %v, want [all opt-in]", modes)
	}
}

// TestPublishAndReconcile_StalePolicyRace pauses a worker after it computed
// a grant under the old config, publishes a narrower config through
// PublishAndReconcile, then releases the worker. The publication waits for
// the worker, and the pass it requests runs afterwards, so the cgroup ends
// with the narrower (empty) set rather than the stale grant.
func TestPublishAndReconcile_StalePolicyRace(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)

	store, publisher := newPublishedStore(policy.ModeAll, false)
	env.proc.Cfg = store
	env.proc.Publisher = publisher

	var (
		held    = make(chan struct{})
		release = make(chan struct{})
		once    sync.Once
	)

	env.proc.afterCompute = func() {
		once.Do(func() {
			close(held)
			<-release
		})
	}

	// The pass the coordinator would run: reconcile the one container.
	passErrs := make(chan error, 1)

	env.proc.SetPassRequester(func(ctx context.Context, _ uint64) {
		passErrs <- env.proc.Reconcile(ctx, "abc")
	})

	var workers sync.WaitGroup

	workers.Go(func() { _ = env.reconcile() })

	<-held

	published := make(chan uint64, 1)

	workers.Go(func() {
		published <- env.proc.PublishAndReconcile(
			context.Background(),
			config.Runtime{Policy: policy.Global{Mode: policy.ModeOptIn}},
		)
	})

	// Negative assertion: the publication must wait for the worker.
	select {
	case <-published:
		t.Fatal("PublishAndReconcile published while a worker held the lock")
	case <-time.After(50 * time.Millisecond):
	}

	close(release)
	workers.Wait()

	if gen := <-published; gen != 2 {
		t.Errorf("generation = %d, want 2", gen)
	}

	passErr := <-passErrs
	if passErr != nil {
		t.Fatalf("pass: %v", passErr)
	}

	last := env.fake.rules[len(env.fake.rules)-1]
	if env.fake.calls != 2 || len(env.fake.rules[0]) != 1 || len(last) != 0 {
		t.Fatalf("rules = %v, want the stale grant replaced by the empty set", env.fake.rules)
	}
}
