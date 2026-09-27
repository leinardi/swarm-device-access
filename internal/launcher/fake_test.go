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
	"bufio"
	"bytes"
	"context"
	"encoding/binary"
	"errors"
	"fmt"
	"io"
	"net"
	"strings"
	"sync"
	"testing"
	"time"

	cerrdefs "github.com/containerd/errdefs"
	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"
)

const (
	testLauncherID = "1111111111111111111111111111111111111111111111111111111111111111"
	testImageID    = "sha256:2222222222222222222222222222222222222222222222222222222222222222"
	testDaemonID   = "3333333333333333333333333333333333333333333333333333333333333333"

	// stdout and stderr stream types of the multiplexed attach stream.
	streamStdout = 1
	streamStderr = 2
)

var (
	errInjected = errors.New("injected failure")
	errDenied   = errors.New("permission denied")
)

// hijackedConn is the launcher's end of a fake attach connection: closing it
// ends the reader, as closing a real hijacked connection does.
type hijackedConn struct {
	net.Conn

	reader    *io.PipeReader
	closeOnce sync.Once
	closed    chan struct{}
}

func newHijackedConn(reader *io.PipeReader) *hijackedConn {
	return &hijackedConn{reader: reader, closed: make(chan struct{})}
}

func (c *hijackedConn) Close() error {
	c.closeOnce.Do(func() { close(c.closed) })

	return c.reader.Close() //nolint:wrapcheck // fake connection
}

type stopCall struct {
	id      string
	timeout int
}

// fakeDocker is an in-memory Docker engine for one launcher run.
type fakeDocker struct {
	mu    sync.Mutex
	calls []string

	inspect   map[string]client.ContainerInspectResult
	inspectEr map[string]error
	list      []container.Summary
	removed   []string
	created   client.ContainerCreateOptions
	stops     []stopCall

	createErr error
	attachErr error
	startErr  error
	// attachHang and waitHang block the call until its context ends.
	attachHang bool
	waitHang   bool
	// attachStuck blocks the attach, whatever its context, until it is
	// closed, like a dockerd that accepts the upgrade and never answers.
	attachStuck chan struct{}
	// stuckConn is the connection the stuck attach returns once released.
	stuckConn *hijackedConn
	// daemonGone reports the created daemon as removed to inspect.
	daemonGone bool
	// stop replaces the default stop, which ends the daemon.
	stop func(ctx context.Context)

	// The daemon's side of the attach stream and of the wait.
	output     *io.PipeWriter
	outputOnce sync.Once
	waitResult chan container.WaitResponse
	waitErr    chan error
	started    chan struct{}
}

func newFakeDocker() *fakeDocker {
	return &fakeDocker{
		inspect: map[string]client.ContainerInspectResult{
			testLauncherID: {Container: container.InspectResponse{
				ID:    testLauncherID,
				Image: testImageID,
				State: &container.State{Running: true},
			}},
		},
		inspectEr:  map[string]error{},
		waitResult: make(chan container.WaitResponse, 1),
		waitErr:    make(chan error, 1),
		started:    make(chan struct{}),
	}
}

func (f *fakeDocker) ContainerInspect(
	_ context.Context,
	containerID string,
	_ client.ContainerInspectOptions,
) (client.ContainerInspectResult, error) {
	f.record("inspect " + containerID)

	f.mu.Lock()
	defer f.mu.Unlock()

	err, failed := f.inspectEr[containerID]
	if failed {
		return client.ContainerInspectResult{}, err
	}

	if containerID == testDaemonID && !f.daemonGone {
		return client.ContainerInspectResult{Container: container.InspectResponse{
			ID:    testDaemonID,
			State: &container.State{Running: true},
		}}, nil
	}

	res, found := f.inspect[containerID]
	if !found {
		return client.ContainerInspectResult{}, fmt.Errorf(
			"no such container %s: %w",
			containerID,
			cerrdefs.ErrNotFound,
		)
	}

	return res, nil
}

func (f *fakeDocker) ContainerList(
	_ context.Context,
	_ client.ContainerListOptions,
) (client.ContainerListResult, error) {
	f.record("list")

	return client.ContainerListResult{Items: f.list}, nil
}

func (f *fakeDocker) ContainerRemove(
	_ context.Context,
	containerID string,
	_ client.ContainerRemoveOptions,
) (client.ContainerRemoveResult, error) {
	f.record("remove " + containerID)

	f.mu.Lock()
	f.removed = append(f.removed, containerID)
	f.mu.Unlock()

	return client.ContainerRemoveResult{}, nil
}

func (f *fakeDocker) ContainerCreate(
	_ context.Context,
	options client.ContainerCreateOptions,
) (client.ContainerCreateResult, error) {
	f.record("create")

	if f.createErr != nil {
		return client.ContainerCreateResult{}, f.createErr
	}

	f.mu.Lock()
	f.created = options
	f.mu.Unlock()

	return client.ContainerCreateResult{ID: testDaemonID}, nil
}

func (f *fakeDocker) ContainerAttach(
	ctx context.Context,
	_ string,
	_ client.ContainerAttachOptions,
) (client.ContainerAttachResult, error) {
	f.record("attach")

	if f.attachHang {
		<-ctx.Done()

		return client.ContainerAttachResult{}, fmt.Errorf("attach: %w", ctx.Err())
	}

	if f.attachStuck != nil {
		<-f.attachStuck

		reader, _ := io.Pipe()
		conn := newHijackedConn(reader)

		f.mu.Lock()
		f.stuckConn = conn
		f.mu.Unlock()

		return client.ContainerAttachResult{HijackedResponse: client.HijackedResponse{
			Conn:   conn,
			Reader: bufio.NewReader(reader),
		}}, nil
	}

	if f.attachErr != nil {
		return client.ContainerAttachResult{}, f.attachErr
	}

	reader, writer := io.Pipe()
	f.output = writer

	return client.ContainerAttachResult{HijackedResponse: client.HijackedResponse{
		Conn:   newHijackedConn(reader),
		Reader: bufio.NewReader(reader),
	}}, nil
}

func (f *fakeDocker) ContainerWait(
	ctx context.Context,
	_ string,
	_ client.ContainerWaitOptions,
) client.ContainerWaitResult {
	f.record("wait")

	if f.waitHang {
		<-ctx.Done()

		errs := make(chan error, 1)
		errs <- ctx.Err()

		return client.ContainerWaitResult{Result: make(chan container.WaitResponse), Error: errs}
	}

	// The stream only fails on its own when a test injects an error, or
	// when its context ends: a deadline on it would show up here.
	errs := make(chan error, 1)

	go func() {
		select {
		case err := <-f.waitErr:
			errs <- err
		case <-ctx.Done():
			errs <- ctx.Err()
		}
	}()

	return client.ContainerWaitResult{Result: f.waitResult, Error: errs}
}

func (f *fakeDocker) ContainerStart(
	_ context.Context,
	_ string,
	_ client.ContainerStartOptions,
) (client.ContainerStartResult, error) {
	f.record("start")

	if f.startErr != nil {
		return client.ContainerStartResult{}, f.startErr
	}

	close(f.started)

	return client.ContainerStartResult{}, nil
}

func (f *fakeDocker) ContainerStop(
	ctx context.Context,
	containerID string,
	options client.ContainerStopOptions,
) (client.ContainerStopResult, error) {
	f.record("stop " + containerID)

	timeout := -1
	if options.Timeout != nil {
		timeout = *options.Timeout
	}

	f.mu.Lock()
	f.stops = append(f.stops, stopCall{id: containerID, timeout: timeout})
	f.mu.Unlock()

	if f.stop != nil {
		f.stop(ctx)
	} else {
		f.emit(streamStdout, "daemon stopping")
		f.exit(0)
	}

	return client.ContainerStopResult{}, nil
}

func (f *fakeDocker) record(call string) {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.calls = append(f.calls, call)
}

func (f *fakeDocker) recorded() []string {
	f.mu.Lock()
	defer f.mu.Unlock()

	return append([]string(nil), f.calls...)
}

func (f *fakeDocker) stopCalls() []stopCall {
	f.mu.Lock()
	defer f.mu.Unlock()

	return append([]stopCall(nil), f.stops...)
}

func (f *fakeDocker) removedIDs() []string {
	f.mu.Lock()
	defer f.mu.Unlock()

	return append([]string(nil), f.removed...)
}

// emit writes one multiplexed line of daemon output. It returns once the
// launcher has read it.
func (f *fakeDocker) emit(stream byte, line string) {
	payload := []byte(line + "\n")
	size := uint32(len(payload)) //nolint:gosec // test lines are a few bytes

	var frame bytes.Buffer

	frame.Write([]byte{stream, 0, 0, 0})
	_ = binary.Write(&frame, binary.BigEndian, size)
	frame.Write(payload)

	_, _ = f.output.Write(frame.Bytes())
}

// closeOutput ends the attach stream, with an error when err is set.
func (f *fakeDocker) closeOutput(err error) {
	f.outputOnce.Do(func() { _ = f.output.CloseWithError(err) })
}

// exit ends the daemon's output and reports its removal with status.
func (f *fakeDocker) exit(status int64) {
	f.closeOutput(nil)
	f.markGone()

	f.waitResult <- container.WaitResponse{StatusCode: status}
}

// markGone makes inspect report the daemon removed.
func (f *fakeDocker) markGone() {
	f.mu.Lock()
	defer f.mu.Unlock()

	f.daemonGone = true
}

// syncBuffer is a bytes.Buffer safe for the copy goroutine and the test.
type syncBuffer struct {
	mu  sync.Mutex
	buf bytes.Buffer
}

func (b *syncBuffer) Write(data []byte) (int, error) {
	b.mu.Lock()
	defer b.mu.Unlock()

	return b.buf.Write(data) //nolint:wrapcheck // test buffer
}

func (b *syncBuffer) String() string {
	b.mu.Lock()
	defer b.mu.Unlock()

	return b.buf.String()
}

type runResult struct {
	status int
	err    error
}

type testRun struct {
	docker *fakeDocker
	runner *runner
	stdout *syncBuffer
	stderr *syncBuffer
}

func newTestRun(docker *fakeDocker) *testRun {
	stdout, stderr := &syncBuffer{}, &syncBuffer{}

	return &testRun{
		docker: docker,
		stdout: stdout,
		stderr: stderr,
		runner: &runner{
			docker:         docker,
			opts:           Options{HostDockerSocket: DefaultHostDockerSocket},
			stdout:         stdout,
			stderr:         stderr,
			selfID:         func() (string, error) { return testLauncherID, nil },
			callTimeout:    2 * time.Second,
			shutdownBudget: 5 * time.Second,
			pollInterval:   10 * time.Millisecond,
		},
	}
}

// start runs the launcher in the background.
func (tr *testRun) start(ctx context.Context) <-chan runResult {
	done := make(chan runResult, 1)

	go func() {
		status, err := tr.runner.run(ctx)
		done <- runResult{status: status, err: err}
	}()

	return done
}

// waitStarted blocks until the daemon container was started.
func (tr *testRun) waitStarted(t *testing.T) {
	t.Helper()

	select {
	case <-tr.docker.started:
	case <-time.After(2 * time.Second):
		t.Fatalf("daemon not started; calls: %v", tr.docker.recorded())
	}
}

// result waits for the launcher to return.
func result(t *testing.T, done <-chan runResult) runResult {
	t.Helper()

	select {
	case res := <-done:
		return res
	case <-time.After(10 * time.Second):
		t.Fatal("launcher did not return")

		return runResult{}
	}
}

func contains(calls []string, want string) bool {
	for _, call := range calls {
		if strings.HasPrefix(call, want) {
			return true
		}
	}

	return false
}
