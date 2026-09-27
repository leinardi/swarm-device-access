//go:build linux

/*
 * Copyright (c) 2021, NVIDIA CORPORATION.  All rights reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package cgroup

import (
	"bufio"
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"github.com/cilium/ebpf"
	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/logger"
)

const (
	bpfProgramLicense = "Apache"
)

var (
	errNoCgroup2Fs     = errors.New("no cgroup2 filesystem in mountinfo file")
	errNoCgroupV2Entry = errors.New("no cgroupv2 entries in file")
)

// GetDeviceCGroupMountPath returns the mount path (and its prefix) for the device cgroup controller associated with pid.
func (c *cgroupv2) GetDeviceCGroupMountPath(procRootPath string, pid int) (string, string, error) {
	path := filepath.Join(procRootPath, "proc", strconv.Itoa(pid), "mountinfo")

	file, err := os.Open(path)
	if err != nil {
		return "", "", fmt.Errorf("open %q: %w", path, err)
	}
	defer file.Close()

	return scanMountInfoV2(file, path)
}

// scanMountInfoV2 parses a mountinfo reader for the cgroup2 (unified) mount entry.
// Extracted from GetDeviceCGroupMountPath to allow unit testing without a real /proc.
func scanMountInfoV2(r io.Reader, path string) (string, string, error) {
	scanner := bufio.NewScanner(r)
	scanner.Split(bufio.ScanLines)

	// Loop through the file looking for a subsystem of '' (i.e. unified) entry.
	for scanner.Scan() {
		// Split each entry by '[space]'
		parts := strings.Split(scanner.Text(), " ")
		if len(parts) < 5 {
			return "", "", fmt.Errorf( //nolint:err113 // dynamic content, not wrappable
				"malformed mountinfo entry: %v",
				scanner.Text(),
			)
		}
		// Look for an entry with cgroup2 as the mount type.
		if parts[len(parts)-3] != "cgroup2" {
			continue
		}
		// Make sure the mount prefix is not a relative path.
		if strings.HasPrefix(parts[3], "/..") {
			return "", "", fmt.Errorf( //nolint:err113 // dynamic content, not wrappable
				"relative path in mount prefix: %v",
				parts[3],
			)
		}
		// Return the 3rd element as the prefix of the mount point for
		// the devices cgroup and the 4th element as the mount point of
		// the devices cgroup itself.
		return parts[3], parts[4], nil
	}

	scanErr := scanner.Err()
	if scanErr != nil {
		return "", "", fmt.Errorf("read %q: %w", path, scanErr)
	}

	return "", "", errNoCgroup2Fs
}

// GetDeviceCGroupRootPath returns the root path for the device cgroup controller associated with pid.
func (c *cgroupv2) GetDeviceCGroupRootPath(
	procRootPath string,
	prefix string,
	pid int,
) (string, error) {
	path := filepath.Join(procRootPath, "proc", strconv.Itoa(pid), "cgroup")

	file, err := os.Open(path)
	if err != nil {
		return "", fmt.Errorf("open %q: %w", path, err)
	}
	defer file.Close()

	return scanProcCgroupV2(file, path, prefix)
}

// scanProcCgroupV2 parses a /proc/<pid>/cgroup reader for the cgroup v2 unified root path.
// Extracted from GetDeviceCGroupRootPath to allow unit testing without a real /proc.
func scanProcCgroupV2(r io.Reader, path string, prefix string) (string, error) {
	scanner := bufio.NewScanner(r)
	scanner.Split(bufio.ScanLines)

	// Loop through the file looking for either a '' (i.e. unified) entry.
	for scanner.Scan() {
		// Split each entry by ':'
		parts := strings.SplitN(scanner.Text(), ":", 3)
		if len(parts) != 3 {
			return "", fmt.Errorf( //nolint:err113 // dynamic content, not wrappable
				"malformed cgroup entry: %v",
				scanner.Text(),
			)
		}
		// Look for the (empty) subsystem in the 1st element.
		if parts[1] != "" {
			continue
		}
		// Return the cgroup root from the 2nd element
		// (with the prefix possibly stripped off).
		if prefix == "/" {
			return parts[2], nil
		}

		return strings.TrimPrefix(parts[2], prefix), nil
	}

	scanErr := scanner.Err()
	if scanErr != nil {
		return "", fmt.Errorf("read %q: %w", path, scanErr)
	}

	return "", errNoCgroupV2Entry
}

// SetDeviceRules makes this daemon's grants in the device filters of the
// cgroup behind handle equal exactly rules.
//
// Each attached program is either one of this daemon's wrappers (see
// owned.go), which is stripped back to the runtime original it wraps, or an
// unmarked program, which is the original itself. Non-empty rules replace
// every program with a fresh wrapper around its original; empty rules
// replace every wrapper with its bare original and leave unmarked programs
// alone, so a cgroup this daemon never touched is never touched. Nothing
// but the programs themselves carries state, so this works the same after
// a daemon restart.
//
// Unmarked programs are never interpreted: a wrapper written by a version
// that predates the ownership markers looks exactly like a runtime program,
// so its grants are kept until the container restarts.
//
// Unlike NVIDIA's upstream code, which detaches every program before
// attaching the replacements because it runs strictly before the container
// starts, this daemon mutates live containers: detaching first would leave
// the container unfiltered in between, and permanently so if the attach
// failed or the daemon died. Programs are therefore replaced pairwise, each
// new program attached (or atomically swapped in) before its original goes.
func (c *cgroupv2) SetDeviceRules(handle *CgroupHandle, rules []DeviceRule) error {
	return c.setDeviceRules(handle.fd, handle.Identity().Path, rules)
}

// replacement is the planned new program for one attached program.
type replacement struct {
	old  progHandle
	raw  []byte
	name string
}

func (c *cgroupv2) setDeviceRules(dirFD int, cgroupPath string, rules []DeviceRule) error {
	// Find any existing eBPF device filter programs attached to this cgroup.
	oldProgs, total, inaccessible, attachFlags, err := c.ops.query(dirFD)
	if err != nil {
		return fmt.Errorf(
			"unable to find any existing device filters attached to the cgroup: %w",
			err,
		)
	}
	defer closeAll(oldProgs)

	switch {
	case inaccessible > 0:
		return fmt.Errorf("%w: %d of %d programs", ErrFiltersInaccessible, inaccessible, total)
	case total == 0 && len(rules) == 0:
		return nil
	case total == 0:
		return ErrFilterMissing
	case attachFlags&unix.BPF_F_ALLOW_MULTI == 0:
		return fmt.Errorf(
			"%w: attach mode %s",
			ErrUnsupportedAttachMode,
			describeAttachFlags(attachFlags),
		)
	}

	// Plan every replacement before touching the cgroup: a conflict, an
	// unwrappable program or a load failure anywhere mutates nothing.
	plans := make([]replacement, 0, len(oldProgs))

	for _, oldProg := range oldProgs {
		var (
			plan replacement
			keep bool
		)

		plan, keep, err = c.planReplacement(oldProg, cgroupPath, rules)
		if err != nil {
			return err
		}

		if !keep {
			plans = append(plans, plan)
		}
	}

	newProgs := make([]progHandle, 0, len(plans))
	defer func() { closeAll(newProgs) }()

	for _, plan := range plans {
		var newProg progHandle

		newProg, err = c.loadCanonical(plan.raw, plan.name)
		if err != nil {
			return err
		}

		newProgs = append(newProgs, newProg)
	}

	// Pairs are independent: under BPF_F_ALLOW_MULTI the verdict is the AND of
	// every program, so a half-replaced set is narrower, never wider (an
	// original not yet replaced masks the new grant). A failure therefore
	// stops here without undoing earlier pairs; a retry completes the rest.
	for idx, plan := range plans {
		err = c.swap(dirFD, plan.old, newProgs[idx])
		if err != nil {
			return fmt.Errorf(
				"replace device filter program %d of %d: %w",
				idx+1,
				len(plans),
				err,
			)
		}
	}

	return nil
}

// planReplacement decides what replaces oldProg. keep reports that oldProg
// stays as it is (an unmarked program under empty rules).
func (c *cgroupv2) planReplacement(
	oldProg progHandle,
	cgroupPath string,
	rules []DeviceRule,
) (replacement, bool, error) {
	insts, meta, err := c.ops.instructions(oldProg)
	if err != nil {
		return replacement{}, false, err
	}

	raw, err := canonicalBytes(insts)
	if err != nil {
		return replacement{}, false, err
	}

	owned, isOwned, err := parseOwned(raw)
	if err != nil {
		return replacement{}, false, err
	}

	if !isOwned && len(rules) == 0 {
		return replacement{}, true, nil
	}

	original := raw
	if isOwned {
		original = owned.original
	}

	origInsts, err := fromCanonical(original)
	if err != nil {
		return replacement{}, false, err
	}

	err = checkWrappable(origInsts, meta)
	if err != nil {
		return replacement{}, false, err
	}

	if len(rules) == 0 {
		return replacement{old: oldProg, raw: original}, false, nil
	}

	var nonce uint64

	if isOwned {
		nonce = owned.nonce
	} else {
		nonce, err = newNonce()
		if err != nil {
			return replacement{}, false, err
		}

		logger.L().
			Info("wrapping device filter with no prior owned block; pre-upgrade grants, if any, are not managed",
				"cgroup", cgroupPath)
	}

	wrapped, err := emitOwned(rules, nonce, original)
	if err != nil {
		return replacement{}, false, fmt.Errorf(
			"unable to generate new device filter program: %w",
			err,
		)
	}

	return replacement{old: oldProg, raw: wrapped, name: ownedProgramName}, false, nil
}

// loadCanonical loads a program from canonical bytes. A verifier rejection
// is returned as is, before anything is attached.
//
//nolint:ireturn // progHandle hides the kernel handle so the swap logic can be tested with fakes
func (c *cgroupv2) loadCanonical(raw []byte, name string) (progHandle, error) {
	insts, err := fromCanonical(raw)
	if err != nil {
		return nil, err
	}

	spec := &ebpf.ProgramSpec{
		Name:         name,
		Type:         ebpf.CGroupDevice,
		Instructions: insts,
		License:      bpfProgramLicense,
	}

	return c.ops.load(spec)
}

// swap replaces oldProg with newProg on dirFD: atomically when the kernel
// supports BPF_F_REPLACE, otherwise by attaching newProg before detaching
// oldProg. If that detach fails, newProg is detached again so the pair is
// left as it was (both attached would make a later pass stack another copy).
func (c *cgroupv2) swap(dirFD int, oldProg, newProg progHandle) error {
	if c.replace.usable() {
		err := c.ops.attach(newProg, dirFD, unix.BPF_F_ALLOW_MULTI, oldProg)
		c.replace.record(err)

		if err == nil {
			return nil
		}

		if c.replace.usable() {
			return fmt.Errorf("unable to replace device filters program: %w", err)
		}
	}

	err := c.ops.attach(newProg, dirFD, unix.BPF_F_ALLOW_MULTI, nil)
	if err != nil {
		return fmt.Errorf("unable to attach new device filters program: %w", err)
	}

	detachErr := c.ops.detach(oldProg, dirFD)
	if detachErr == nil {
		return nil
	}

	detachErr = fmt.Errorf("unable to detach original device filters program: %w", detachErr)

	rollbackErr := c.ops.detach(newProg, dirFD)
	if rollbackErr != nil {
		return errors.Join(
			detachErr,
			fmt.Errorf("roll back new device filters program: %w", rollbackErr),
		)
	}

	return detachErr
}

func closeAll(progs []progHandle) {
	for _, prog := range progs {
		prog.Close()
	}
}
