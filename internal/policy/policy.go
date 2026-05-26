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
	"fmt"
	"maps"
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
// invalid. Malformed globs silently never match; this prevents silent
// policy misconfiguration.
func ValidateGlobs(patterns []string) error {
	for _, p := range patterns {
		_, err := filepath.Match(p, "")
		if err != nil {
			return fmt.Errorf("invalid glob pattern %q: %w", p, err)
		}
	}

	return nil
}

// Validate checks that the Global policy is well-formed: mode is valid and
// all glob lists are syntactically correct.
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
// per-container allow/deny policy.
//
// Global is the maximum allowed access; per-container labels can only narrow
// it further. Deny always wins over allow.
//

func (g Global) DeviceAllowed(cpol Container, path string) bool {
	if matchAny(g.DeviceDeny, path) {
		return false
	}

	if matchAny(cpol.DeviceDeny, path) {
		return false
	}

	if len(g.DeviceAllow) > 0 && !matchAny(g.DeviceAllow, path) {
		return false
	}

	if len(cpol.DeviceAllow) > 0 && !matchAny(cpol.DeviceAllow, path) {
		return false
	}

	return true
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

// MergeLabels returns a merged label map: service labels as base, container
// labels win on conflict. Nil inputs are treated as empty maps.
func MergeLabels(service, container map[string]string) map[string]string {
	merged := make(map[string]string, len(service)+len(container))

	maps.Copy(merged, service)
	maps.Copy(merged, container)

	return merged
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
