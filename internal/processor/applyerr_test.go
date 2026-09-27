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
	"strings"
	"syscall"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// failingCgroup is a cgroup.Interface whose mutation returns err. The path
// lookups delegate to the real v2 implementation, which reads the fake /proc.
type failingCgroup struct {
	cgroup.Interface

	err      error
	calls    int
	identity cgroup.Identity
}

func (f *failingCgroup) SetDeviceRules(handle *cgroup.CgroupHandle, _ []cgroup.DeviceRule) error {
	f.calls++
	f.identity = handle.Identity()

	return f.err
}

// hostRootWithCgroup returns a host root holding the cgroup directory that
// buildProcRoot's /proc resolves to, so the processor can open it.
func hostRootWithCgroup(t *testing.T) string {
	t.Helper()

	root := t.TempDir()

	err := os.MkdirAll(filepath.Join(root, "sys", "fs", "cgroup", "docker", "testcontainer"), 0o755)
	if err != nil {
		t.Fatal(err)
	}

	return root
}

func devNullProcessor(t *testing.T, fake *failingCgroup) *Processor {
	t.Helper()

	const pid = 70

	real2, err := cgroup.New(2, nil)
	if err != nil {
		t.Fatal(err)
	}

	fake.Interface = real2

	return &Processor{
		Inspector: &fakeInspector{result: container.InspectResponse{
			State: &container.State{Pid: pid},
			Mounts: []container.MountPoint{
				{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
			},
		}},
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  hostRootWithCgroup(t),
		ProcRoot:  buildProcRoot(t, pid),
		newCgroup: func(int, *cgroup.Ledger) (cgroup.Interface, error) { return fake, nil },
	}
}

// TestProcessContainer_SetsRulesThroughOpenedHandle checks that the
// processor opens the resolved cgroup directory itself and hands the cgroup
// API a handle carrying that directory's inode.
func TestProcessContainer_SetsRulesThroughOpenedHandle(t *testing.T) {
	fake := &failingCgroup{}
	proc := devNullProcessor(t, fake)

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatal(err)
	}

	wantPath := filepath.Join(proc.HostRoot, "sys", "fs", "cgroup", "docker", "testcontainer")

	info, err := os.Stat(wantPath)
	if err != nil {
		t.Fatal(err)
	}

	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		t.Fatal("no stat_t")
	}

	if fake.identity != (cgroup.Identity{Path: wantPath, Inode: stat.Ino}) {
		t.Errorf("handle identity = %+v, want %s inode %d", fake.identity, wantPath, stat.Ino)
	}
}

func TestProcessContainer_UnsupportedAttachModeSkipsContainer(t *testing.T) {
	buf := captureLogger(t)
	fake := &failingCgroup{
		err: fmt.Errorf("%w: attach mode none (exclusive)", cgroup.ErrUnsupportedAttachMode),
	}

	err := devNullProcessor(t, fake).ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("unsupported attach mode must skip, not fail: %v", err)
	}

	if fake.calls != 1 {
		t.Fatalf("SetDeviceRules calls = %d, want 1", fake.calls)
	}

	out := buf.String()
	if !strings.Contains(out, "level=ERROR") ||
		!strings.Contains(out, "attach mode none (exclusive)") {
		t.Errorf("expected ERROR naming the attach mode, got:\n%s", out)
	}
}

func TestProcessContainer_InaccessibleFiltersIsRetryableError(t *testing.T) {
	fake := &failingCgroup{err: fmt.Errorf("%w: 1 of 2 programs", cgroup.ErrFiltersInaccessible)}

	err := devNullProcessor(t, fake).ProcessContainer(context.Background(), "abc")
	if !errors.Is(err, cgroup.ErrFiltersInaccessible) {
		t.Fatalf("err = %v, want ErrFiltersInaccessible", err)
	}

	if !strings.Contains(err.Error(), "filters_inaccessible") {
		t.Errorf("error %q does not carry the reason", err)
	}
}

func TestProcessContainer_OwnedBlockConflictIsLoggedWithRemedy(t *testing.T) {
	buf := captureLogger(t)
	fake := &failingCgroup{err: fmt.Errorf("%w: trailer missing", cgroup.ErrOwnedBlockConflict)}

	err := devNullProcessor(t, fake).ProcessContainer(context.Background(), "abc")
	if !errors.Is(err, cgroup.ErrOwnedBlockConflict) ||
		!strings.Contains(err.Error(), "owned_block_conflict") {
		t.Fatalf("err = %v, want a retryable ErrOwnedBlockConflict carrying the reason", err)
	}

	out := buf.String()
	if !strings.Contains(out, "level=ERROR") || !strings.Contains(out, "restart the container") ||
		!strings.Contains(out, "cgroup=") {
		t.Errorf("expected ERROR with cgroup and remedy, got:\n%s", out)
	}
}

func TestProcessContainer_NotWrappableIsRetryableError(t *testing.T) {
	fake := &failingCgroup{err: fmt.Errorf("%w: uses maps", cgroup.ErrProgramNotWrappable)}

	err := devNullProcessor(t, fake).ProcessContainer(context.Background(), "abc")
	if !errors.Is(err, cgroup.ErrProgramNotWrappable) ||
		!strings.Contains(err.Error(), "program_not_wrappable") {
		t.Fatalf("err = %v, want ErrProgramNotWrappable with its reason", err)
	}
}
