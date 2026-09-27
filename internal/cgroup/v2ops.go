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
	"strings"
	"sync/atomic"

	"github.com/cilium/ebpf"
	"github.com/cilium/ebpf/asm"
	"github.com/cilium/ebpf/link"
	"golang.org/x/sys/unix"
)

var (
	// ErrFiltersInaccessible reports that at least one device filter attached
	// to the cgroup could not be opened (typically hidden by an LSM policy).
	// Nothing is attached or detached: such a program cannot be inspected, so
	// neither its ownership nor the outcome of a replacement can be known.
	// The condition can clear (the program may be detached), so it is
	// retryable.
	ErrFiltersInaccessible = errors.New("device filters attached to the cgroup cannot be accessed")

	// ErrUnsupportedAttachMode reports that the cgroup's device filter is
	// attached without BPF_F_ALLOW_MULTI. Adding a program would then fail
	// with EBUSY, and replacing it would require detaching the runtime's
	// filter first, leaving the container unfiltered; the container is skipped.
	ErrUnsupportedAttachMode = errors.New("device filter attached without BPF_F_ALLOW_MULTI")

	// ErrFilterMissing reports that no device filter is attached to the
	// cgroup, so there is no runtime program to add grants to.
	ErrFilterMissing = errors.New("no device filter attached to the cgroup")
)

// progHandle is an open BPF program. The real implementation wraps an
// *ebpf.Program; tests use fakes, since a real handle needs the kernel.
type progHandle interface {
	Close() error
}

// v2ops is the kernel surface cgroupv2 uses. It exists so the ordering of
// attach, detach and close can be tested without privileges.
type v2ops interface {
	// query returns the programs attached to dirFD for BPF_CGROUP_DEVICE: the
	// handles that could be opened, the kernel's total count, the number that
	// could not be opened and the attach flags.
	query(dirFD int) (progs []progHandle, total, inaccessible int, attachFlags uint32, err error)
	load(spec *ebpf.ProgramSpec) (progHandle, error)
	// attach attaches prog with flags; a non-nil replace makes it an atomic
	// replacement of that program.
	attach(prog progHandle, dirFD int, flags uint32, replace progHandle) error
	detach(prog progHandle, dirFD int) error
	// instructions returns the program's translated instructions and what
	// the reloadability gate needs to know about its maps.
	instructions(prog progHandle) (asm.Instructions, progMeta, error)
}

// kernelProg adapts *ebpf.Program to progHandle.
type kernelProg struct {
	*ebpf.Program
}

var errForeignHandle = errors.New("program handle was not opened by the kernel ops")

// kernelProgram unwraps a handle produced by kernelOps.
func kernelProgram(handle progHandle) (*ebpf.Program, error) {
	prog, ok := handle.(kernelProg)
	if !ok {
		return nil, fmt.Errorf("%w: %T", errForeignHandle, handle)
	}

	return prog.Program, nil
}

// kernelOps is the production v2ops.
type kernelOps struct{}

var _ v2ops = kernelOps{}

func (kernelOps) query(dirFD int) ([]progHandle, int, int, uint32, error) {
	progs, total, inaccessible, flags, err := FindAttachedCgroupDeviceFilters(dirFD)
	if err != nil {
		return nil, 0, 0, 0, err
	}

	handles := make([]progHandle, 0, len(progs))
	for _, prog := range progs {
		handles = append(handles, kernelProg{prog})
	}

	return handles, total, inaccessible, flags, nil
}

//nolint:ireturn // progHandle hides the kernel handle so the swap logic can be tested with fakes
func (kernelOps) load(spec *ebpf.ProgramSpec) (progHandle, error) {
	prog, err := ebpf.NewProgram(spec)
	if err != nil {
		return nil, fmt.Errorf("unable to create new device filters program: %w", err)
	}

	return kernelProg{prog}, nil
}

func (kernelOps) attach(prog progHandle, dirFD int, flags uint32, replace progHandle) error {
	newProg, err := kernelProgram(prog)
	if err != nil {
		return err
	}

	opts := link.RawAttachProgramOptions{
		Target:  dirFD,
		Program: newProg,
		Attach:  ebpf.AttachCGroupDevice,
		// Never BPF_F_REPLACE here: cilium rejects anchor flags in Flags and
		// sets it itself from the ReplaceProgram anchor.
		Flags: flags,
	}

	if replace != nil {
		var oldProg *ebpf.Program

		oldProg, err = kernelProgram(replace)
		if err != nil {
			return err
		}

		opts.Anchor = link.ReplaceProgram(oldProg)
	}

	err = link.RawAttachProgram(opts)
	if err != nil {
		return fmt.Errorf(
			"failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, flags %s): %w",
			describeAttachFlags(flags),
			err,
		)
	}

	return nil
}

func (kernelOps) detach(prog progHandle, dirFD int) error {
	kprog, err := kernelProgram(prog)
	if err != nil {
		return err
	}

	return DetachCgroupDeviceFilter(kprog, dirFD)
}

func (kernelOps) instructions(prog progHandle) (asm.Instructions, progMeta, error) {
	kprog, err := kernelProgram(prog)
	if err != nil {
		return nil, progMeta{}, err
	}

	if !kernelDumpsSanitized() {
		return nil, progMeta{}, fmt.Errorf("%w: kernel older than 4.16", ErrProgramNotWrappable)
	}

	info, err := kprog.Info()
	if err != nil {
		return nil, progMeta{}, fmt.Errorf(
			"unable to get Info() of the original device filters program: %w",
			err,
		)
	}

	insts, err := info.Instructions()
	if err != nil {
		return nil, progMeta{}, fmt.Errorf(
			"unable to get the instructions of the original device filters program: %w",
			err,
		)
	}

	mapIDs, known := info.MapIDs()

	return insts, progMeta{mapIDsKnown: known, mapCount: len(mapIDs)}, nil
}

// kernelDumpsSanitized reports whether the running kernel is 4.16 or newer,
// the first release whose translated dumps of unprivileged-style programs
// are sanitized consistently enough to be loaded back.
func kernelDumpsSanitized() bool {
	var uname unix.Utsname

	err := unix.Uname(&uname)
	if err != nil {
		return false
	}

	return kernelAtLeast(unix.ByteSliceToString(uname.Release[:]), 4, 16)
}

// kernelAtLeast parses the leading "major.minor" of a kernel release string.
func kernelAtLeast(release string, wantMajor, wantMinor int) bool {
	var major, minor int

	_, err := fmt.Sscanf(release, "%d.%d", &major, &minor)
	if err != nil {
		return false
	}

	return major > wantMajor || (major == wantMajor && minor >= wantMinor)
}

// Replace-support states for replaceProbe.
const (
	replaceUnknown int32 = iota
	replaceSupported
	replaceUnsupported
)

// replaceProbe caches whether the kernel supports atomic BPF_F_REPLACE for
// cgroup programs (Linux 5.5+). It is probed on first use rather than
// hard-coded from a kernel version: the first replace attempt either succeeds
// or fails with EINVAL (unknown flag on older kernels), and the answer is
// kept for the life of the process. Degrading to attach-then-detach on a
// kernel that could have replaced is safe, only less tidy, so a stray EINVAL
// cannot make anything wider.
type replaceProbe struct {
	state atomic.Int32
}

// processReplaceProbe is shared by every cgroupv2 instance: cgroup.New
// builds a fresh instance per call, and the kernel's answer does not change.
var processReplaceProbe = &replaceProbe{}

func (r *replaceProbe) usable() bool {
	return r.state.Load() != replaceUnsupported
}

func (r *replaceProbe) record(err error) {
	switch {
	case err == nil:
		r.state.CompareAndSwap(replaceUnknown, replaceSupported)
	case errors.Is(err, unix.EINVAL):
		r.state.CompareAndSwap(replaceUnknown, replaceUnsupported)
	}
}

// describeAttachFlags names cgroup attach flags for logs and errors.
func describeAttachFlags(flags uint32) string {
	var names []string

	if flags&unix.BPF_F_ALLOW_OVERRIDE != 0 {
		names = append(names, "BPF_F_ALLOW_OVERRIDE")
	}

	if flags&unix.BPF_F_ALLOW_MULTI != 0 {
		names = append(names, "BPF_F_ALLOW_MULTI")
	}

	if len(names) == 0 {
		return "none (exclusive)"
	}

	return strings.Join(names, "|")
}
