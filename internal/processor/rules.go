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
	"io/fs"
	"os"
	"path/filepath"
	"strings"

	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

const (
	maxDirDepth    = 8
	maxLogChildren = 32

	deviceAccessAll = "rwm"
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
	linfo, err := os.Lstat(mountPath)
	if err != nil {
		return nil, []error{fmt.Errorf("stat %q: %w", mountPath, err)}
	}

	effectivePath := mountPath
	if linfo.Mode()&os.ModeSymlink != 0 {
		resolved, resolveErr := filepath.EvalSymlinks(mountPath)
		if resolveErr != nil {
			return nil, []error{fmt.Errorf("resolve symlink %q: %w", mountPath, resolveErr)}
		}

		effectivePath = resolved

		linfo, err = os.Lstat(effectivePath)
		if err != nil {
			return nil, []error{fmt.Errorf("stat %q: %w", effectivePath, err)}
		}
	}

	if !linfo.IsDir() {
		if !gpol.DeviceAllowed(cpol, effectivePath) {
			logger.L().Debug("device mount excluded by policy", "path", effectivePath)

			return nil, nil
		}

		rule, ruleErr := collectDeviceRule(effectivePath)
		if ruleErr != nil {
			return nil, []error{ruleErr}
		}

		return []cgroup.DeviceRule{rule}, nil
	}

	state := &mountWalkState{
		mountPath: effectivePath,
		gpol:      gpol,
		cpol:      cpol,
	}

	walkErr := filepath.WalkDir(effectivePath, state.visitEntry)
	if walkErr != nil {
		state.errs = append(state.errs, fmt.Errorf("walk %q: %w", effectivePath, walkErr))
	}

	if len(state.rules) == 0 && len(state.childrenSeen) > 0 && len(state.errs) == 0 {
		logger.L().Warn("mount excluded: no children matched allow/deny policy",
			"path", effectivePath,
			"children_seen", state.childrenSeen,
			"allow_globs", append(gpol.DeviceAllow, cpol.DeviceAllow...),
			"deny_globs", append(gpol.DeviceDeny, cpol.DeviceDeny...),
		)
	}

	return state.rules, state.errs
}

type mountWalkState struct {
	mountPath    string
	gpol         policy.Global
	cpol         policy.Container
	rules        []cgroup.DeviceRule
	errs         []error
	childrenSeen []string
}

func (s *mountWalkState) visitEntry(walkedPath string, entry fs.DirEntry, entryErr error) error {
	if entryErr != nil {
		s.errs = append(s.errs, entryErr)

		return nil
	}

	if entry.IsDir() {
		if walkedPath == s.mountPath {
			return nil
		}

		depth := strings.Count(
			strings.TrimPrefix(walkedPath, s.mountPath),
			string(filepath.Separator),
		)
		if depth > maxDirDepth {
			logger.L().Debug("walk depth cap reached", "path", walkedPath)

			return filepath.SkipDir
		}

		return nil
	}

	if entry.Type()&os.ModeSymlink != 0 {
		return s.visitSymlink(walkedPath)
	}

	return s.visitRegularFile(walkedPath)
}

func (s *mountWalkState) visitSymlink(symlinkPath string) error {
	realPath, resolveErr := filepath.EvalSymlinks(symlinkPath)
	if resolveErr != nil {
		s.errs = append(s.errs, fmt.Errorf("resolve symlink %q: %w", symlinkPath, resolveErr))

		return nil
	}

	targetInfo, statErr := os.Stat(realPath)
	if statErr != nil {
		s.errs = append(s.errs, fmt.Errorf("stat symlink target %q: %w", realPath, statErr))

		return nil
	}

	if targetInfo.IsDir() {
		logger.L().Debug("symlink to directory skipped", "path", symlinkPath, "target", realPath)

		return nil
	}

	s.trackChild(realPath)

	if !s.gpol.DeviceAllowed(s.cpol, realPath) {
		logger.L().Debug("device file excluded by policy", "path", realPath)

		return nil
	}

	rule, ruleErr := collectDeviceRule(realPath)
	if ruleErr != nil {
		s.errs = append(s.errs, fmt.Errorf("device rule for %q: %w", realPath, ruleErr))

		return nil
	}

	s.rules = append(s.rules, rule)

	return nil
}

func (s *mountWalkState) visitRegularFile(filePath string) error {
	s.trackChild(filePath)

	if !s.gpol.DeviceAllowed(s.cpol, filePath) {
		logger.L().Debug("device file excluded by policy", "path", filePath)

		return nil
	}

	rule, ruleErr := collectDeviceRule(filePath)
	if ruleErr != nil {
		s.errs = append(s.errs, fmt.Errorf("device rule for %q: %w", filePath, ruleErr))

		return nil
	}

	s.rules = append(s.rules, rule)

	return nil
}

func (s *mountWalkState) trackChild(path string) {
	if len(s.childrenSeen) < maxLogChildren {
		s.childrenSeen = append(s.childrenSeen, filepath.Base(path))
	}
}

// collectDeviceRule returns the DeviceRule for a single (non-directory) device file.
func collectDeviceRule(devicePath string) (cgroup.DeviceRule, error) {
	deviceType, major, minor, err := getDeviceInfo(devicePath)
	if err != nil {
		return cgroup.DeviceRule{}, err
	}

	return cgroup.DeviceRule{
		Allow:  true,
		Access: deviceAccessAll,
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
