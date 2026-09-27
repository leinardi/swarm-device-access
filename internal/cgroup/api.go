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
)

// DeviceRule mirrors the shape of specs.LinuxDeviceCgroup from
// github.com/opencontainers/runtime-spec/specs-go, inlined here to avoid
// pulling in the runtime-spec module for a single type.
type DeviceRule struct {
	Allow  bool   `json:"allow"`
	Type   string `json:"type,omitempty"`
	Major  *int64 `json:"major,omitempty"`
	Minor  *int64 `json:"minor,omitempty"`
	Access string `json:"access,omitempty"`
}

type Interface interface {
	GetDeviceCGroupMountPath(procRootPath string, pid int) (string, string, error)
	GetDeviceCGroupRootPath(procRootPath string, prefix string, pid int) (string, error)
	// SetDeviceRules makes the daemon-owned device grants of the cgroup
	// behind handle equal exactly devices. Every read and write goes through
	// the handle's descriptor, never the path.
	SetDeviceRules(handle *CgroupHandle, devices []DeviceRule) error
}

// New returns the cgroup API for version. ledger records the cgroup v1
// grants this daemon made and cache the cgroup v2 runtime originals it has
// seen; both must outlive the call (one per Processor). ledger is required
// for version 1; a nil cache disables rebuilding wiped cgroup v2 filters.
//
//nolint:ireturn // intentional: callers use the interface
func New(version int, ledger *Ledger, cache *FilterCache) (Interface, error) {
	switch version {
	case 1:
		if ledger == nil {
			return nil, errMissingLedger
		}

		return &cgroupv1{ledger: ledger, files: openatFiles{}}, nil
	case 2:
		return &cgroupv2{ops: kernelOps{}, replace: processReplaceProbe, cache: cache}, nil
	default:
		return nil, fmt.Errorf( //nolint:err113 // dynamic content
			"invalid cgroup version %d",
			version,
		)
	}
}

var errNoDeviceOrUnifiedCgroup = errors.New("no devices or unified cgroup entries found")

type (
	cgroupv1 struct {
		ledger *Ledger
		files  v1files
	}
	cgroupv2 struct {
		ops     v2ops
		replace *replaceProbe
		cache   *FilterCache
	}
)

var (
	_ Interface = (*cgroupv1)(nil)
	_ Interface = (*cgroupv2)(nil)
)

// GetDeviceCGroupVersion returns the version of linux cgroups in use.
func GetDeviceCGroupVersion(rootPath string, pid int) (int, error) {
	path := filepath.Join(rootPath, "proc", strconv.Itoa(pid), "cgroup")

	file, err := os.Open(path)
	if err != nil {
		return -1, fmt.Errorf("failed to open cgroup path for pid '%d': %w", pid, err)
	}
	defer file.Close()

	return scanCGroupVersion(file, path)
}

// scanCGroupVersion parses the cgroup hierarchy file and returns 1 (v1) or 2 (v2).
// Extracted from GetDeviceCGroupVersion to allow unit testing without a real /proc.
func scanCGroupVersion(r io.Reader, path string) (int, error) {
	scanner := bufio.NewScanner(r)
	scanner.Split(bufio.ScanLines)

	// Loop through the file looking for either a 'devices' or a '' (i.e. unified) entry
	found := make(map[string]bool)

	for scanner.Scan() {
		parts := strings.SplitN(scanner.Text(), ":", 3)
		if len(parts) != 3 {
			return -1, fmt.Errorf( //nolint:err113 // dynamic content, not wrappable
				"malformed cgroup entry: %v",
				scanner.Text(),
			)
		}

		found[parts[1]] = true
	}

	scanErr := scanner.Err()
	if scanErr != nil {
		return -1, fmt.Errorf("read %q: %w", path, scanErr)
	}

	// If a 'devices' entry was found, return version 1.
	if found["devices"] {
		return 1, nil
	}

	// If a '', (i.e. 'unified') entry was found, return version 2.
	if found[""] {
		return 2, nil
	}

	return -1, errNoDeviceOrUnifiedCgroup
}
