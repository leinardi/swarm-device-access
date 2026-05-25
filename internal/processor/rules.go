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
	"fmt"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// IsMountSource reports whether path is /dev or a path under /dev/.
func IsMountSource(path string) bool {
	return path == "/dev" || strings.HasPrefix(path, "/dev/")
}

// CollectMountRules returns the DeviceRules and any errors for a mount source.
// Directory mounts are walked; per-file policy checks apply.
func CollectMountRules(
	mountPath string,
	gpol policy.Global,
	cpol policy.Container,
) ([]cgroup.DeviceRule, []error) {
	if !gpol.DeviceAllowed(cpol, mountPath) {
		logger.L().Debug("device mount excluded by policy", "path", mountPath)

		return nil, nil
	}

	fileInfo, err := os.Stat(mountPath)
	if err != nil {
		return nil, []error{fmt.Errorf("stat %q: %w", mountPath, err)}
	}

	if !fileInfo.IsDir() {
		rule, ruleErr := collectDeviceRule(mountPath)
		if ruleErr != nil {
			return nil, []error{ruleErr}
		}

		return []cgroup.DeviceRule{rule}, nil
	}

	var (
		rules []cgroup.DeviceRule
		errs  []error
	)

	walkErr := filepath.Walk(mountPath, func(walkedPath string, info os.FileInfo, err error) error {
		if err != nil {
			errs = append(errs, err)

			return nil
		}

		if info.IsDir() {
			return nil
		}

		if !gpol.DeviceAllowed(cpol, walkedPath) {
			logger.L().Debug("device file excluded by policy", "path", walkedPath)

			return nil
		}

		rule, ruleErr := collectDeviceRule(walkedPath)
		if ruleErr != nil {
			errs = append(errs, fmt.Errorf("device rule for %q: %w", walkedPath, ruleErr))

			return nil
		}

		rules = append(rules, rule)

		return nil
	})
	if walkErr != nil {
		errs = append(errs, fmt.Errorf("walk %q: %w", mountPath, walkErr))
	}

	return rules, errs
}

// collectDeviceRule returns the DeviceRule for a single (non-directory) device file.
func collectDeviceRule(devicePath string) (cgroup.DeviceRule, error) {
	deviceType, major, minor, err := getDeviceInfo(devicePath)
	if err != nil {
		return cgroup.DeviceRule{}, err
	}

	return cgroup.DeviceRule{
		Allow:  true,
		Access: "rwm",
		Type:   deviceType,
		Major:  &major,
		Minor:  &minor,
	}, nil
}

func getDeviceInfo(devicePath string) (deviceType string, major, minor int64, statErr error) {
	var stat unix.Stat_t

	statErr = unix.Stat(devicePath, &stat)
	if statErr != nil {
		return "", -1, -1, fmt.Errorf("stat %q: %w", devicePath, statErr)
	}

	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFBLK:
		deviceType = "b"
	case unix.S_IFCHR:
		deviceType = "c"
	default:
		return "", -1, -1, fmt.Errorf( //nolint:err113 // dynamic content includes device path
			"device %q is neither character nor block device",
			devicePath,
		)
	}

	major = int64(unix.Major(stat.Rdev))
	minor = int64(unix.Minor(stat.Rdev))

	return deviceType, major, minor, nil
}
