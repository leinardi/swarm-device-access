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

package cgroup

import (
	"errors"
	"fmt"
	"slices"
	"testing"

	"github.com/cilium/ebpf"
	"github.com/cilium/ebpf/asm"
	"golang.org/x/sys/unix"
)

var (
	errInjected  = errors.New("injected failure")
	errRollback  = errors.New("injected rollback failure")
	errLoadFails = errors.New("injected load failure")
	errBadFlags  = errors.New("unexpected attach flags")
)

// fakeProg is a progHandle naming the program it refers to. Several handles
// may refer to the same program (every query opens fresh ones).
type fakeProg struct {
	name   string
	closed bool
}

func (p *fakeProg) Close() error {
	p.closed = true

	return nil
}

// fakeOps models one cgroup's BPF_CGROUP_DEVICE attachment list. errs maps a
// call description ("attach N1", "replace O1->N1", "detach O1", "load N2") to
// the error that call returns; a failing call changes nothing.
type fakeOps struct {
	attached     []string
	flags        uint32
	inaccessible int
	errs         map[string]error

	calls  []string
	opened []*fakeProg
	loads  int
}

func newFakeOps(attached ...string) *fakeOps {
	return &fakeOps{attached: attached, flags: unix.BPF_F_ALLOW_MULTI, errs: map[string]error{}}
}

func (f *fakeOps) open(name string) *fakeProg {
	prog := &fakeProg{name: name}
	f.opened = append(f.opened, prog)

	return prog
}

func (f *fakeOps) query(int) ([]progHandle, int, int, uint32, error) {
	handles := make([]progHandle, 0, len(f.attached))
	for _, name := range f.attached {
		handles = append(handles, f.open(name))
	}

	return handles, len(f.attached) + f.inaccessible, f.inaccessible, f.flags, nil
}

func nameOf(handle progHandle) string {
	prog, ok := handle.(*fakeProg)
	if !ok {
		return fmt.Sprintf("foreign %T", handle)
	}

	return prog.name
}

//nolint:ireturn // implements v2ops.load
func (f *fakeOps) load(*ebpf.ProgramSpec) (progHandle, error) {
	f.loads++
	name := fmt.Sprintf("N%d", f.loads)

	call := "load " + name
	f.calls = append(f.calls, call)

	err := f.errs[call]
	if err != nil {
		return nil, err
	}

	return f.open(name), nil
}

func (f *fakeOps) attach(prog progHandle, _ int, flags uint32, replace progHandle) error {
	if flags != unix.BPF_F_ALLOW_MULTI {
		return fmt.Errorf("%w: attach flags %#x, want BPF_F_ALLOW_MULTI only", errBadFlags, flags)
	}

	name := nameOf(prog)

	call := "attach " + name
	if replace != nil {
		call = "replace " + nameOf(replace) + "->" + name
	}

	f.calls = append(f.calls, call)

	err := f.errs[call]
	if err != nil {
		return err
	}

	if replace == nil {
		f.attached = append(f.attached, name)

		return nil
	}

	idx := slices.Index(f.attached, nameOf(replace))
	if idx < 0 {
		return unix.ENOENT
	}

	f.attached[idx] = name

	return nil
}

func (f *fakeOps) detach(prog progHandle, _ int) error {
	name := nameOf(prog)

	call := "detach " + name
	f.calls = append(f.calls, call)

	err := f.errs[call]
	if err != nil {
		return err
	}

	f.attached = slices.DeleteFunc(f.attached, func(n string) bool { return n == name })

	return nil
}

func (f *fakeOps) instructions(progHandle) (asm.Instructions, error) {
	return asm.Instructions{asm.Mov.Imm32(asm.R0, 0), asm.Return()}, nil
}

func (f *fakeOps) assertAllClosed(t *testing.T) {
	t.Helper()

	for _, prog := range f.opened {
		if !prog.closed {
			t.Errorf("handle %s left open", prog.name)
		}
	}
}

func testRules() []DeviceRule {
	major, minor := int64(10), int64(200)

	return []DeviceRule{{Allow: true, Type: "c", Major: &major, Minor: &minor, Access: "rwm"}}
}

func probeIn(state int32) *replaceProbe {
	probe := &replaceProbe{}
	probe.state.Store(state)

	return probe
}

func run(ops *fakeOps, probe *replaceProbe) error {
	c := &cgroupv2{ops: ops, replace: probe}

	return c.addDeviceRules(3, testRules())
}

func assertCalls(t *testing.T, ops *fakeOps, want ...string) {
	t.Helper()

	if !slices.Equal(ops.calls, want) {
		t.Errorf("calls = %q, want %q", ops.calls, want)
	}
}

func assertAttached(t *testing.T, ops *fakeOps, want ...string) {
	t.Helper()

	if !slices.Equal(ops.attached, want) {
		t.Errorf("attached = %q, want %q", ops.attached, want)
	}
}

func TestSwap_UsesAtomicReplaceWhenSupported(t *testing.T) {
	for _, state := range []int32{replaceUnknown, replaceSupported} {
		ops := newFakeOps("O1", "O2")
		probe := probeIn(state)

		err := run(ops, probe)
		if err != nil {
			t.Fatalf("state %d: %v", state, err)
		}

		assertCalls(t, ops, "load N1", "load N2", "replace O1->N1", "replace O2->N2")
		assertAttached(t, ops, "N1", "N2")
		ops.assertAllClosed(t)

		if probe.state.Load() != replaceSupported {
			t.Errorf("probe state = %d, want supported", probe.state.Load())
		}
	}
}

func TestSwap_AttachBeforeDetachWhenReplaceUnsupported(t *testing.T) {
	ops := newFakeOps("O1", "O2")

	err := run(ops, probeIn(replaceUnsupported))
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops, "load N1", "load N2", "attach N1", "detach O1", "attach N2", "detach O2")
	assertAttached(t, ops, "N1", "N2")
	ops.assertAllClosed(t)
}

// TestSwap_EINVALOnFirstReplaceIsCachedAsUnsupported covers the probe: an
// EINVAL on the first replace switches to attach-then-detach for this and
// every later pair, without retrying the replace.
func TestSwap_EINVALOnFirstReplaceIsCachedAsUnsupported(t *testing.T) {
	ops := newFakeOps("O1", "O2")
	ops.errs["replace O1->N1"] = unix.EINVAL
	probe := probeIn(replaceUnknown)

	err := run(ops, probe)
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops,
		"load N1", "load N2", "replace O1->N1", "attach N1", "detach O1", "attach N2", "detach O2")
	assertAttached(t, ops, "N1", "N2")
	ops.assertAllClosed(t)

	if probe.state.Load() != replaceUnsupported {
		t.Errorf("probe state = %d, want unsupported", probe.state.Load())
	}
}

// TestSwap_ReplaceFailureOnSupportingKernelIsReturned checks that a replace
// failure other than the probe's EINVAL is an error, not a fallback.
func TestSwap_ReplaceFailureOnSupportingKernelIsReturned(t *testing.T) {
	ops := newFakeOps("O1")
	ops.errs["replace O1->N1"] = errInjected

	err := run(ops, probeIn(replaceSupported))
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v, want injected", err)
	}

	assertCalls(t, ops, "load N1", "replace O1->N1")
	assertAttached(t, ops, "O1")
	ops.assertAllClosed(t)
}

func TestSwap_FirstAttachFailureDetachesNothing(t *testing.T) {
	ops := newFakeOps("O1", "O2")
	ops.errs["attach N1"] = errInjected

	err := run(ops, probeIn(replaceUnsupported))
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v, want injected", err)
	}

	assertCalls(t, ops, "load N1", "load N2", "attach N1")
	assertAttached(t, ops, "O1", "O2")
	ops.assertAllClosed(t)
}

func TestSwap_LaterAttachFailureLeavesEarlierPairReplaced(t *testing.T) {
	ops := newFakeOps("O1", "O2")
	ops.errs["attach N2"] = errInjected

	err := run(ops, probeIn(replaceUnsupported))
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v, want injected", err)
	}

	assertCalls(t, ops, "load N1", "load N2", "attach N1", "detach O1", "attach N2")
	assertAttached(t, ops, "O2", "N1")
	ops.assertAllClosed(t)
}

func TestSwap_DetachFailureRollsBackThatPair(t *testing.T) {
	ops := newFakeOps("O1")
	ops.errs["detach O1"] = errInjected

	err := run(ops, probeIn(replaceUnsupported))
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v, want injected", err)
	}

	assertCalls(t, ops, "load N1", "attach N1", "detach O1", "detach N1")
	assertAttached(t, ops, "O1")
	ops.assertAllClosed(t)
}

func TestSwap_RollbackFailureIsJoined(t *testing.T) {
	ops := newFakeOps("O1")
	ops.errs["detach O1"] = errInjected
	ops.errs["detach N1"] = errRollback

	err := run(ops, probeIn(replaceUnsupported))
	if !errors.Is(err, errInjected) || !errors.Is(err, errRollback) {
		t.Fatalf("err = %v, want both the detach and the rollback error", err)
	}

	assertCalls(t, ops, "load N1", "attach N1", "detach O1", "detach N1")
	assertAttached(t, ops, "O1", "N1")
	ops.assertAllClosed(t)
}

func TestSwap_SecondDetachFailureLeavesPairOneReplacedPairTwoRolledBack(t *testing.T) {
	ops := newFakeOps("O1", "O2")
	ops.errs["detach O2"] = errInjected

	err := run(ops, probeIn(replaceUnsupported))
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v, want injected", err)
	}

	assertCalls(t, ops,
		"load N1", "load N2", "attach N1", "detach O1", "attach N2", "detach O2", "detach N2")
	assertAttached(t, ops, "O2", "N1")
	ops.assertAllClosed(t)
}

func TestSwap_NonMultiAttachModeIsRefused(t *testing.T) {
	for _, flags := range []uint32{0, unix.BPF_F_ALLOW_OVERRIDE} {
		ops := newFakeOps("O1")
		ops.flags = flags

		err := run(ops, probeIn(replaceUnknown))
		if !errors.Is(err, ErrUnsupportedAttachMode) {
			t.Fatalf("flags %#x: err = %v, want ErrUnsupportedAttachMode", flags, err)
		}

		assertCalls(t, ops)
		assertAttached(t, ops, "O1")
		ops.assertAllClosed(t)
	}
}

func TestSwap_AnyInaccessibleProgramRefusesEverything(t *testing.T) {
	ops := newFakeOps("O1")
	ops.inaccessible = 1

	err := run(ops, probeIn(replaceUnknown))
	if !errors.Is(err, ErrFiltersInaccessible) {
		t.Fatalf("err = %v, want ErrFiltersInaccessible", err)
	}

	assertCalls(t, ops)
	assertAttached(t, ops, "O1")
	ops.assertAllClosed(t)
}

// TestSwap_OnlyInaccessibleProgramsIsNotMissing checks that "no handles"
// is not read as "nothing attached" when the kernel counted programs.
func TestSwap_OnlyInaccessibleProgramsIsNotMissing(t *testing.T) {
	ops := newFakeOps()
	ops.inaccessible = 2

	err := run(ops, probeIn(replaceUnknown))
	if !errors.Is(err, ErrFiltersInaccessible) || errors.Is(err, ErrFilterMissing) {
		t.Fatalf("err = %v, want ErrFiltersInaccessible only", err)
	}

	assertCalls(t, ops)
}

func TestSwap_NoProgramAttachedFailsClosed(t *testing.T) {
	ops := newFakeOps()

	err := run(ops, probeIn(replaceUnknown))
	if !errors.Is(err, ErrFilterMissing) {
		t.Fatalf("err = %v, want ErrFilterMissing", err)
	}

	assertCalls(t, ops)
	assertAttached(t, ops)
}

func TestSwap_LoadFailureMutatesNothing(t *testing.T) {
	ops := newFakeOps("O1", "O2")
	ops.errs["load N2"] = errLoadFails

	err := run(ops, probeIn(replaceUnknown))
	if !errors.Is(err, errLoadFails) {
		t.Fatalf("err = %v, want load failure", err)
	}

	assertCalls(t, ops, "load N1", "load N2")
	assertAttached(t, ops, "O1", "O2")
	ops.assertAllClosed(t)
}

func TestPrependDeviceFilter_RefusesMissingOriginal(t *testing.T) {
	_, err := PrependDeviceFilter(testRules(), nil)
	if !errors.Is(err, errNoOriginalProgram) {
		t.Fatalf("err = %v, want errNoOriginalProgram", err)
	}
}
