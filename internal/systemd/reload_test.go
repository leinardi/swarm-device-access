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

package systemd

import (
	"context"
	"sync/atomic"
	"testing"

	"github.com/godbus/dbus/v5"
)

func TestIsReloadCompleted(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name string
		sig  *dbus.Signal
		want bool
	}{
		{
			name: "completion edge (active=false)",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{false},
			},
			want: true,
		},
		{
			name: "start edge (active=true)",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{true},
			},
			want: false,
		},
		{
			name: "nil signal",
			sig:  nil,
			want: false,
		},
		{
			name: "wrong signal name",
			sig: &dbus.Signal{
				Name: "org.freedesktop.systemd1.Manager.UnitNew",
				Body: []any{false},
			},
			want: false,
		},
		{
			name: "empty body",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{},
			},
			want: false,
		},
		{
			name: "non-boolean body",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{"not-a-bool"},
			},
			want: false,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got := isReloadCompleted(tc.sig)
			if got != tc.want {
				t.Errorf("isReloadCompleted() = %v, want %v", got, tc.want)
			}
		})
	}
}

const testOwner = ":1.1"

func completed(sender string, path dbus.ObjectPath) *dbus.Signal {
	return &dbus.Signal{Sender: sender, Path: path, Name: reloadingFullName, Body: []any{false}}
}

func ownerChanged(sender, newOwner string) *dbus.Signal {
	return &dbus.Signal{
		Sender: sender,
		Path:   busDaemonPath,
		Name:   nameOwnerChangedFullName,
		Body:   []any{systemdBusName, testOwner, newOwner},
	}
}

// runWatch feeds signals one by one through an unbuffered channel, so each
// has been handled before the next is sent, and returns how many times
// onReload ran once watch has returned.
func runWatch(t *testing.T, signals ...*dbus.Signal) int {
	t.Helper()

	var calls atomic.Int32

	sigCh := make(chan *dbus.Signal)
	done := make(chan struct{})

	go func() {
		defer close(done)

		watch(context.Background(), sigCh, testOwner, func() { calls.Add(1) })
	}()

	for _, sig := range signals {
		sigCh <- sig
	}

	close(sigCh)
	<-done

	return int(calls.Load())
}

func TestWatch_OnlySystemdCounts(t *testing.T) {
	t.Parallel()

	for name, sig := range map[string]*dbus.Signal{
		"foreign sender":        completed(":1.99", systemdObjectPath),
		"well-known name spoof": completed(systemdBusName, systemdObjectPath),
		"foreign object path":   completed(testOwner, "/org/example/evil"),
		"start edge from owner": {Sender: testOwner, Path: systemdObjectPath, Name: reloadingFullName, Body: []any{true}},
	} {
		if calls := runWatch(t, sig); calls != 0 {
			t.Errorf("%s: onReload ran %d times, want 0", name, calls)
		}
	}

	// A peer claiming the name changed hands to it is not believed.
	if calls := runWatch(
		t,
		ownerChanged(":1.99", ":1.99"),
		completed(":1.99", systemdObjectPath),
	); calls != 0 {
		t.Errorf("owner change by a peer: onReload ran %d times, want 0", calls)
	}

	if calls := runWatch(t, completed(testOwner, systemdObjectPath)); calls != 1 {
		t.Errorf("systemd's own completion: onReload ran %d times, want 1", calls)
	}
}

// A peer that claims the owner changed must not be believed; the bus
// daemon's NameOwnerChanged moves trust to systemd's new unique name.
func TestWatch_FollowsOwnerChangeFromTheBusDaemon(t *testing.T) {
	t.Parallel()

	calls := runWatch(t,
		ownerChanged(":1.99", ":1.99"),
		completed(":1.99", systemdObjectPath),
		ownerChanged(busDaemonName, ":1.5"),
		completed(testOwner, systemdObjectPath),
		completed(":1.5", systemdObjectPath),
		ownerChanged(busDaemonName, ""),
		completed(":1.5", systemdObjectPath),
	)
	if calls != 1 {
		t.Errorf("onReload ran %d times, want 1 (only :1.5 while it owned the name)", calls)
	}
}

// Reloads that complete while a re-apply runs collapse into one more run
// after it.
func TestWatch_CoalescesWhileRunning(t *testing.T) {
	t.Parallel()

	var calls atomic.Int32

	started := make(chan struct{})
	release := make(chan struct{})

	onReload := func() {
		if calls.Add(1) == 1 {
			close(started)
			<-release
		}
	}

	sigCh := make(chan *dbus.Signal)
	done := make(chan struct{})

	go func() {
		defer close(done)

		watch(context.Background(), sigCh, testOwner, onReload)
	}()

	sigCh <- completed(testOwner, systemdObjectPath)

	<-started

	for range 3 {
		sigCh <- completed(testOwner, systemdObjectPath)
	}

	// Handled only after the third completion's trigger returned.
	sigCh <- completed(":1.99", systemdObjectPath)

	close(release)
	close(sigCh)
	<-done

	if got := calls.Load(); got != 2 {
		t.Errorf("onReload ran %d times, want 2 (the running one and one trailing run)", got)
	}
}
