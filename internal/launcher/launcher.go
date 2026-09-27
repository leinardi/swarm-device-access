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

// Package launcher runs the privileged daemon container from the image the
// launcher itself runs, and supervises it: the launcher is an unprivileged
// Swarm service task, so Swarm's rolling updates, rollbacks and image
// watchers act on the daemon too.
package launcher

import (
	"context"
	"errors"
	"fmt"
	"io"
	"log/slog"
	"os"
	"strings"
	"time"

	cerrdefs "github.com/containerd/errdefs"
	"github.com/moby/moby/api/pkg/stdcopy"
	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/daemon"
	"github.com/leinardi/swarm-device-access/internal/logger"
)

const (
	// DockerHost is the Docker endpoint the launcher uses: its own socket
	// mount, never a configurable path, so it cannot disagree with the
	// socket bound into the daemon.
	DockerHost = "unix://" + containerDockerSocket

	component = "launcher"

	// imageIDPrefix is the prefix of a content-addressed image ID. A tag
	// would let the daemon run a different image than the launcher.
	imageIDPrefix = "sha256:"

	// shutdownBudget bounds everything a shutdown does, the stop, the wait
	// for the removal and the log drain together. It stays inside the
	// compose files' stop_grace_period of 30s, so Swarm never kills a
	// launcher that is still stopping its daemon.
	shutdownBudget = stopTimeout + daemon.DockerCallTimeout
)

var (
	errEstablishTimeout = errors.New("not established before timeout")
	errNoImageID        = errors.New("launcher container has no content-addressed image ID")
	errWaitStream       = errors.New("daemon wait stream failed")
	errLogStream        = errors.New("daemon log stream lost while the daemon was running")
	errShutdownBudget   = errors.New("daemon not removed within the shutdown budget")
)

// dockerAPI is the subset of *client.Client the launcher uses.
type dockerAPI interface {
	ContainerInspect(
		ctx context.Context,
		containerID string,
		options client.ContainerInspectOptions,
	) (client.ContainerInspectResult, error)
	ContainerList(
		ctx context.Context,
		options client.ContainerListOptions,
	) (client.ContainerListResult, error)
	ContainerRemove(
		ctx context.Context,
		containerID string,
		options client.ContainerRemoveOptions,
	) (client.ContainerRemoveResult, error)
	ContainerCreate(
		ctx context.Context,
		options client.ContainerCreateOptions,
	) (client.ContainerCreateResult, error)
	ContainerAttach(
		ctx context.Context,
		containerID string,
		options client.ContainerAttachOptions,
	) (client.ContainerAttachResult, error)
	ContainerWait(
		ctx context.Context,
		containerID string,
		options client.ContainerWaitOptions,
	) client.ContainerWaitResult
	ContainerStart(
		ctx context.Context,
		containerID string,
		options client.ContainerStartOptions,
	) (client.ContainerStartResult, error)
	ContainerStop(
		ctx context.Context,
		containerID string,
		options client.ContainerStopOptions,
	) (client.ContainerStopResult, error)
}

type runner struct {
	docker dockerAPI
	opts   Options
	stdout io.Writer
	stderr io.Writer
	selfID func() (string, error)

	callTimeout    time.Duration
	shutdownBudget time.Duration
}

// session is a created daemon container with its log and wait streams.
type session struct {
	id       string
	hijacked client.HijackedResponse
	wait     client.ContainerWaitResult

	// copyDone reports the log copy's result once; copyFinished and copyErr
	// keep it, so later drains read them instead of the channel.
	copyDone     chan error
	copyFinished bool
	copyErr      error

	cancelAttach context.CancelFunc
	cancelWait   context.CancelFunc
}

// Run creates the daemon container from the launcher's own image, streams
// its output to the launcher's stdout and stderr, and returns its exit
// status once it is removed. ctx ends the daemon: it is stopped and Run
// still returns its status. An error means the launcher failed; the status
// is then 1.
func Run(ctx context.Context, docker dockerAPI, opts *Options) (int, error) {
	run := &runner{
		docker:         docker,
		opts:           *opts,
		stdout:         os.Stdout,
		stderr:         os.Stderr,
		selfID:         selfIDFromMountinfo,
		callTimeout:    daemon.DockerCallTimeout,
		shutdownBudget: shutdownBudget,
	}

	return run.run(ctx)
}

func launcherLog() *slog.Logger {
	return logger.L().With("component", component)
}

func (r *runner) run(ctx context.Context) (int, error) {
	launcherID, imageID, err := r.identify(ctx)
	if err != nil {
		return 1, err
	}

	err = r.removeStale(ctx, launcherID)
	if err != nil {
		return 1, err
	}

	createCtx, cancelCreate := context.WithTimeout(ctx, r.callTimeout)
	created, err := r.docker.ContainerCreate(createCtx, daemonSpec(&r.opts, imageID, launcherID))

	cancelCreate()

	if err != nil {
		return 1, fmt.Errorf("create daemon container: %w", err)
	}

	for _, warning := range created.Warnings {
		launcherLog().Warn("docker warning on daemon create", "warning", warning)
	}

	sess, err := r.prepare(ctx, created.ID)
	if err != nil {
		return 1, r.abandon(ctx, created.ID, err)
	}
	defer sess.close()

	startCtx, cancelStart := context.WithTimeout(ctx, r.callTimeout)
	_, err = r.docker.ContainerStart(startCtx, created.ID, client.ContainerStartOptions{})

	cancelStart()

	if err != nil {
		return 1, r.abandon(ctx, created.ID, fmt.Errorf("start daemon container: %w", err))
	}

	launcherLog().Info("daemon started",
		"id", shortID(created.ID), "image", imageID, "launcher", shortID(launcherID))

	return r.supervise(ctx, sess)
}

// identify returns the launcher's container ID and the ID of the image it
// runs. Swarm already pulled the task's digest, so no pull is needed.
func (r *runner) identify(ctx context.Context) (launcherID, imageID string, err error) {
	launcherID, err = r.selfID()
	if err != nil {
		return "", "", fmt.Errorf("identify launcher container: %w", err)
	}

	inspectCtx, cancel := context.WithTimeout(ctx, r.callTimeout)
	defer cancel()

	self, err := r.docker.ContainerInspect(inspectCtx, launcherID, client.ContainerInspectOptions{})
	if err != nil {
		return "", "", fmt.Errorf("inspect launcher container %s: %w", shortID(launcherID), err)
	}

	imageID = self.Container.Image
	if !strings.HasPrefix(imageID, imageIDPrefix) {
		return "", "", fmt.Errorf("%w: %q", errNoImageID, imageID)
	}

	return launcherID, imageID, nil
}

// abandon removes a daemon container whose setup failed, so a failed
// launcher never leaves a daemon behind.
func (r *runner) abandon(ctx context.Context, id string, cause error) error {
	return errors.Join(cause, r.removeContainer(ctx, id))
}

// prepare attaches to the daemon's output, starts copying it and registers
// the wait for its removal, all before the daemon starts, in the order the
// docker run CLI uses: nothing the daemon prints or its exit is missed, and
// the name is free when the wait returns.
func (r *runner) prepare(ctx context.Context, containerID string) (*session, error) {
	attached, cancelAttach, err := establish(ctx, r.callTimeout,
		func(streamCtx context.Context) (client.ContainerAttachResult, error) {
			return r.docker.ContainerAttach(streamCtx, containerID, client.ContainerAttachOptions{
				Stream: true,
				Stdout: true,
				Stderr: true,
			})
		},
		func(res client.ContainerAttachResult) { res.Close() },
	)
	if err != nil {
		return nil, fmt.Errorf("attach to daemon container: %w", err)
	}

	sess := &session{
		id:           containerID,
		hijacked:     attached.HijackedResponse,
		copyDone:     make(chan error, 1),
		cancelAttach: cancelAttach,
		cancelWait:   func() {},
	}

	// Started before the daemon: copying after ContainerStart returns could
	// block the daemon on a full attach buffer and deadlock the start.
	go func() {
		_, copyErr := stdcopy.StdCopy(r.stdout, r.stderr, sess.hijacked.Reader)
		sess.copyDone <- copyErr
	}()

	sess.wait, sess.cancelWait, err = establish(ctx, r.callTimeout,
		func(streamCtx context.Context) (client.ContainerWaitResult, error) {
			res := r.docker.ContainerWait(streamCtx, containerID, client.ContainerWaitOptions{
				Condition: container.WaitConditionRemoved,
			})

			// A request that failed outright reports it before returning.
			select {
			case waitErr := <-res.Error:
				return res, waitErr
			default:
				return res, nil
			}
		},
		func(client.ContainerWaitResult) {},
	)
	if err != nil {
		sess.close()

		return nil, fmt.Errorf("wait for daemon container: %w", err)
	}

	return sess, nil
}

// establish runs a streaming call with a bounded establishment. The stream
// gets its own context, detached from ctx: a signal must not cut the
// stream before the daemon's status and last lines arrive, and a deadline
// would kill it once established. The call is raced against a timer
// instead; on timeout (or ctx ending first) its context is canceled and the
// goroutine is waited for, and a result it still returns is released. On
// success the returned cancel func owns the stream context.
//
//nolint:ireturn // T is the concrete result of the one streaming call; generic over the two stream kinds
func establish[T any](
	ctx context.Context,
	timeout time.Duration,
	call func(context.Context) (T, error),
	release func(T),
) (T, context.CancelFunc, error) {
	type outcome struct {
		val T
		err error
	}

	streamCtx, cancel := context.WithCancel(context.WithoutCancel(ctx))
	done := make(chan outcome, 1)

	go func() {
		val, err := call(streamCtx)
		done <- outcome{val: val, err: err}
	}()

	timer := time.NewTimer(timeout)
	defer timer.Stop()

	var (
		zero  T
		cause error
	)

	select {
	case out := <-done:
		if out.err != nil {
			cancel()

			return zero, func() {}, out.err
		}

		return out.val, cancel, nil
	case <-timer.C:
		cause = errEstablishTimeout
	case <-ctx.Done():
		cause = fmt.Errorf("canceled: %w", ctx.Err())
	}

	cancel()

	out := <-done
	if out.err == nil {
		release(out.val)
	}

	return zero, func() {}, cause
}

// supervise waits for the first of: the daemon's removal, a broken wait
// stream, a lost log stream, or ctx ending.
func (r *runner) supervise(ctx context.Context, sess *session) (int, error) {
	for {
		select {
		case res := <-sess.wait.Result:
			drainCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), r.callTimeout)
			status := sess.finish(drainCtx, &res)

			cancel()

			return status, nil

		case waitErr := <-sess.wait.Error:
			budgetCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), r.shutdownBudget)
			r.stop(budgetCtx, sess.id)

			cancel()

			return 1, fmt.Errorf("%w: %w", errWaitStream, waitErr)

		case copyErr := <-sess.pendingCopy():
			sess.recordCopy(copyErr)

			if copyErr == nil {
				// End of stream: the daemon is exiting; its status follows.
				continue
			}

			launcherLog().Error("daemon log stream lost; stopping the daemon", "err", copyErr)

			_, shutdownErr := r.shutdown(ctx, sess)

			return 1, errors.Join(fmt.Errorf("%w: %w", errLogStream, copyErr), shutdownErr)

		case <-ctx.Done():
			launcherLog().Info("stopping daemon", "id", shortID(sess.id))

			return r.shutdown(ctx, sess)
		}
	}
}

// shutdown stops the daemon and waits for its removal and its last log
// lines, all within one shutdownBudget.
func (r *runner) shutdown(ctx context.Context, sess *session) (int, error) {
	budgetCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), r.shutdownBudget)
	defer cancel()

	r.stop(budgetCtx, sess.id)

	select {
	case res := <-sess.wait.Result:
		return sess.finish(budgetCtx, &res), nil
	case waitErr := <-sess.wait.Error:
		return 1, fmt.Errorf("%w: %w", errWaitStream, waitErr)
	case <-budgetCtx.Done():
		return 1, fmt.Errorf("%w (%s)", errShutdownBudget, r.shutdownBudget)
	}
}

// stop stops the daemon by ID, never by name, so a launcher cannot stop a
// daemon that a newer launcher created.
func (r *runner) stop(ctx context.Context, id string) {
	seconds := int(stopTimeout / time.Second)

	_, err := r.docker.ContainerStop(ctx, id, client.ContainerStopOptions{Timeout: &seconds})
	if err != nil && !cerrdefs.IsNotFound(err) {
		launcherLog().Warn("stop daemon failed", "id", shortID(id), "err", err)
	}
}

// finish drains the daemon's remaining output within ctx and returns its
// exit status.
func (s *session) finish(ctx context.Context, res *container.WaitResponse) int {
	if !s.copyFinished {
		select {
		case copyErr := <-s.copyDone:
			s.recordCopy(copyErr)
		case <-ctx.Done():
			launcherLog().Warn("daemon log stream not drained before timeout", "id", shortID(s.id))
		}
	}

	if s.copyErr != nil {
		launcherLog().Warn("daemon log stream ended with an error", "err", s.copyErr)
	}

	if res.Error != nil && res.Error.Message != "" {
		launcherLog().Warn("daemon wait reported an error", "err", res.Error.Message)
	}

	launcherLog().Info("daemon exited", "id", shortID(s.id), "status", res.StatusCode)

	return int(res.StatusCode)
}

// pendingCopy returns copyDone until its one result is recorded, then nil,
// which a select never receives from.
func (s *session) pendingCopy() <-chan error {
	if s.copyFinished {
		return nil
	}

	return s.copyDone
}

func (s *session) recordCopy(err error) {
	s.copyFinished = true
	s.copyErr = err
}

// close releases both streams.
func (s *session) close() {
	s.hijacked.Close()
	s.cancelAttach()
	s.cancelWait()
}
