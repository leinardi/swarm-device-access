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

package launcher

import (
	"context"
	"errors"
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"
)

func TestRun_CallOrder(t *testing.T) {
	t.Parallel()

	run := newTestRun(newFakeDocker())
	done := run.start(t.Context())
	run.waitStarted(t)
	run.docker.exit(0)

	res := result(t, done)
	if res.err != nil || res.status != 0 {
		t.Fatalf("run = %d, %v; want 0, nil", res.status, res.err)
	}

	want := []string{"inspect " + testLauncherID, "list", "create", "attach", "wait", "start"}
	if got := run.docker.recorded(); !slices.Equal(got, want) {
		t.Errorf("calls = %v, want %v", got, want)
	}

	created := run.docker.created
	if created.Name != DaemonName || created.Config.Image != testImageID ||
		created.Config.Labels[LabelLauncher] != testLauncherID {
		t.Errorf("created %+v, want the daemon from the launcher's image and ID", created)
	}
}

// The wait and attach streams have no deadline: a daemon that runs far
// longer than the call timeout keeps running, and its status is returned.
func TestRun_LongLivedDaemon(t *testing.T) {
	t.Parallel()

	const callTimeout = 20 * time.Millisecond

	run := newTestRun(newFakeDocker())
	run.runner.callTimeout = callTimeout

	done := run.start(t.Context())
	run.waitStarted(t)

	// Real elapsed window: well past the call timeout, which a deadline on
	// either stream would have hit.
	time.Sleep(10 * callTimeout)

	select {
	case res := <-done:
		t.Fatalf("launcher returned while the daemon runs: %d, %v", res.status, res.err)
	default:
	}

	run.docker.emit(streamStdout, "still here")
	run.docker.exit(3)

	res := result(t, done)
	if res.err != nil || res.status != 3 {
		t.Fatalf("run = %d, %v; want 3, nil", res.status, res.err)
	}

	if !strings.Contains(run.stdout.String(), "still here") {
		t.Errorf("stdout = %q, want the daemon's output", run.stdout.String())
	}
}

func TestRun_EstablishmentBounded(t *testing.T) {
	t.Parallel()

	for _, stream := range []string{"attach", "wait"} {
		t.Run(stream, func(t *testing.T) {
			t.Parallel()

			docker := newFakeDocker()
			docker.attachHang = stream == "attach"
			docker.waitHang = stream == "wait"

			run := newTestRun(docker)
			run.runner.callTimeout = 50 * time.Millisecond

			res := result(t, run.start(t.Context()))
			if !errors.Is(res.err, errEstablishTimeout) || res.status != 1 {
				t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errEstablishTimeout)
			}

			calls := docker.recorded()
			if contains(calls, "start") {
				t.Errorf("daemon started after a failed %s: %v", stream, calls)
			}

			if !slices.Equal(docker.removedIDs(), []string{testDaemonID}) {
				t.Errorf("removed %v, want the created daemon", docker.removedIDs())
			}
		})
	}
}

// A Docker call that ignores its canceled context (the hijacked attach reads
// the upgrade response on a raw connection) must not hold the launcher past
// the timeout; the result it returns later is released.
func TestRun_EstablishmentIgnoringCancel(t *testing.T) {
	t.Parallel()

	const callTimeout = 50 * time.Millisecond

	docker := newFakeDocker()
	docker.attachStuck = make(chan struct{})

	run := newTestRun(docker)
	run.runner.callTimeout = callTimeout

	began := time.Now()

	res := result(t, run.start(t.Context()))
	if !errors.Is(res.err, errEstablishTimeout) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errEstablishTimeout)
	}

	if elapsed := time.Since(began); elapsed > 20*callTimeout {
		t.Errorf("returned after %s, want soon after the %s timeout", elapsed, callTimeout)
	}

	if !slices.Equal(docker.removedIDs(), []string{testDaemonID}) {
		t.Errorf("removed %v, want the created daemon", docker.removedIDs())
	}

	close(docker.attachStuck)

	// Positive eventual: the late connection is closed by the background
	// release; closed is the observable.
	select {
	case <-waitStuckConn(t, docker).closed:
	case <-time.After(2 * time.Second):
		t.Error("late attach result was not released")
	}
}

// waitStuckConn polls until the stuck attach has returned its connection.
func waitStuckConn(t *testing.T, docker *fakeDocker) *hijackedConn {
	t.Helper()

	deadline := time.Now().Add(2 * time.Second)

	for time.Now().Before(deadline) {
		docker.mu.Lock()
		conn := docker.stuckConn
		docker.mu.Unlock()

		if conn != nil {
			return conn
		}

		time.Sleep(5 * time.Millisecond)
	}

	t.Fatal("stuck attach never returned")

	return nil
}

func TestRun_AttachErrorRemovesCreated(t *testing.T) {
	t.Parallel()

	docker := newFakeDocker()
	docker.attachErr = errInjected

	res := result(t, newTestRun(docker).start(t.Context()))
	if !errors.Is(res.err, errInjected) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errInjected)
	}

	if contains(docker.recorded(), "start") {
		t.Errorf("daemon started after a failed attach: %v", docker.recorded())
	}

	if !slices.Equal(docker.removedIDs(), []string{testDaemonID}) {
		t.Errorf("removed %v, want the created daemon", docker.removedIDs())
	}
}

func TestRun_StartErrorRemovesCreated(t *testing.T) {
	t.Parallel()

	docker := newFakeDocker()
	docker.startErr = errInjected

	res := result(t, newTestRun(docker).start(t.Context()))
	if !errors.Is(res.err, errInjected) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errInjected)
	}

	if !slices.Equal(docker.removedIDs(), []string{testDaemonID}) {
		t.Errorf("removed %v, want the created daemon", docker.removedIDs())
	}
}

// The last lines the daemon prints, after its removal is already reported,
// still reach the launcher's output before Run returns.
func TestRun_FinalLogsDrained(t *testing.T) {
	t.Parallel()

	run := newTestRun(newFakeDocker())
	done := run.start(t.Context())
	run.waitStarted(t)

	run.docker.emit(streamStdout, "out line")
	run.docker.emit(streamStderr, "err line")

	run.docker.waitResult <- container.WaitResponse{StatusCode: 2}

	run.docker.emit(streamStdout, "last out")
	run.docker.emit(streamStderr, "last err")
	run.docker.closeOutput(nil)

	res := result(t, done)
	if res.err != nil || res.status != 2 {
		t.Fatalf("run = %d, %v; want 2, nil", res.status, res.err)
	}

	if got := run.stdout.String(); got != "out line\nlast out\n" {
		t.Errorf("stdout = %q", got)
	}

	if got := run.stderr.String(); got != "err line\nlast err\n" {
		t.Errorf("stderr = %q", got)
	}
}

// A log stream that fails while the daemon runs would lose its logs
// silently: the launcher stops the daemon by ID and fails.
func TestRun_LogStreamLost(t *testing.T) {
	t.Parallel()

	run := newTestRun(newFakeDocker())
	run.docker.stop = func(context.Context) { run.docker.waitResult <- container.WaitResponse{} }

	done := run.start(t.Context())
	run.waitStarted(t)
	run.docker.closeOutput(errInjected)

	res := result(t, done)
	if !errors.Is(res.err, errLogStream) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errLogStream)
	}

	requireStopped(t, run.docker)
}

// An inspect that keeps failing while the removal is polled is reported
// with the budget, not replaced by it.
func TestRun_WaitStreamErrorKeepsInspectError(t *testing.T) {
	t.Parallel()

	run := newTestRun(newFakeDocker())
	run.runner.shutdownBudget = 100 * time.Millisecond
	run.docker.stop = func(context.Context) {}

	done := run.start(t.Context())
	run.waitStarted(t)

	run.docker.mu.Lock()
	run.docker.inspectEr[testDaemonID] = errDenied
	run.docker.mu.Unlock()

	run.docker.waitErr <- errInjected

	res := result(t, done)
	if !errors.Is(res.err, errShutdownBudget) || !errors.Is(res.err, errDenied) || res.status != 1 {
		t.Fatalf(
			"run = %d, %v; want 1, %v and %v",
			res.status,
			res.err,
			errShutdownBudget,
			errDenied,
		)
	}
}

// With the wait stream broken, the launcher still stops the daemon by ID,
// confirms its removal by inspecting it, and drains its last lines.
func TestRun_WaitStreamError(t *testing.T) {
	t.Parallel()

	const step = 50 * time.Millisecond

	run := newTestRun(newFakeDocker())
	run.docker.stop = func(context.Context) {
		go func() {
			time.Sleep(step)
			run.docker.emit(streamStdout, "daemon last words")
			run.docker.closeOutput(nil)
			time.Sleep(step)
			run.docker.markGone()
		}()
	}

	done := run.start(t.Context())
	run.waitStarted(t)

	run.docker.waitErr <- errInjected

	res := result(t, done)
	if !errors.Is(res.err, errWaitStream) || !errors.Is(res.err, errInjected) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errWaitStream)
	}

	if errors.Is(res.err, errShutdownBudget) {
		t.Errorf("removal not confirmed: %v", res.err)
	}

	requireStopped(t, run.docker)

	calls := run.docker.recorded()
	if !contains(calls[slices.Index(calls, "stop "+testDaemonID):], "inspect "+testDaemonID) {
		t.Errorf("calls %v, want the removal confirmed by inspect after the stop", calls)
	}

	if !strings.Contains(run.stdout.String(), "daemon last words") {
		t.Errorf("stdout = %q, want the daemon's last line", run.stdout.String())
	}
}

// A daemon that never disappears after a broken wait stream ends the
// shutdown at the budget.
func TestRun_WaitStreamErrorBudget(t *testing.T) {
	t.Parallel()

	const budget = 200 * time.Millisecond

	run := newTestRun(newFakeDocker())
	run.runner.shutdownBudget = budget
	run.docker.stop = func(context.Context) {}

	done := run.start(t.Context())
	run.waitStarted(t)

	began := time.Now()

	run.docker.waitErr <- errInjected

	res := result(t, done)
	if !errors.Is(res.err, errWaitStream) || !errors.Is(res.err, errShutdownBudget) ||
		res.status != 1 {
		t.Fatalf(
			"run = %d, %v; want 1, %v and %v",
			res.status,
			res.err,
			errWaitStream,
			errShutdownBudget,
		)
	}

	if elapsed := time.Since(began); elapsed > 3*budget {
		t.Errorf("shutdown took %s, want within the %s budget", elapsed, budget)
	}
}

// A signal stops the daemon by ID; the launcher still waits for its
// removal on the stream context, drains its output and returns its status.
func TestRun_CancelStopsDaemon(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()

	run := newTestRun(newFakeDocker())
	done := run.start(ctx)
	run.waitStarted(t)
	cancel()

	res := result(t, done)
	if res.err != nil || res.status != 0 {
		t.Fatalf("run = %d, %v; want 0, nil", res.status, res.err)
	}

	requireStopped(t, run.docker)

	if !strings.Contains(run.stdout.String(), "daemon stopping") {
		t.Errorf("stdout = %q, want the daemon's last line", run.stdout.String())
	}
}

// The stop, the wait for removal and the drain share one budget: each is
// slow, and together they still end within one budget, not the sum.
func TestRun_ShutdownBudgetShared(t *testing.T) {
	t.Parallel()

	const (
		budget     = 500 * time.Millisecond
		slowStop   = 200 * time.Millisecond
		slowRemove = 150 * time.Millisecond
	)

	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()

	run := newTestRun(newFakeDocker())
	run.runner.shutdownBudget = budget
	// Longer than the budget, so a drain bounded by it would show.
	run.runner.callTimeout = 10 * budget
	run.docker.stop = func(context.Context) {
		time.Sleep(slowStop)

		go func() {
			time.Sleep(slowRemove)

			run.docker.waitResult <- container.WaitResponse{StatusCode: 0}
		}()
		// The output is never closed: the drain runs out of budget.
	}

	done := run.start(ctx)
	run.waitStarted(t)

	began := time.Now()

	cancel()

	res := result(t, done)
	elapsed := time.Since(began)

	if res.err != nil || res.status != 0 {
		t.Fatalf("run = %d, %v; want 0, nil", res.status, res.err)
	}

	if elapsed > 2*budget {
		t.Errorf("shutdown took %s, want within one budget of %s", elapsed, budget)
	}
}

func TestRun_ShutdownBudgetExhausted(t *testing.T) {
	t.Parallel()

	ctx, cancel := context.WithCancel(t.Context())
	defer cancel()

	run := newTestRun(newFakeDocker())
	run.runner.shutdownBudget = 100 * time.Millisecond
	run.docker.stop = func(context.Context) {}

	done := run.start(ctx)
	run.waitStarted(t)
	cancel()

	res := result(t, done)
	if !errors.Is(res.err, errShutdownBudget) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errShutdownBudget)
	}

	run.docker.closeOutput(nil)
}

// An output stream that ends before the removal is reported leaves nothing
// to drain: the status is returned at once, not after a drain timeout.
func TestRun_EOFBeforeExit(t *testing.T) {
	t.Parallel()

	run := newTestRun(newFakeDocker())
	run.runner.callTimeout = 10 * time.Second

	done := run.start(t.Context())
	run.waitStarted(t)
	run.docker.closeOutput(nil)

	began := time.Now()

	run.docker.waitResult <- container.WaitResponse{StatusCode: 5}

	res := result(t, done)
	if res.err != nil || res.status != 5 {
		t.Fatalf("run = %d, %v; want 5, nil", res.status, res.err)
	}

	if elapsed := time.Since(began); elapsed > 2*time.Second {
		t.Errorf("returned after %s, want at once", elapsed)
	}
}

func TestRun_SetupErrorsStartNothing(t *testing.T) {
	t.Parallel()

	cases := map[string]func(*fakeDocker, *runner){
		"self id": func(_ *fakeDocker, r *runner) {
			r.selfID = func() (string, error) { return "", errInjected }
		},
		"self inspect": func(f *fakeDocker, _ *runner) {
			f.inspectEr[testLauncherID] = errInjected
		},
		"image tag instead of id": func(f *fakeDocker, _ *runner) {
			f.inspect[testLauncherID] = client.ContainerInspectResult{
				Container: container.InspectResponse{
					Image: "ghcr.io/leinardi/swarm-device-access:1",
				},
			}
		},
		"create": func(f *fakeDocker, _ *runner) {
			f.createErr = errInjected
		},
	}

	for name, setup := range cases {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			docker := newFakeDocker()
			run := newTestRun(docker)
			setup(docker, run.runner)

			res := result(t, run.start(t.Context()))
			if res.err == nil || res.status != 1 {
				t.Fatalf("run = %d, %v; want 1 and an error", res.status, res.err)
			}

			for _, call := range []string{"attach", "start"} {
				if contains(docker.recorded(), call) {
					t.Errorf("%s called: %v", call, docker.recorded())
				}
			}
		})
	}
}

func requireStopped(t *testing.T, docker *fakeDocker) {
	t.Helper()

	stops := docker.stopCalls()
	if len(stops) != 1 || stops[0].id != testDaemonID || stops[0].timeout != 10 {
		t.Errorf("stops = %+v, want one of %s with timeout 10", stops, testDaemonID)
	}
}
