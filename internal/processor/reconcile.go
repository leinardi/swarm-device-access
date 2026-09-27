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

	"github.com/moby/moby/api/types/container"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/logger"
)

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

	// Recorded after verification, so an identity resolved through a
	// recycled pid is never kept, and before the mutation, so a failed or
	// partial one can still be revoked here later.
	p.recordKnown(
		lifecycleKey{containerID: containerID, startedAt: state.StartedAt},
		knownCgroup{identity: handle.Identity(), version: resolved.Version, privileged: privileged},
	)

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
// Without a pid there is no cgroup to resolve, so every lifecycle of the
// container with a known cgroup is revoked there, and the container stays
// pending either way. Dry-run never mutates.
func (p *Processor) revokeAfterInspectFailure(
	containerID string,
	dryRun bool,
	inspectErr error,
) error {
	errs := []error{inspectErr}

	if dryRun {
		return inspectErr
	}

	for key, known := range p.known {
		if key.containerID == containerID {
			errs = append(errs, p.revokeAt(key, known, true))
		}
	}

	return errors.Join(errs...)
}

// revokeKnown applies the empty set to the cgroup key was last verified in,
// if any. It serves a lifecycle whose process is gone: nothing can be
// pinned any more, and the cgroup may already have been removed.
func (p *Processor) revokeKnown(key lifecycleKey) error {
	known, ok := p.known[key]
	if !ok {
		return nil
	}

	return p.revokeAt(key, known, false)
}

// revokeAt re-opens the recorded path and applies the empty set only if it
// is still the same directory (same inode). A cgroup that is gone, or was
// recreated at the same path, holds none of the lifecycle's grants: the
// entry is released and nothing is mutated. Otherwise the entry is kept
// even after the revoke, until the cgroup is verified gone. mayRun reports
// that the container may still be running (Docker could not say).
func (p *Processor) revokeAt(key lifecycleKey, known knownCgroup, mayRun bool) error {
	log := logger.L()

	handle, err := cgroup.OpenCgroup(known.identity.Path)
	if errors.Is(err, fs.ErrNotExist) {
		p.release(key, known, "cgroup removed")

		return nil
	}

	if err != nil {
		return fmt.Errorf("revoke container %q: %w", key.containerID, err)
	}

	defer closeHandle(handle)

	if handle.Identity() != known.identity {
		p.release(key, known, "cgroup recreated")

		return nil
	}

	api, err := p.cgroupAPI(known.version)
	if err != nil {
		return err
	}

	err = api.SetDeviceRules(handle, nil)

	switch {
	case errors.Is(err, cgroup.ErrFilterMissing) && mayRun && !known.privileged:
		// Nothing attached holds a grant, but a container that may still be
		// running is unfiltered. The container stays pending either way.
		log.Warn("device filter missing and no cached original; restart the container",
			"id", key.containerID, "cgroup", known.identity.Path, "reason", "filter_missing")
	case errors.Is(err, cgroup.ErrFilterMissing):
		// An exited or privileged container has nothing attached, so
		// nothing to revoke.
	case err != nil:
		return fmt.Errorf(
			"revoke container %q in %q: %w",
			key.containerID,
			known.identity.Path,
			err,
		)
	}

	log.Debug("device grants revoked", "id", key.containerID, "cgroup", known.identity.Path)

	return nil
}

func (p *Processor) recordKnown(key lifecycleKey, known knownCgroup) {
	if p.known == nil {
		p.known = make(map[lifecycleKey]knownCgroup)
	}

	p.known[key] = known
}

// release forgets a lifecycle whose cgroup is verified gone, together with
// what the cgroup API remembered for that directory.
func (p *Processor) release(key lifecycleKey, known knownCgroup, why string) {
	logger.L().Debug("container cgroup gone; released",
		"id", key.containerID, "cgroup", known.identity.Path, "why", why)

	delete(p.known, key)
	p.ledger.Forget(known.identity)
	p.filterCache.Forget(known.identity)
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
