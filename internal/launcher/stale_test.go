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
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"
)

const (
	staleID = "4444444444444444444444444444444444444444444444444444444444444444"
	ownerID = "5555555555555555555555555555555555555555555555555555555555555555"
)

func TestRemoveStale(t *testing.T) {
	t.Parallel()

	daemonLabels := func(owner string) map[string]string {
		return map[string]string{LabelRole: RoleDaemon, LabelLauncher: owner}
	}

	owner := func(running bool) client.ContainerInspectResult {
		return client.ContainerInspectResult{Container: container.InspectResponse{
			ID:    ownerID,
			State: &container.State{Running: running},
		}}
	}

	cases := []struct {
		name        string
		labels      map[string]string
		owner       *client.ContainerInspectResult
		ownerErr    error
		wantErr     error
		wantRemoved bool
	}{
		{name: "launcher gone", labels: daemonLabels(ownerID), wantRemoved: true},
		{
			name:        "launcher exited",
			labels:      daemonLabels(ownerID),
			owner:       new(owner(false)),
			wantRemoved: true,
		},
		{
			name:    "launcher running",
			labels:  daemonLabels(ownerID),
			owner:   new(owner(true)),
			wantErr: errOwnedByRunningLauncher,
		},
		{
			name:     "launcher inspect times out",
			labels:   daemonLabels(ownerID),
			ownerErr: context.DeadlineExceeded,
			wantErr:  context.DeadlineExceeded,
		},
		{
			name:    "launcher state unknown",
			labels:  daemonLabels(ownerID),
			owner:   &client.ContainerInspectResult{},
			wantErr: errOwnerState,
		},
		{
			name:        "earlier run of this launcher",
			labels:      daemonLabels(testLauncherID),
			wantRemoved: true,
		},
		{
			name:    "no owner label",
			labels:  map[string]string{LabelRole: RoleDaemon},
			wantErr: errNoOwner,
		},
		{
			name:        "sh wrapper daemon",
			labels:      map[string]string{"org.opencontainers.image.title": "swarm-device-access"},
			wantRemoved: true,
		},
		{
			name:    "foreign container",
			labels:  map[string]string{"org.opencontainers.image.title": "something-else"},
			wantErr: errForeignContainer,
		},
		{name: "unlabelled container", wantErr: errForeignContainer},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			docker := newFakeDocker()
			docker.list = []container.Summary{{ID: staleID, Labels: tc.labels}}

			if tc.owner != nil {
				docker.inspect[ownerID] = *tc.owner
			}

			if tc.ownerErr != nil {
				docker.inspectEr[ownerID] = tc.ownerErr
			}

			run := newTestRun(docker)

			err := run.runner.removeStale(t.Context(), testLauncherID)
			if !errors.Is(err, tc.wantErr) {
				t.Fatalf("removeStale = %v, want %v", err, tc.wantErr)
			}

			var wantRemoved []string
			if tc.wantRemoved {
				wantRemoved = []string{staleID}
			}

			if got := docker.removedIDs(); !slices.Equal(got, wantRemoved) {
				t.Errorf("removed %v, want %v", got, wantRemoved)
			}
		})
	}
}

// A daemon owned by a live launcher stops the whole run before anything is
// created.
func TestRun_RefusesLiveDaemon(t *testing.T) {
	t.Parallel()

	docker := newFakeDocker()
	docker.list = []container.Summary{{
		ID:     staleID,
		Labels: map[string]string{LabelRole: RoleDaemon, LabelLauncher: ownerID},
	}}
	docker.inspect[ownerID] = client.ContainerInspectResult{Container: container.InspectResponse{
		ID:    ownerID,
		State: &container.State{Running: true},
	}}

	res := result(t, newTestRun(docker).start(t.Context()))
	if !errors.Is(res.err, errOwnedByRunningLauncher) || res.status != 1 {
		t.Fatalf("run = %d, %v; want 1, %v", res.status, res.err, errOwnedByRunningLauncher)
	}

	if contains(docker.recorded(), "create") || len(docker.removedIDs()) > 0 {
		t.Errorf("calls %v, want no create and no remove", docker.recorded())
	}
}
