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
	"github.com/cilium/ebpf/asm"
	"golang.org/x/sys/unix"
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

// SetDeviceRules adds rules to the device filter of the cgroup behind handle.
//
// It is still grant-only: rules are prepended to the attached programs, and
// earlier grants are not replaced or revoked. An empty rule set is a no-op.
//
// Every attached program is replaced by a copy with the rules prepended.
// Unlike NVIDIA's upstream code, which detaches every program before
// attaching the replacements because it runs strictly before the container
// starts, this daemon mutates live containers: detaching first would leave
// the container unfiltered in between, and permanently so if the attach
// failed or the daemon died. Programs are therefore replaced pairwise, each
// new program attached (or atomically swapped in) before its original goes.
func (c *cgroupv2) SetDeviceRules(handle *CgroupHandle, rules []DeviceRule) error {
	if len(rules) == 0 {
		return nil
	}

	return c.addDeviceRules(handle.fd, rules)
}

func (c *cgroupv2) addDeviceRules(dirFD int, rules []DeviceRule) error {
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
	case total == 0:
		return ErrFilterMissing
	case attachFlags&unix.BPF_F_ALLOW_MULTI == 0:
		return fmt.Errorf(
			"%w: attach mode %s",
			ErrUnsupportedAttachMode,
			describeAttachFlags(attachFlags),
		)
	}

	// Build every replacement before touching the cgroup, so a generation or
	// load failure mutates nothing.
	newProgs := make([]progHandle, 0, len(oldProgs))
	defer func() { closeAll(newProgs) }()

	for _, oldProg := range oldProgs {
		var (
			oldInsts asm.Instructions
			newProg  progHandle
		)

		oldInsts, err = c.ops.instructions(oldProg)
		if err != nil {
			return err
		}

		newProg, err = c.generateNewProgram(rules, oldInsts)
		if err != nil {
			return fmt.Errorf(
				"unable to generate new device filter program from existing programs: %w",
				err,
			)
		}

		newProgs = append(newProgs, newProg)
	}

	// Pairs are independent: under BPF_F_ALLOW_MULTI the verdict is the AND of
	// every program, so a half-replaced set is narrower, never wider (an
	// original not yet replaced masks the new grant). A failure therefore
	// stops here without undoing earlier pairs; a retry completes the rest.
	for idx := range oldProgs {
		err = c.swap(dirFD, oldProgs[idx], newProgs[idx])
		if err != nil {
			return fmt.Errorf(
				"replace device filter program %d of %d: %w",
				idx+1,
				len(oldProgs),
				err,
			)
		}
	}

	return nil
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

//nolint:ireturn // progHandle hides the kernel handle so the swap logic can be tested with fakes
func (c *cgroupv2) generateNewProgram(
	rules []DeviceRule,
	oldInsts asm.Instructions,
) (progHandle, error) {
	// Prepend instructions for the new devices to the original set of instructions.
	newInsts, err := PrependDeviceFilter(rules, oldInsts)
	if err != nil {
		return nil, fmt.Errorf(
			"unable to prepend new device filters to the original device filters program: %w",
			err,
		)
	}

	// Generate new eBPF program for the merged device filter instructions.
	spec := &ebpf.ProgramSpec{
		Type:         ebpf.CGroupDevice,
		Instructions: newInsts,
		License:      bpfProgramLicense,
	}

	return c.ops.load(spec)
}

func closeAll(progs []progHandle) {
	for _, prog := range progs {
		prog.Close()
	}
}
