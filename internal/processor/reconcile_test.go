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
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"
	"time"

	cerrdefs "github.com/containerd/errdefs"
	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

const (
	reconcilePid     = 80
	reconcileStarted = "2026-09-27T10:00:00.000000000Z"
)

type inspectReply struct {
	resp container.InspectResponse
	err  error
}

// seqInspector answers the n-th ContainerInspect with replies[n], repeating
// the last reply once they run out.
type seqInspector struct {
	replies []inspectReply
	calls   int
}

func (s *seqInspector) ContainerInspect(
	_ context.Context,
	_ string,
	_ client.ContainerInspectOptions,
) (client.ContainerInspectResult, error) {
	reply := s.replies[min(s.calls, len(s.replies)-1)]
	s.calls++

	return client.ContainerInspectResult{Container: reply.resp}, reply.err
}

// then makes every later inspect answer with replies, in order.
func (s *seqInspector) then(replies ...inspectReply) {
	s.replies = replies
	s.calls = 0
}

func runningDevNull(started string) inspectReply {
	return inspectReply{resp: container.InspectResponse{
		State: &container.State{Running: true, Pid: reconcilePid, StartedAt: started},
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}
}

func exited() inspectReply {
	return inspectReply{resp: container.InspectResponse{
		State: &container.State{Running: false, StartedAt: reconcileStarted},
	}}
}

type reconcileEnv struct {
	proc   *Processor
	insp   *seqInspector
	fake   *failingCgroup
	pinner *fakePinner
	apis   int
}

func newReconcileEnv(t *testing.T, mode policy.Mode, dryRun bool) *reconcileEnv {
	t.Helper()

	env := &reconcileEnv{
		insp: &seqInspector{
			replies: []inspectReply{runningDevNull(reconcileStarted)},
		},
		fake:   &failingCgroup{},
		pinner: &fakePinner{},
	}
	env.proc = &Processor{
		Inspector: env.insp,
		Cfg:       newStore(mode, dryRun),
		HostRoot:  hostRootWithCgroup(t, reconcilePid),
		ProcRoot:  buildProcRoot(t, reconcilePid),
		pinner:    env.pinner,
		newCgroup: func(int, *cgroup.Ledger, *cgroup.FilterCache) (cgroup.Interface, error) {
			env.apis++

			return env.fake, nil
		},
	}

	return env
}

func (e *reconcileEnv) reconcile() error {
	return e.proc.Reconcile(context.Background(), "abc")
}

// grantOnce runs one successful reconcile that grants /dev/null, leaving the
// lifecycle's cgroup known.
func (e *reconcileEnv) grantOnce(t *testing.T) {
	t.Helper()

	err := e.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	if e.fake.calls != 1 || len(e.fake.rules[0]) != 1 {
		t.Fatalf(
			"grant: SetDeviceRules calls = %d with %v, want one call with /dev/null",
			e.fake.calls,
			e.fake.rules,
		)
	}

	if len(e.proc.known) != 1 {
		t.Fatalf("known = %v, want the lifecycle recorded", e.proc.known)
	}
}

// TestReconcile_PidRecycledBeforePinDoesNotMutate: the container exits right
// after the first inspect and its pid goes to an unrelated process in
// another cgroup. The pin and the /proc reads then describe that process,
// but the second inspect no longer reports the pid, so nothing is mutated
// and the foreign cgroup is not recorded.
func TestReconcile_PidRecycledBeforePinDoesNotMutate(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)

	other := filepath.Join(env.proc.HostRoot, "sys", "fs", "cgroup", "system.slice", "other")

	err := os.MkdirAll(other, 0o755)
	if err != nil {
		t.Fatal(err)
	}

	err = os.WriteFile(
		filepath.Join(other, "cgroup.procs"),
		[]byte(strconv.Itoa(reconcilePid)+"\n"),
		0o600,
	)
	if err != nil {
		t.Fatal(err)
	}

	err = os.WriteFile(
		filepath.Join(env.proc.ProcRoot, "proc", strconv.Itoa(reconcilePid), "cgroup"),
		[]byte("0::/system.slice/other\n"),
		0o600,
	)
	if err != nil {
		t.Fatal(err)
	}

	env.insp.then(runningDevNull(reconcileStarted), exited())

	err = env.reconcile()
	if !errors.Is(err, errLifecycleChanged) {
		t.Fatalf("err = %v, want errLifecycleChanged", err)
	}

	if env.fake.calls != 0 || len(env.proc.known) != 0 {
		t.Errorf("SetDeviceRules calls = %d, known = %v; want no mutation and nothing recorded",
			env.fake.calls, env.proc.known)
	}
}

func TestReconcile_StartedAtChangedDoesNotMutate(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.insp.then(
		runningDevNull(reconcileStarted),
		runningDevNull("2026-09-27T10:00:05.000000000Z"),
	)

	err := env.reconcile()
	if !errors.Is(err, errLifecycleChanged) {
		t.Fatalf("err = %v, want errLifecycleChanged", err)
	}

	if env.fake.calls != 0 {
		t.Errorf("SetDeviceRules calls = %d, want none", env.fake.calls)
	}
}

func TestReconcile_PinnedProcessGoneDoesNotMutate(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.pinner.gone = true

	err := env.reconcile()
	if !errors.Is(err, errPinnedProcessGone) {
		t.Fatalf("err = %v, want errPinnedProcessGone", err)
	}

	if env.fake.calls != 0 {
		t.Errorf("SetDeviceRules calls = %d, want none", env.fake.calls)
	}
}

// TestReconcile_MembershipGatesGrantsOnly: a pid missing from cgroup.procs
// blocks a grant, but not the empty set, which can only narrow.
func TestReconcile_MembershipGatesGrantsOnly(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	writeProcs(t, env.proc.HostRoot)

	err := env.reconcile()
	if !errors.Is(err, errNotInCgroup) || env.fake.calls != 0 {
		t.Fatalf(
			"grant: err = %v, calls = %d; want errNotInCgroup and no mutation",
			err,
			env.fake.calls,
		)
	}

	env.proc.Cfg = newStore(policy.ModeOptIn, false)

	err = env.reconcile()
	if err != nil || env.fake.calls != 1 || len(env.fake.rules[0]) != 0 {
		t.Fatalf(
			"empty set: err = %v, rules = %v; want one empty SetDeviceRules",
			err,
			env.fake.rules,
		)
	}
}

// TestReconcile_RevokeAfterExitWithMatchingInode: once the container has
// exited nothing can be pinned; the empty set goes to the cgroup the
// lifecycle was verified in, still the same directory, and the entry is
// kept until the cgroup is verified gone.
func TestReconcile_RevokeAfterExitWithMatchingInode(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)

	granted := env.fake.identity

	writeProcs(t, env.proc.HostRoot)
	env.insp.then(exited())

	err := env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	if env.fake.calls != 2 || len(env.fake.rules[1]) != 0 || env.fake.identity != granted {
		t.Fatalf("revoke: calls = %d, rules = %v, identity = %+v; want an empty set on %+v",
			env.fake.calls, env.fake.rules, env.fake.identity, granted)
	}

	if len(env.pinner.pins) != 1 {
		t.Errorf("pins = %v, want only the grant to pin", env.pinner.pins)
	}

	if len(env.proc.known) != 1 {
		t.Errorf("known = %v, want the entry kept until the cgroup is gone", env.proc.known)
	}
}

// TestReconcile_InodeMismatchReleasesWithoutMutation: the cgroup was
// recreated at the same path, so it holds none of the lifecycle's grants.
func TestReconcile_InodeMismatchReleasesWithoutMutation(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)

	// Build the replacement before removing the original, so the new
	// directory cannot reuse the old inode number.
	dir := testCgroupDir(env.proc.HostRoot)

	err := os.Mkdir(dir+".new", 0o755)
	if err != nil {
		t.Fatal(err)
	}

	err = os.RemoveAll(dir)
	if err != nil {
		t.Fatal(err)
	}

	err = os.Rename(dir+".new", dir)
	if err != nil {
		t.Fatal(err)
	}

	env.insp.then(exited())

	err = env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	if env.fake.calls != 1 {
		t.Errorf("SetDeviceRules calls = %d, want only the grant", env.fake.calls)
	}

	if len(env.proc.known) != 0 {
		t.Errorf("known = %v, want the entry released", env.proc.known)
	}
}

func TestReconcile_ExitAfterCgroupRemovedReleases(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)

	err := os.RemoveAll(testCgroupDir(env.proc.HostRoot))
	if err != nil {
		t.Fatal(err)
	}

	env.insp.then(exited())

	err = env.reconcile()
	if err != nil || env.fake.calls != 1 || len(env.proc.known) != 0 {
		t.Fatalf("err = %v, calls = %d, known = %v; want release without mutation",
			err, env.fake.calls, env.proc.known)
	}
}

// TestReconcile_InspectFailureRevokesKnownIdentity: without a pid the
// container's grants are revoked where its lifecycle was verified, and the
// container stays pending. With no known identity it is pending only.
func TestReconcile_InspectFailureRevokesKnownIdentity(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.insp.then(inspectReply{err: errDaemonUnavail})

	err := env.reconcile()
	if !errors.Is(err, errDaemonUnavail) || env.fake.calls != 0 {
		t.Fatalf(
			"unknown: err = %v, calls = %d; want pending with no mutation",
			err,
			env.fake.calls,
		)
	}

	env.insp.then(runningDevNull(reconcileStarted))
	env.grantOnce(t)
	env.insp.then(inspectReply{err: errDaemonUnavail})

	err = env.reconcile()
	if !errors.Is(err, errDaemonUnavail) {
		t.Fatalf("known: err = %v, want the inspect error (container stays pending)", err)
	}

	if env.fake.calls != 2 || len(env.fake.rules[1]) != 0 {
		t.Fatalf("known: rules = %v, want the empty set applied", env.fake.rules)
	}

	if len(env.proc.known) != 1 {
		t.Errorf("known = %v, want the entry kept (not-found alone never releases)", env.proc.known)
	}
}

func TestReconcile_DisabledContainerSetsEmptySet(t *testing.T) {
	buf := captureLogger(t)
	env := newReconcileEnv(t, policy.ModeOptIn, false)

	err := env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	if env.fake.calls != 1 || len(env.fake.rules[0]) != 0 {
		t.Fatalf("rules = %v, want one SetDeviceRules with the empty set", env.fake.rules)
	}

	if strings.Contains(buf.String(), "container processed") {
		t.Errorf(
			"a container skipped by policy must not also be reported processed:\n%s",
			buf.String(),
		)
	}
}

func TestReconcile_InvalidLabelsSetEmptySet(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	reply := runningDevNull(reconcileStarted)
	reply.resp.Config = &container.Config{Labels: map[string]string{policy.LabelEnable: "maybe"}}
	env.insp.then(reply)

	err := env.reconcile()
	if err != nil || env.fake.calls != 1 || len(env.fake.rules[0]) != 0 {
		t.Fatalf("err = %v, rules = %v; want the empty set applied", err, env.fake.rules)
	}
}

// TestReconcile_IncompleteDeviceSetSetsEmptySet: one unresolved device
// empties the whole set; the container is retried.
func TestReconcile_IncompleteDeviceSetSetsEmptySet(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	reply := runningDevNull(reconcileStarted)
	reply.resp.Mounts = append(
		reply.resp.Mounts,
		container.MountPoint{
			Source:      "/dev/sda-does-not-exist",
			Destination: "/dev/x",
			Type:        mount.TypeBind,
		},
	)
	env.insp.then(reply)

	err := env.reconcile()
	if err == nil || !strings.Contains(err.Error(), "incomplete_device_set") {
		t.Fatalf("err = %v, want a retryable incomplete_device_set error", err)
	}

	if env.fake.calls != 1 || len(env.fake.rules[0]) != 0 {
		t.Fatalf("rules = %v, want the empty set applied", env.fake.rules)
	}
}

// TestReconcile_DryRunIssuesNoCgroupCalls: dry-run computes and reports the
// set but pins nothing, reads no /proc and builds no cgroup API.
func TestReconcile_DryRunIssuesNoCgroupCalls(t *testing.T) {
	buf := captureLogger(t)
	env := newReconcileEnv(t, policy.ModeAll, true)
	env.proc.ProcRoot = filepath.Join(t.TempDir(), "absent")

	err := env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	env.insp.then(exited())

	err = env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	if env.apis != 0 || env.fake.calls != 0 || len(env.pinner.pins) != 0 {
		t.Errorf("apis = %d, calls = %d, pins = %v; want no cgroup or pid access",
			env.apis, env.fake.calls, env.pinner.pins)
	}

	if !strings.Contains(buf.String(), "dry-run: would set device rules") {
		t.Errorf("expected the would-set line, got:\n%s", buf.String())
	}
}

// TestReconcile_CancelledCallerKeepsGrants: an inspect that fails because
// the caller gave up (daemon shutdown) is not Docker failing; nothing would
// retry, so a running container must keep its grants.
func TestReconcile_CancelledCallerKeepsGrants(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)
	env.insp.then(inspectReply{err: context.Canceled})

	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	err := env.proc.Reconcile(ctx, "abc")
	if !errors.Is(err, context.Canceled) {
		t.Fatalf("err = %v, want context.Canceled", err)
	}

	if env.fake.calls != 1 {
		t.Errorf("SetDeviceRules calls = %d, want only the grant", env.fake.calls)
	}
}

// TestReconcile_EmptySetWithMissingFilterIsNotDone: an unprivileged
// container with no device filter is never reported done, whatever its
// desired set.
func TestReconcile_EmptySetWithMissingFilterIsNotDone(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeOptIn, false)
	env.fake.err = cgroup.ErrFilterMissing

	err := env.reconcile()
	if !errors.Is(err, cgroup.ErrFilterMissing) ||
		!strings.Contains(err.Error(), "filter_missing") {
		t.Fatalf("err = %v, want a retryable filter_missing error", err)
	}
}

// TestReconcile_SkippedContainerStillReportsIncompleteSet: a container
// skipped for good (privileged, no filter) keeps its incomplete-set error.
func TestReconcile_SkippedContainerStillReportsIncompleteSet(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.fake.err = cgroup.ErrFilterMissing
	reply := runningDevNull(reconcileStarted)
	reply.resp.HostConfig = &container.HostConfig{Privileged: true}
	reply.resp.Mounts = append(
		reply.resp.Mounts,
		container.MountPoint{
			Source:      "/dev/sda-does-not-exist",
			Destination: "/dev/x",
			Type:        mount.TypeBind,
		},
	)
	env.insp.then(reply)

	err := env.reconcile()
	if err == nil || !strings.Contains(err.Error(), "incomplete_device_set") {
		t.Fatalf("err = %v, want the incomplete_device_set error", err)
	}
}

// TestReconcile_DeadlineExceededRevokes: an inspect that ran out of time is
// Docker not answering, even when the caller's own deadline has passed too;
// the known grants are revoked.
func TestReconcile_DeadlineExceededRevokes(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)
	env.insp.then(inspectReply{err: context.DeadlineExceeded})

	ctx, cancel := context.WithDeadline(context.Background(), time.Now().Add(-time.Second))
	defer cancel()

	err := env.proc.Reconcile(ctx, "abc")
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("err = %v, want context.DeadlineExceeded (container stays pending)", err)
	}

	if env.fake.calls != 2 || len(env.fake.rules[1]) != 0 {
		t.Fatalf("rules = %v, want the empty set applied", env.fake.rules)
	}
}

// TestReconcile_InspectFailureOfPrivilegedDoesNotWarn: a privileged
// container never has a device filter, so a missing one is not reported.
func TestReconcile_InspectFailureOfPrivilegedDoesNotWarn(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	reply := runningDevNull(reconcileStarted)
	reply.resp.HostConfig = &container.HostConfig{Privileged: true}
	env.insp.then(reply)
	env.grantOnce(t)

	buf := captureLogger(t)
	env.fake.err = cgroup.ErrFilterMissing
	env.insp.then(inspectReply{err: errDaemonUnavail})

	err := env.reconcile()
	if !errors.Is(err, errDaemonUnavail) {
		t.Fatalf("err = %v, want the inspect error", err)
	}

	if strings.Contains(buf.String(), "restart the container") {
		t.Errorf("privileged container must not get the filter_missing warning:\n%s", buf.String())
	}
}

var errRevokeFailed = errors.New("revoke failed")

// TestReconcile_NotFoundIsContainerGone: a container Docker does not know
// is reported gone, so it is not retried forever, but only once nothing is
// left to revoke for it.
func TestReconcile_NotFoundIsContainerGone(t *testing.T) {
	notFound := inspectReply{err: fmt.Errorf("no such container: %w", cerrdefs.ErrNotFound)}

	t.Run("unknown", func(t *testing.T) {
		env := newReconcileEnv(t, policy.ModeAll, false)
		env.insp.then(notFound)

		err := env.reconcile()
		if !errors.Is(err, ErrContainerGone) || env.fake.calls != 0 {
			t.Fatalf(
				"err = %v, calls = %d; want ErrContainerGone and no mutation",
				err,
				env.fake.calls,
			)
		}
	})

	t.Run("dry-run", func(t *testing.T) {
		env := newReconcileEnv(t, policy.ModeAll, true)
		env.insp.then(notFound)

		err := env.reconcile()
		if !errors.Is(err, ErrContainerGone) {
			t.Fatalf("err = %v, want ErrContainerGone", err)
		}
	})

	t.Run("known and revoked", func(t *testing.T) {
		env := newReconcileEnv(t, policy.ModeAll, false)
		env.grantOnce(t)
		env.insp.then(notFound)

		err := env.reconcile()
		if !errors.Is(err, ErrContainerGone) || env.fake.calls != 2 || len(env.fake.rules[1]) != 0 {
			t.Fatalf(
				"err = %v, rules = %v; want the revoke, then ErrContainerGone",
				err,
				env.fake.rules,
			)
		}
	})

	t.Run("known and revoke failed", func(t *testing.T) {
		env := newReconcileEnv(t, policy.ModeAll, false)
		env.grantOnce(t)
		env.insp.then(notFound)
		env.fake.err = errRevokeFailed

		err := env.reconcile()
		if errors.Is(err, ErrContainerGone) || !errors.Is(err, errRevokeFailed) {
			t.Fatalf("err = %v, want the revoke failure and the container kept pending", err)
		}
	})
}
