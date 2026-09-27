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
	"fmt"

	cerrdefs "github.com/containerd/errdefs"
	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"
)

const (
	// labelImageTitle identifies a daemon started by the sh wrapper of
	// earlier releases, which recorded no owner.
	labelImageTitle = "org.opencontainers.image.title"
	imageTitle      = "swarm-device-access"
)

var (
	errOwnedByRunningLauncher = errors.New("daemon owned by running launcher")
	errNoOwner                = errors.New("daemon container has no launcher label")
	errOwnerState             = errors.New("inspect reported no state")
	errForeignContainer       = errors.New("container is not a swarm-device-access daemon")
)

// removeStale removes the daemon containers a previous launcher left behind,
// so the name is free for this one. A daemon is removed only when it
// provably has no live owner; any doubt is an error and leaves it alone.
func (r *runner) removeStale(ctx context.Context, selfID string) error {
	listCtx, cancel := context.WithTimeout(ctx, r.callTimeout)
	defer cancel()

	list, err := r.docker.ContainerList(listCtx, client.ContainerListOptions{
		All:     true,
		Filters: make(client.Filters).Add("name", "^/"+DaemonName+"$"),
	})
	if err != nil {
		return fmt.Errorf("list %s containers: %w", DaemonName, err)
	}

	for i := range list.Items {
		stale := &list.Items[i]

		err = r.checkStale(ctx, stale, selfID)
		if err != nil {
			return err
		}

		err = r.removeContainer(ctx, stale.ID)
		if err != nil {
			return fmt.Errorf("remove stale daemon %s: %w", shortID(stale.ID), err)
		}
	}

	return nil
}

// checkStale returns nil when stale may be removed: a daemon whose launcher
// is confirmed gone or stopped, one owned by this very launcher container
// (a restart of it), or one started by the old sh wrapper.
func (r *runner) checkStale(ctx context.Context, stale *container.Summary, selfID string) error {
	if stale.Labels[LabelRole] != RoleDaemon {
		if stale.Labels[labelImageTitle] == imageTitle {
			launcherLog().Info("removing daemon left by the sh wrapper",
				"id", shortID(stale.ID))

			return nil
		}

		return fmt.Errorf("%s (%s): %w", DaemonName, shortID(stale.ID), errForeignContainer)
	}

	owner := stale.Labels[LabelLauncher]
	if owner == "" {
		return fmt.Errorf("%s (%s): %w", DaemonName, shortID(stale.ID), errNoOwner)
	}

	if owner == selfID {
		launcherLog().Info("removing daemon left by an earlier run of this launcher",
			"id", shortID(stale.ID))

		return nil
	}

	inspectCtx, cancel := context.WithTimeout(ctx, r.callTimeout)
	defer cancel()

	inspected, err := r.docker.ContainerInspect(inspectCtx, owner, client.ContainerInspectOptions{})

	switch {
	case cerrdefs.IsNotFound(err):
	case err != nil:
		// A timeout or an engine error says nothing about whether the owner
		// still runs; only a confirmed absence makes the daemon an orphan.
		return fmt.Errorf(
			"inspect launcher %s of daemon %s: %w",
			shortID(owner),
			shortID(stale.ID),
			err,
		)
	case inspected.Container.State == nil:
		return fmt.Errorf(
			"launcher %s of daemon %s: %w",
			shortID(owner),
			shortID(stale.ID),
			errOwnerState,
		)
	case inspected.Container.State.Running:
		return fmt.Errorf("%w %s", errOwnedByRunningLauncher, shortID(owner))
	}

	launcherLog().Info("removing orphaned daemon",
		"id", shortID(stale.ID), "launcher", shortID(owner))

	return nil
}

// removeContainer force-removes containerID. It does not depend on ctx being live,
// so a failed setup can still clean up after a signal.
func (r *runner) removeContainer(ctx context.Context, containerID string) error {
	removeCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), r.callTimeout)
	defer cancel()

	_, err := r.docker.ContainerRemove(
		removeCtx,
		containerID,
		client.ContainerRemoveOptions{Force: true},
	)
	if err != nil && !cerrdefs.IsNotFound(err) {
		return fmt.Errorf("remove container %s: %w", shortID(containerID), err)
	}

	return nil
}

func shortID(fullID string) string {
	const shortLen = 12

	if len(fullID) > shortLen {
		return fullID[:shortLen]
	}

	return fullID
}
