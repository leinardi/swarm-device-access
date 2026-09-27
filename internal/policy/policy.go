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

// Package policy evaluates container-level and daemon-level device-access policy.
// It is cross-platform: no Linux-specific imports, so tests run on any OS.
package policy

import (
	"errors"
	"fmt"
	"path/filepath"
	"sort"
	"strconv"
	"strings"
)

// Label key constants for the swarm-device-access policy labels.
const (
	LabelPrefix      = "swarm-device-access."
	LabelEnable      = LabelPrefix + "enable"
	LabelDeviceAllow = LabelPrefix + "device-allow"
	LabelDeviceDeny  = LabelPrefix + "device-deny"
)

// devPrefix is what every glob must start with: only paths under /dev are
// ever matched against it.
const devPrefix = "/dev/"

var (
	errGlobNotClean   = errors.New("glob pattern is not a clean path")
	errGlobOutsideDev = errors.New("glob pattern must start with /dev/")
)

// Mode controls which containers the daemon processes by default.
type Mode string

const (
	// ModeOptIn processes only containers that explicitly set LabelEnable=true.
	ModeOptIn Mode = "opt-in"
	// ModeAll processes all containers unless they explicitly set LabelEnable=false.
	ModeAll Mode = "all"
)

// Global is the daemon-wide policy parsed once from flags/config.
type Global struct {
	Mode        Mode
	DeviceAllow []string
	DeviceDeny  []string
}

// Container is the per-container policy parsed from Docker container labels.
type Container struct {
	Enable      *bool    // nil = unset; true = opted in; false = opted out
	DeviceAllow []string // comma-separated values from LabelDeviceAllow
	DeviceDeny  []string // comma-separated values from LabelDeviceDeny
}

// ParseMode parses a mode string, returning an error for unrecognized values.
func ParseMode(modeStr string) (Mode, error) {
	switch Mode(modeStr) {
	case ModeOptIn, ModeAll:
		return Mode(modeStr), nil
	default:
		return "", fmt.Errorf( //nolint:err113 // dynamic content includes the mode string
			"unknown policy mode %q: must be %q or %q",
			modeStr, ModeOptIn, ModeAll,
		)
	}
}

// ParseContainer reads the swarm-device-access.* labels from a container's label
// map and returns a Container policy. On invalid label values an error is returned
// and the caller should fail closed (skip the container).
func ParseContainer(labels map[string]string) (Container, error) {
	var cont Container

	if raw, ok := labels[LabelEnable]; ok {
		val, err := strconv.ParseBool(raw)
		if err != nil {
			return Container{}, fmt.Errorf( //nolint:err113 // dynamic content includes the label value
				"invalid value for label %q: %q is not a boolean",
				LabelEnable,
				raw,
			)
		}

		cont.Enable = &val
	}

	allow, err := parseGlobList(labels[LabelDeviceAllow], LabelDeviceAllow)
	if err != nil {
		return Container{}, err
	}

	cont.DeviceAllow = allow

	deny, err := parseGlobList(labels[LabelDeviceDeny], LabelDeviceDeny)
	if err != nil {
		return Container{}, err
	}

	cont.DeviceDeny = deny

	return cont, nil
}

// parseGlobList splits a comma-separated glob list and validates each pattern.
func parseGlobList(raw, labelName string) ([]string, error) {
	if raw == "" {
		return nil, nil
	}

	parts := splitAndTrim(raw)

	err := ValidateGlobs(parts)
	if err != nil {
		return nil, fmt.Errorf("invalid glob in label %q: %w", labelName, err)
	}

	return parts, nil
}

// splitAndTrim splits s on commas and trims whitespace from each part.
func splitAndTrim(s string) []string {
	raw := strings.Split(s, ",")
	result := make([]string, 0, len(raw))

	for _, p := range raw {
		p = strings.TrimSpace(p)
		if p != "" {
			result = append(result, p)
		}
	}

	return result
}

// ValidateGlobs returns an error if any pattern in patterns is syntactically
// invalid, not clean, or not under /dev/. Such a glob silently never
// matches a device path, which for a deny means a device the operator meant
// to deny stays grantable; rejecting it prevents that misconfiguration.
func ValidateGlobs(patterns []string) error {
	for _, pattern := range patterns {
		_, err := filepath.Match(pattern, "")
		if err != nil {
			return fmt.Errorf("invalid glob pattern %q: %w", pattern, err)
		}

		if filepath.Clean(pattern) != pattern {
			return fmt.Errorf("%w: %q", errGlobNotClean, pattern)
		}

		if !strings.HasPrefix(pattern, devPrefix) {
			return fmt.Errorf("%w: %q", errGlobOutsideDev, pattern)
		}
	}

	return nil
}

// Validate checks that the Global policy is well-formed: mode is valid and
// every glob passes ValidateGlobs.
func (g Global) Validate() error {
	_, err := ParseMode(string(g.Mode))
	if err != nil {
		return err
	}

	err = ValidateGlobs(g.DeviceAllow)
	if err != nil {
		return fmt.Errorf("device-allow: %w", err)
	}

	err = ValidateGlobs(g.DeviceDeny)
	if err != nil {
		return fmt.Errorf("device-deny: %w", err)
	}

	return nil
}

// Enabled reports whether a container should be processed.
//
// Decision table:
//
//	enable=false → always skip (both modes)
//	opt-in mode  → process only if enable=true
//	all mode     → process unless enable=false (already handled)
func (g Global) Enabled(cpol Container) bool {
	if cpol.Enable != nil && !*cpol.Enable {
		return false
	}

	switch g.Mode {
	case ModeOptIn:
		return cpol.Enable != nil && *cpol.Enable
	case ModeAll:
		return true
	default:
		return false
	}
}

// DeviceAllowed reports whether path is permitted by the combined global and
// per-container allow/deny policy: not Denied and Authorized.
//
// Global is the maximum allowed access; per-container labels can only narrow
// it further. Deny always wins over allow.
func (g Global) DeviceAllowed(cpol Container, path string) bool {
	return !g.Denied(cpol, path) && g.Authorized(cpol, path)
}

// Denied reports whether path matches a global or a per-container deny glob.
// A device is checked under every name it was found by, and one match on
// any of them denies it.
func (g Global) Denied(cpol Container, path string) bool {
	return matchAny(g.DeviceDeny, path) || matchAny(cpol.DeviceDeny, path)
}

// Authorized reports whether path passes the allow lists: the global one and
// the per-container one, each allowing everything when empty. A device must
// be authorized under its canonical and its resolved name; missing the allow
// list under some other alias is not a reason to deny it.
func (g Global) Authorized(cpol Container, path string) bool {
	if len(g.DeviceAllow) > 0 && !matchAny(g.DeviceAllow, path) {
		return false
	}

	return len(cpol.DeviceAllow) == 0 || matchAny(cpol.DeviceAllow, path)
}

// ExplicitlyAllowed reports whether path is matched by at least one explicit
// allow glob (global or per-container) and is not denied. Unlike
// DeviceAllowed, it returns false when no allow globs are configured at all.
func (g Global) ExplicitlyAllowed(cpol Container, path string) bool {
	return (len(g.DeviceAllow) > 0 || len(cpol.DeviceAllow) > 0) && g.DeviceAllowed(cpol, path)
}

// matchAny reports whether path matches any glob pattern in patterns.
func matchAny(patterns []string, path string) bool {
	for _, p := range patterns {
		if ok, _ := filepath.Match(p, path); ok {
			return true
		}
	}

	return false
}

// knownLabels is the set of recognized swarm-device-access.* label keys.
var knownLabels = map[string]struct{}{
	LabelEnable:      {},
	LabelDeviceAllow: {},
	LabelDeviceDeny:  {},
}

// UnknownLabels returns a sorted slice of keys in labels that start with
// LabelPrefix but are not in the known label set. Returns nil when none.
func UnknownLabels(labels map[string]string) []string {
	var unknown []string

	for k := range labels {
		if strings.HasPrefix(k, LabelPrefix) {
			if _, ok := knownLabels[k]; !ok {
				unknown = append(unknown, k)
			}
		}
	}

	if len(unknown) == 0 {
		return nil
	}

	sort.Strings(unknown)

	return unknown
}
