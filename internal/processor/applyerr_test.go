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
	"syscall"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// failingCgroup is a cgroup.Interface that records every SetDeviceRules
// call and returns err. The processor resolves cgroup paths itself, so the
// embedded Interface is never called.
type failingCgroup struct {
	cgroup.Interface

	err        error
	calls      int
	identity   cgroup.Identity
	identities []cgroup.Identity
	rules      [][]cgroup.DeviceRule
	// onSet, when set, runs inside every call, after it is recorded.
	onSet func()
}

func (f *failingCgroup) SetDeviceRules(
	handle *cgroup.CgroupHandle,
	rules []cgroup.DeviceRule,
) error {
	f.calls++
	f.identity = handle.Identity()
	f.identities = append(f.identities, f.identity)
	f.rules = append(f.rules, rules)

	if f.onSet != nil {
		f.onSet()
	}

	return f.err
}

// fakePinner pins nothing: every pinned process is alive unless gone is set.
type fakePinner struct {
	pins []int
	gone bool
}

//nolint:ireturn // implements processPinner
func (f *fakePinner) pin(pid int) (pinnedProcess, error) {
	f.pins = append(f.pins, pid)

	return fakePinned{gone: f.gone}, nil
}

type fakePinned struct{ gone bool }

func (fakePinned) Close() error { return nil }

func (f fakePinned) alive() error {
	if f.gone {
		return errPinnedProcessGone
	}

	return nil
}

// testCgroupDir is where buildProcRoot's /proc places the container, below
// a host root.
func testCgroupDir(hostRoot string) string {
	return filepath.Join(hostRoot, "sys", "fs", "cgroup", "docker", "testcontainer")
}

// hostRootWithCgroup returns a host root holding the cgroup directory that
// buildProcRoot's /proc resolves to, with pid listed in its cgroup.procs.
func hostRootWithCgroup(t *testing.T, pid int) string {
	t.Helper()

	root := t.TempDir()

	err := os.MkdirAll(testCgroupDir(root), 0o755)
	if err != nil {
		t.Fatal(err)
	}

	writeProcs(t, root, pid)

	return root
}

// writeProcs replaces the test cgroup's cgroup.procs with pids.
func writeProcs(t *testing.T, hostRoot string, pids ...int) {
	t.Helper()

	var content strings.Builder
	for _, pid := range pids {
		content.WriteString(strconv.Itoa(pid) + "\n")
	}

	err := os.WriteFile(
		filepath.Join(testCgroupDir(hostRoot), "cgroup.procs"),
		[]byte(content.String()),
		0o600,
	)
	if err != nil {
		t.Fatal(err)
	}
}

func devNullProcessor(t *testing.T, fake *failingCgroup) *Processor {
	t.Helper()

	const pid = 70

	return &Processor{
		Inspector: &fakeInspector{result: container.InspectResponse{
			State: &container.State{Running: true, Pid: pid},
			Mounts: []container.MountPoint{
				{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
			},
		}},
		Cfg:      newStore(policy.ModeAll, false),
		HostRoot: hostRootWithCgroup(t, pid),
		ProcRoot: buildProcRoot(t, pid),
		pinner:   &fakePinner{},
		newCgroup: func(int, *cgroup.Ledger, *cgroup.FilterCache) (cgroup.Interface, error) {
			return fake, nil
		},
	}
}

// TestReconcile_SetsRulesThroughOpenedHandle checks that the
// processor opens the resolved cgroup directory itself and hands the cgroup
// API a handle carrying that directory's inode.
func TestReconcile_SetsRulesThroughOpenedHandle(t *testing.T) {
	fake := &failingCgroup{}
	proc := devNullProcessor(t, fake)

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatal(err)
	}

	wantPath := testCgroupDir(proc.HostRoot)

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

func TestReconcile_UnsupportedAttachModeSkipsContainer(t *testing.T) {
	buf := captureLogger(t)
	fake := &failingCgroup{
		err: fmt.Errorf("%w: attach mode none (exclusive)", cgroup.ErrUnsupportedAttachMode),
	}

	err := devNullProcessor(t, fake).Reconcile(context.Background(), "abc")
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

func TestReconcile_InaccessibleFiltersIsRetryableError(t *testing.T) {
	fake := &failingCgroup{err: fmt.Errorf("%w: 1 of 2 programs", cgroup.ErrFiltersInaccessible)}

	err := devNullProcessor(t, fake).Reconcile(context.Background(), "abc")
	if !errors.Is(err, cgroup.ErrFiltersInaccessible) {
		t.Fatalf("err = %v, want ErrFiltersInaccessible", err)
	}

	if !strings.Contains(err.Error(), "filters_inaccessible") {
		t.Errorf("error %q does not carry the reason", err)
	}
}

func TestReconcile_OwnedBlockConflictIsLoggedWithRemedy(t *testing.T) {
	buf := captureLogger(t)
	fake := &failingCgroup{err: fmt.Errorf("%w: trailer missing", cgroup.ErrOwnedBlockConflict)}

	err := devNullProcessor(t, fake).Reconcile(context.Background(), "abc")
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

func TestReconcile_NotWrappableIsRetryableError(t *testing.T) {
	fake := &failingCgroup{err: fmt.Errorf("%w: uses maps", cgroup.ErrProgramNotWrappable)}

	err := devNullProcessor(t, fake).Reconcile(context.Background(), "abc")
	if !errors.Is(err, cgroup.ErrProgramNotWrappable) ||
		!strings.Contains(err.Error(), "program_not_wrappable") {
		t.Fatalf("err = %v, want ErrProgramNotWrappable with its reason", err)
	}
}

func TestReconcile_FilterMissing(t *testing.T) {
	for _, privileged := range []bool{true, false} {
		buf := captureLogger(t)
		fake := &failingCgroup{err: cgroup.ErrFilterMissing}
		proc := devNullProcessor(t, fake)

		insp, ok := proc.Inspector.(*fakeInspector)
		if !ok {
			t.Fatal("unexpected inspector")
		}

		insp.result.HostConfig = &container.HostConfig{Privileged: privileged}

		err := proc.Reconcile(context.Background(), "abc")

		switch {
		case privileged && err != nil:
			t.Errorf("privileged: err = %v, want a no-op", err)
		case privileged && !strings.Contains(buf.String(), "privileged container has no device filter"):
			t.Errorf("privileged: expected INFO, got:\n%s", buf.String())
		case !privileged && (!errors.Is(err, cgroup.ErrFilterMissing) || !strings.Contains(err.Error(), "filter_missing")):
			t.Errorf(
				"unprivileged: err = %v, want a retryable ErrFilterMissing with its reason",
				err,
			)
		case !privileged && !strings.Contains(buf.String(), "restart the container"):
			t.Errorf("unprivileged: expected WARN with remedy, got:\n%s", buf.String())
		}
	}
}
