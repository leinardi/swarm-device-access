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
	"io/fs"
	"time"

	cerrdefs "github.com/containerd/errdefs"
	"github.com/moby/moby/api/types/container"
	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/logger"
)

// ErrContainerGone reports that Docker no longer knows the container and no
// revoke is outstanding for it, so there is nothing left to retry.
var ErrContainerGone = errors.New("container no longer exists")

// applyPinned sets rules on the cgroup of the container's running process.
// It returns the cgroup path once resolved, for error reporting.
//
// The pid from inspect can be reused by another process before the cgroup
// is resolved, so the process is pinned: a pidfd is opened first, /proc/<pid>
// is read through one directory descriptor, and immediately before the
// mutation the pinned process must still be alive and a second inspect must
// report the same pid, start time and running state. While the pinned
// process is alive its pid cannot belong to anyone else, so the container
// still reporting that pid proves the reads were of the container's own
// process. A grant additionally requires the pid to be listed in the
// cgroup's cgroup.procs; an empty set is only ever narrowing.
func (p *Processor) applyPinned(
	ctx context.Context,
	containerID string,
	state *container.State,
	privileged bool,
	rules []cgroup.DeviceRule,
) (string, error) {
	log := logger.L()
	pid := state.Pid

	pinned, err := p.processPinner().pin(pid)
	if err != nil {
		return "", fmt.Errorf("container %q: %w", containerID, err)
	}

	defer func() {
		closeErr := pinned.Close()
		if closeErr != nil {
			log.Warn("close pidfd", "id", containerID, "err", closeErr)
		}
	}()

	resolved, err := readProcCgroup(p.ProcRoot, pid)
	if err != nil {
		return "", fmt.Errorf("container %q: %w", containerID, err)
	}

	cgroupPath := hostCGroupPath(
		p.HostRoot,
		resolved.MountPoint,
		resolved.MountPrefix,
		resolved.Root,
	)
	log.Debug("cgroup path resolved", "pid", pid, "version", resolved.Version, "path", cgroupPath)

	api, err := p.cgroupAPI(resolved.Version)
	if err != nil {
		return cgroupPath, err
	}

	handle, err := cgroup.OpenCgroup(cgroupPath)
	if err != nil {
		return cgroupPath, fmt.Errorf("set device rules: %w", err)
	}

	defer closeHandle(handle)

	err = p.verifyPinned(ctx, containerID, state, pinned, handle, len(rules) > 0)
	if err != nil {
		return cgroupPath, err
	}

	identity := handle.Identity()

	// The same run can move to another cgroup: a container running systemd
	// moves its processes into a child cgroup, and a grant left on the
	// parent keeps applying to every descendant. The grants at the recorded
	// cgroup go before the record is replaced; if they cannot, the record
	// keeps naming that cgroup and the container is retried. On cgroup v1 a
	// child cannot be granted what its parent denies, so such a run ends
	// up with no grants at all: fail closed.
	moved, ok := p.lifecycleStore().lookup(containerID, state.StartedAt)
	if ok && moved.HasIdentity() && moved.Identity != identity {
		err = p.revokeMoved(&moved, identity)
		if err != nil {
			return cgroupPath, err
		}

		// The revoke is real work between the check above and the
		// mutation below, and the run can move again meanwhile: check once
		// more, membership included even for the empty set, so the record
		// never names a cgroup the run was not verified in.
		err = p.verifyPinned(ctx, containerID, state, pinned, handle, true)
		if err != nil {
			return cgroupPath, err
		}
	}

	// Recorded after verification, so an identity resolved through a
	// recycled pid is never kept, and before the mutation, so a failed or
	// partial one can still be revoked here later.
	p.lifecycleStore().verify(&LifecycleRecord{
		ContainerID: containerID,
		StartedAt:   state.StartedAt,
		Identity:    identity,
		Version:     resolved.Version,
		Privileged:  privileged,
	})

	for _, rule := range rules {
		log.Debug("setting device rule",
			"pid", pid, "cgroup", cgroupPath,
			"type", rule.Type, "major", *rule.Major, "minor", *rule.Minor)
	}

	err = api.SetDeviceRules(handle, rules)
	if err != nil {
		return cgroupPath, fmt.Errorf("set device rules: %w", err)
	}

	return cgroupPath, nil
}

// verifyPinned is the check made immediately before a mutation; see
// applyPinned.
func (p *Processor) verifyPinned(
	ctx context.Context,
	containerID string,
	state *container.State,
	pinned pinnedProcess,
	handle *cgroup.CgroupHandle,
	grants bool,
) error {
	again, err := p.inspect(ctx, containerID)
	if err != nil {
		return fmt.Errorf("verify container %q: %w", containerID, err)
	}

	if again.State == nil || !again.State.Running ||
		again.State.Pid != state.Pid || again.State.StartedAt != state.StartedAt {
		return fmt.Errorf("container %q pid %d: %w", containerID, state.Pid, errLifecycleChanged)
	}

	err = pinned.alive()
	if err != nil {
		return fmt.Errorf("container %q pid %d: %w", containerID, state.Pid, err)
	}

	if !grants {
		return nil
	}

	member, err := handle.HasProcess(state.Pid)
	if err != nil {
		return fmt.Errorf("verify container %q: %w", containerID, err)
	}

	if !member {
		return fmt.Errorf("container %q pid %d: %w", containerID, state.Pid, errNotInCgroup)
	}

	return nil
}

// revokeAfterInspectFailure handles a container Docker cannot report on.
// The failure creates or refreshes a provisional record. Without a pid
// there is no cgroup to resolve, so every run of the container with a
// verified cgroup is revoked there. The container stays pending, unless
// Docker reported it does not exist and every revoke succeeded: the error
// then wraps ErrContainerGone (the records stay until the sweep verifies
// their cgroups gone). Dry-run never mutates, so for it a container Docker
// does not know is simply gone.
func (p *Processor) revokeAfterInspectFailure(
	containerID string,
	dryRun bool,
	inspectErr error,
) error {
	var errs []error

	if !dryRun {
		store := p.lifecycleStore()
		store.provisional(containerID)

		recs := store.Records(containerID)
		for idx := range recs {
			if !recs[idx].HasIdentity() {
				continue
			}

			// An ended run is not running, whatever Docker can say.
			revokeErr := p.revokeAt(&recs[idx], recs[idx].State != LifecycleTerminal)
			if revokeErr != nil {
				errs = append(errs, revokeErr)
			}
		}
	}

	if len(errs) == 0 && cerrdefs.IsNotFound(inspectErr) {
		return fmt.Errorf("%w: %w", ErrContainerGone, inspectErr)
	}

	return errors.Join(append([]error{inspectErr}, errs...)...)
}

// revokeEnded applies the empty set for a run whose process is gone, in
// the cgroup it was verified in, if any: nothing can be pinned any more,
// and the cgroup may already have been removed.
func (p *Processor) revokeEnded(containerID, startedAt string) error {
	store := p.lifecycleStore()

	rec, ok := store.lookup(containerID, startedAt)
	if !ok || !rec.HasIdentity() {
		return nil
	}

	store.markTerminal(containerID, startedAt)

	rec.State = LifecycleTerminal

	return p.revokeAt(&rec, false)
}

// Terminate handles an event that ended a run of the container at
// eventTime (die, destroy). The event names no run, so it is resolved
// against the history: the verified run with the latest start not after
// eventTime (never a newer run under the same ID), else the provisional
// records. The empty set is applied in that run's cgroup; the record stays
// until the sweep verifies the cgroup gone. Dry-run never mutates.
func (p *Processor) Terminate(containerID string, eventTime time.Time) error {
	p.mu.Lock()
	defer p.mu.Unlock()

	ended := p.lifecycleStore().terminate(containerID, eventTime)

	if p.Cfg.Load().DryRun {
		return nil
	}

	var errs []error

	for idx := range ended {
		if ended[idx].HasIdentity() {
			errs = append(errs, p.revokeAt(&ended[idx], false))
		}
	}

	return errors.Join(errs...)
}

// provisionalTTL is how long a provisional record without a cgroup is kept
// before the sweep drops it: it names no cgroup, so nothing can be revoked
// through it.
const provisionalTTL = 10 * time.Minute

// Sweep re-checks every lifecycle record: a record whose cgroup no longer
// exists, or was recreated with another inode, is released together with
// what the cgroup API remembered for it; an ended run whose revoke has not
// succeeded yet is revoked again; records that never named a cgroup are
// dropped once ended or stale. It returns how many records it released.
func (p *Processor) Sweep() int {
	p.mu.Lock()
	defer p.mu.Unlock()

	store := p.lifecycleStore()
	now := store.clock()
	dryRun := p.Cfg.Load().DryRun
	released := 0

	recs := store.all()
	for idx := range recs {
		rec := &recs[idx]

		if !rec.HasIdentity() {
			lastSeen := rec.FirstSeen
			if rec.Observed.After(lastSeen) {
				lastSeen = rec.Observed
			}

			if rec.State == LifecycleTerminal || now.Sub(lastSeen) > provisionalTTL {
				store.release(rec)

				released++
			}

			continue
		}

		gone, err := cgroupGone(rec.Identity)

		switch {
		case err != nil:
			logger.L().Warn("could not check container cgroup", "id", rec.ContainerID, "err", err)
		case gone:
			p.release(rec, "cgroup gone")

			released++
		case rec.State == LifecycleTerminal && !rec.Revoked && !dryRun:
			revokeErr := p.revokeAt(rec, false)
			if revokeErr != nil {
				logger.L().
					Warn("could not revoke ended container", "id", rec.ContainerID, "err", revokeErr)
			}
		}
	}

	return released
}

// cgroupGone reports whether the directory named by id no longer exists or
// was recreated (another inode).
func cgroupGone(identity cgroup.Identity) (bool, error) {
	var stat unix.Stat_t

	err := unix.Stat(identity.Path, &stat)
	if errors.Is(err, unix.ENOENT) {
		return true, nil
	}

	if err != nil {
		return false, fmt.Errorf("stat cgroup %q: %w", identity.Path, err)
	}

	return stat.Ino != identity.Inode, nil
}

// revokeAt re-opens the recorded path and applies the empty set only if it
// is still the same directory (same inode). A cgroup that is gone, or was
// recreated at the same path, holds none of the run's grants: the record is
// released and nothing is mutated. Otherwise the record is kept even after
// the revoke, until the sweep verifies the cgroup gone. mayRun reports that
// the container may still be running (Docker could not say).
func (p *Processor) revokeAt(rec *LifecycleRecord, mayRun bool) error {
	log := logger.L()

	handle, goneWhy, err := openRecorded(rec)
	if err != nil {
		return err
	}

	if goneWhy != "" {
		p.release(rec, goneWhy)

		return nil
	}

	defer closeHandle(handle)

	api, err := p.cgroupAPI(rec.Version)
	if err != nil {
		return err
	}

	err = api.SetDeviceRules(handle, nil)

	switch {
	case errors.Is(err, cgroup.ErrFilterMissing) && mayRun && !rec.Privileged:
		// Nothing attached holds a grant, but a container that may still be
		// running is unfiltered. The container stays pending either way.
		log.Warn("device filter missing and no cached original; restart the container",
			"id", rec.ContainerID, "cgroup", rec.Identity.Path, "reason", "filter_missing")
	case errors.Is(err, cgroup.ErrFilterMissing):
		// An exited or privileged container has nothing attached, so
		// nothing to revoke.
	case err != nil:
		return fmt.Errorf(
			"revoke container %q in %q: %w",
			rec.ContainerID,
			rec.Identity.Path,
			err,
		)
	}

	p.lifecycleStore().markRevoked(rec.ContainerID, rec.StartedAt)
	log.Debug("device grants revoked", "id", rec.ContainerID, "cgroup", rec.Identity.Path)

	return nil
}

// revokeMoved applies the empty set at the cgroup a still-running run was
// recorded in, once the run is verified in another one (current). The
// cgroup API then forgets that directory: no record names it any more.
// A missing filter means nothing attached there holds a grant.
func (p *Processor) revokeMoved(prev *LifecycleRecord, current cgroup.Identity) error {
	log := logger.L()

	handle, goneWhy, err := openRecorded(prev)
	if err != nil {
		return err
	}

	if goneWhy == "" {
		defer closeHandle(handle)

		api, err := p.cgroupAPI(prev.Version)
		if err != nil {
			return err
		}

		err = api.SetDeviceRules(handle, nil)
		if err != nil && !errors.Is(err, cgroup.ErrFilterMissing) {
			return fmt.Errorf(
				"revoke container %q in its previous cgroup %q: %w",
				prev.ContainerID,
				prev.Identity.Path,
				err,
			)
		}
	}

	log.Info("container moved to another cgroup; previous grants revoked",
		"id", prev.ContainerID, "from", prev.Identity.Path, "to", current.Path)

	p.ledger.Forget(prev.Identity)
	p.filterCache.Forget(prev.Identity)

	return nil
}

// openRecorded re-opens the cgroup a record names. goneWhy is set, and
// the handle nil, when the directory is gone or was recreated at the same
// path (another inode): either way it holds none of the run's grants.
func openRecorded(rec *LifecycleRecord) (handle *cgroup.CgroupHandle, goneWhy string, err error) {
	handle, err = cgroup.OpenCgroup(rec.Identity.Path)
	if errors.Is(err, fs.ErrNotExist) {
		return nil, "cgroup removed", nil
	}

	if err != nil {
		return nil, "", fmt.Errorf("revoke container %q: %w", rec.ContainerID, err)
	}

	if handle.Identity() != rec.Identity {
		closeHandle(handle)

		return nil, "cgroup recreated", nil
	}

	return handle, "", nil
}

// release forgets a run whose cgroup is verified gone, together with what
// the cgroup API remembered for that directory (the v1 ledger and the
// cached v2 filters).
func (p *Processor) release(rec *LifecycleRecord, why string) {
	logger.L().Debug("container cgroup gone; released",
		"id", rec.ContainerID, "cgroup", rec.Identity.Path, "why", why)

	p.lifecycleStore().release(rec)
	p.ledger.Forget(rec.Identity)
	p.filterCache.Forget(rec.Identity)
}

// SetLifecycles installs the lifecycle history the daemon's coordinator
// keeps; until then the processor keeps a private one.
func (p *Processor) SetLifecycles(lifecycles *Lifecycles) {
	p.mu.Lock()
	defer p.mu.Unlock()

	p.lifecycles = lifecycles
}

// lifecycleStore returns the lifecycle history; callers hold mu.
func (p *Processor) lifecycleStore() *Lifecycles {
	if p.lifecycles == nil {
		p.lifecycles = &Lifecycles{}
	}

	return p.lifecycles
}

//nolint:ireturn // processPinner is the seam that lets tests fake pidfds
func (p *Processor) processPinner() processPinner {
	if p.pinner == nil {
		return pidfdPinner{}
	}

	return p.pinner
}

//nolint:ireturn // intentional: callers use the interface
func (p *Processor) cgroupAPI(version int) (cgroup.Interface, error) {
	newCgroup := p.newCgroup
	if newCgroup == nil {
		newCgroup = cgroup.New
	}

	api, err := newCgroup(version, &p.ledger, &p.filterCache)
	if err != nil {
		return nil, fmt.Errorf("init cgroup api (version=%d): %w", version, err)
	}

	return api, nil
}

func closeHandle(handle *cgroup.CgroupHandle) {
	err := handle.Close()
	if err != nil {
		logger.L().Warn("close cgroup handle", "cgroup", handle.Identity().Path, "err", err)
	}
}

// startedAt is the lifecycle's start time, empty when inspect reported no
// state.
func startedAt(state *container.State) string {
	if state == nil {
		return ""
	}

	return state.StartedAt
}
