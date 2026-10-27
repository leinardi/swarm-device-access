//go:build goldencapture

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

package policy

import (
	"path/filepath"
	"strconv"
	"testing"

	"github.com/leinardi/swarm-device-access/internal/goldencapture"
)

// The policy golden: every function of this package whose result the Rust port must reproduce,
// on inputs that reach each branch, plus the Go standard library behavior it is built on
// (strconv.ParseBool, filepath.Match, filepath.Clean). An error is its text; "" is no error.

type goldenParseBool struct {
	Input string `json:"input"`
	Value bool   `json:"value"`
	Error string `json:"error"`
}

type goldenMode struct {
	Input string `json:"input"`
	Mode  string `json:"mode"`
	Error string `json:"error"`
}

// goldenContainer is a Container, with a nil list kept apart from an empty one.
type goldenContainer struct {
	Enable      *bool    `json:"enable"`
	DeviceAllow []string `json:"deviceAllow"`
	DeviceDeny  []string `json:"deviceDeny"`
}

type goldenParseContainer struct {
	Labels    map[string]string `json:"labels"`
	Container *goldenContainer  `json:"container"`
	Error     string            `json:"error"`
	Unknown   []string          `json:"unknownLabels"`
}

type goldenGlobs struct {
	Patterns []string `json:"patterns"`
	Error    string   `json:"error"`
}

type goldenGlobal struct {
	Mode        string   `json:"mode"`
	DeviceAllow []string `json:"deviceAllow"`
	DeviceDeny  []string `json:"deviceDeny"`
	Error       string   `json:"validateError"`
}

// goldenDecision is every predicate of one global and container policy on one path.
type goldenDecision struct {
	Global            goldenGlobal    `json:"global"`
	Container         goldenContainer `json:"container"`
	Path              string          `json:"path"`
	Enabled           bool            `json:"enabled"`
	Denied            bool            `json:"denied"`
	Authorized        bool            `json:"authorized"`
	DeviceAllowed     bool            `json:"deviceAllowed"`
	ExplicitlyAllowed bool            `json:"explicitlyAllowed"`
}

type goldenMatch struct {
	Pattern string `json:"pattern"`
	Name    string `json:"name"`
	Matched bool   `json:"matched"`
	Error   string `json:"error"`
}

type goldenClean struct {
	Input  string `json:"input"`
	Output string `json:"output"`
}

type goldenPolicy struct {
	ParseBool      []goldenParseBool      `json:"parseBool"`
	ParseMode      []goldenMode           `json:"parseMode"`
	ParseContainer []goldenParseContainer `json:"parseContainer"`
	ValidateGlobs  []goldenGlobs          `json:"validateGlobs"`
	GlobalValidate []goldenGlobal         `json:"globalValidate"`
	Decisions      []goldenDecision       `json:"decisions"`
	Match          []goldenMatch          `json:"filepathMatch"`
	Clean          []goldenClean          `json:"filepathClean"`
}

func errorText(err error) string {
	if err == nil {
		return ""
	}

	return err.Error()
}

func toGolden(container Container) goldenContainer {
	return goldenContainer(container)
}

func goldenParseBools() []goldenParseBool {
	inputs := []string{
		"1", "t", "T", "TRUE", "true", "True", "0", "f", "F", "FALSE", "false", "False",
		"", " true", "true ", "tRUE", "fALSE", "yes", "no", "on", "off", "y", "n", "2", "-1",
		"01", "trUe", "\ttrue",
	}

	results := make([]goldenParseBool, 0, len(inputs))

	for _, input := range inputs {
		value, err := strconv.ParseBool(input)
		results = append(
			results,
			goldenParseBool{Input: input, Value: value, Error: errorText(err)},
		)
	}

	return results
}

func goldenParseModes() []goldenMode {
	inputs := []string{"opt-in", "all", "", "ALL", "Opt-In", "optin", "opt_in", " all", "none"}
	results := make([]goldenMode, 0, len(inputs))

	for _, input := range inputs {
		mode, err := ParseMode(input)
		results = append(
			results,
			goldenMode{Input: input, Mode: string(mode), Error: errorText(err)},
		)
	}

	return results
}

func goldenParseContainers() []goldenParseContainer {
	labelSets := []map[string]string{
		{},
		{"com.example.other": "x"},
		{LabelEnable: "true"},
		{LabelEnable: "false"},
		{LabelEnable: "1"},
		{LabelEnable: "yes"},
		{LabelEnable: ""},
		{LabelEnable: " true"},
		{LabelDeviceAllow: "/dev/dri/*"},
		{LabelDeviceAllow: "/dev/dri/*, /dev/snd/*"},
		{LabelDeviceAllow: " /dev/a ,,/dev/b, "},
		{LabelDeviceAllow: ""},
		{LabelDeviceAllow: " "},
		{LabelDeviceAllow: " , ,"},
		{LabelDeviceAllow: "/etc/passwd"},
		{LabelDeviceAllow: "/dev/../etc"},
		{LabelDeviceAllow: "/dev/dri/"},
		{LabelDeviceAllow: "/dev/[a"},
		{LabelDeviceAllow: "/dev\\x"},
		{LabelDeviceDeny: "/dev/sda*"},
		{LabelDeviceDeny: "dev/sda"},
		{LabelDeviceDeny: "/dev//sda"},
		{LabelEnable: "maybe", LabelDeviceAllow: "/dev/[a"},
		{LabelDeviceAllow: "/dev/[a", LabelDeviceDeny: "/etc"},
		{LabelEnable: "true", LabelDeviceAllow: "/dev/dri/*", LabelDeviceDeny: "/dev/dri/card1"},
		{LabelPrefix + "enabled": "true", LabelPrefix + "device_allow": "/dev/x", LabelPrefix: "x"},
		{LabelPrefix + "zeta": "1", LabelPrefix + "alpha": "2", LabelEnable: "true"},
		{"SWARM-DEVICE-ACCESS.enable": "true"},
	}

	results := make([]goldenParseContainer, 0, len(labelSets))

	for _, labels := range labelSets {
		container, err := ParseContainer(labels)

		result := goldenParseContainer{
			Labels: labels, Error: errorText(err), Unknown: UnknownLabels(labels),
		}
		if err == nil {
			parsed := toGolden(container)
			result.Container = &parsed
		}

		results = append(results, result)
	}

	return results
}

func goldenValidateGlobs() []goldenGlobs {
	lists := [][]string{
		nil,
		{},
		{"/dev/null"},
		{"/dev/dri/*", "/dev/snd/*"},
		{"/dev/[ab]?", "/dev/[!a]*", "/dev/[^a]*", "/dev/\\*"},
		{"/dev/[a-"},
		{"/dev/["},
		{"/dev/\\"},
		{"/dev/[]a]"},
		{"/dev/a]"},
		{"/dev/"},
		{"/dev"},
		{"/devices/x"},
		{"dev/x"},
		{"/dev/./x"},
		{"/dev/x/"},
		{"/dev/x/.."},
		{"/dev//x"},
		{""},
		{"/dev/ok", "/etc/bad", "/dev/[bad"},
		{"/dev/[a-z]*[0-9]"},
		{"/dev/[z-a]"},
		{"/dev/[\\]]"},
	}

	results := make([]goldenGlobs, 0, len(lists))

	for _, patterns := range lists {
		results = append(
			results,
			goldenGlobs{Patterns: patterns, Error: errorText(ValidateGlobs(patterns))},
		)
	}

	return results
}

func goldenGlobals() []Global {
	return []Global{
		{Mode: ModeAll},
		{Mode: ModeOptIn, DeviceAllow: []string{"/dev/dri/*"}},
		{Mode: ModeAll, DeviceDeny: []string{"/dev/sd*"}},
		{
			Mode:        ModeAll,
			DeviceAllow: []string{"/dev/dri/*", "/dev/null"},
			DeviceDeny:  []string{"/dev/dri/card1"},
		},
		{Mode: ""},
		{Mode: "bogus"},
		{Mode: ModeAll, DeviceAllow: []string{"/etc/x"}},
		{Mode: ModeAll, DeviceDeny: []string{"/dev/[a"}},
		{Mode: ModeAll, DeviceAllow: []string{"/dev/x/"}, DeviceDeny: []string{"/etc"}},
	}
}

func toGoldenGlobal(global Global) goldenGlobal {
	return goldenGlobal{
		Mode: string(global.Mode), DeviceAllow: global.DeviceAllow, DeviceDeny: global.DeviceDeny,
		Error: errorText(global.Validate()),
	}
}

// goldenDecisions evaluates every predicate for the valid globals and the two invalid modes
// (which must enable nothing), against containers and paths that reach each branch.
func goldenDecisions() []goldenDecision {
	containers := []Container{
		{},
		{Enable: new(true)},
		{Enable: new(false)},
		{Enable: new(true), DeviceAllow: []string{"/dev/dri/card0"}},
		{DeviceDeny: []string{"/dev/null"}},
		{DeviceAllow: []string{}},
		{DeviceAllow: []string{"/dev/sd*"}},
	}
	paths := []string{
		"/dev/dri/card0",
		"/dev/dri/card1",
		"/dev/null",
		"/dev/sda",
		"/dev/dri/by-path/x",
	}

	globals := goldenGlobals()[:6]
	results := make([]goldenDecision, 0, len(globals)*len(containers)*len(paths))

	for _, global := range globals {
		for _, container := range containers {
			for _, path := range paths {
				results = append(results, goldenDecision{
					Global:            toGoldenGlobal(global),
					Container:         toGolden(container),
					Path:              path,
					Enabled:           global.Enabled(container),
					Denied:            global.Denied(container, path),
					Authorized:        global.Authorized(container, path),
					DeviceAllowed:     global.DeviceAllowed(container, path),
					ExplicitlyAllowed: global.ExplicitlyAllowed(container, path),
				})
			}
		}
	}

	return results
}

// Multi-byte runes for the rune-wise matching and cleaning cases, built from their code points.
var (
	hanSun  = string(rune(0x65e5))
	hanBook = string(rune(0x672c))
	hanWord = string(rune(0x8a9e))
)

func goldenMatches() []goldenMatch {
	cases := [][2]string{
		{"*", ""},
		{"*", "abc"},
		{"*", "a/b"},
		{"/dev/*", "/dev/sda"},
		{"/dev/*", "/dev/dri/card0"},
		{"/dev/*/*", "/dev/dri/card0"},
		{"/dev/sd?", "/dev/sda"},
		{"/dev/sd?", "/dev/sda1"},
		{"/dev/sd?", "/dev/sd/"},
		{"/dev/sd[a-c]", "/dev/sdb"},
		{"/dev/sd[a-c]", "/dev/sdd"},
		{"/dev/sd[!a-c]", "/dev/sdd"},
		{"/dev/sd[^a-c]", "/dev/sdd"},
		{"/dev/sd[^a-c]", "/dev/sda"},
		{"/dev/sd[a-c", "/dev/sda"},
		{"/dev/sd[", "/dev/sda"},
		{"/dev/\\*", "/dev/*"},
		{"/dev/\\*", "/dev/a"},
		{"/dev/\\", "/dev/"},
		{"a\\", "a"},
		{"[]a]", "]"},
		{"[]a]", "a"},
		{"[-]", "-"},
		{"[x-]", "x"},
		{"[a-]", "b"},
		{"[\\]]", "]"},
		{"[\\-]", "-"},
		{"[z-a]", "m"},
		{"*x", "xxx"},
		{"a*b*c", "abxbc"},
		{"a*b*c", "abxbcx"},
		{"*/b", "a/b"},
		{"a*", "a/b"},
		{"[!]", "a"},
		{"[", "["},
		{"?", hanSun},
		{"[" + hanSun + hanBook + "]", hanBook},
		{"[^" + hanSun + "]", hanBook},
		{"a?b", "a/b"},
		{"", ""},
		{"", "a"},
		{"*[", "a"},
		{"a[", "a"},
		{"/dev/dri/*", "/dev/dri/"},
		{"/dev/[a", "/dev/b"},
		{"[a-b-c]", "a"},
		{"\\[", "["},
		{"[\\", "a"},
		{"*?", ""},
		{"**", "ab"},
	}

	results := make([]goldenMatch, 0, len(cases))

	for _, pair := range cases {
		matched, err := filepath.Match(pair[0], pair[1])
		results = append(results, goldenMatch{
			Pattern: pair[0], Name: pair[1], Matched: matched, Error: errorText(err),
		})
	}

	return results
}

func goldenCleans() []goldenClean {
	inputs := []string{
		"", ".", "..", "/", "//", "///a", "/dev/", "/dev//sda", "/dev/./sda", "/dev/../etc",
		"/../dev", "a/b/../..", "a/../../b", "../../a", "/dev/sda/", "dev", "./dev", "/dev/*/",
		"/a/b/c/../../d", "a//b/./c/..", "/.", "/..", "a/.", "/dev/dri/by-path/../card0",
		"/dev/\\x/..", hanSun + hanBook + "/../" + hanWord,
	}

	results := make([]goldenClean, 0, len(inputs))

	for _, input := range inputs {
		results = append(results, goldenClean{Input: input, Output: filepath.Clean(input)})
	}

	return results
}

// TestGoldenPolicy captures tests/testdata/policy/policy.json.
func TestGoldenPolicy(t *testing.T) {
	globals := make([]goldenGlobal, 0, len(goldenGlobals()))
	for _, global := range goldenGlobals() {
		globals = append(globals, toGoldenGlobal(global))
	}

	golden := goldenPolicy{
		ParseBool:      goldenParseBools(),
		ParseMode:      goldenParseModes(),
		ParseContainer: goldenParseContainers(),
		ValidateGlobs:  goldenValidateGlobs(),
		GlobalValidate: globals,
		Decisions:      goldenDecisions(),
		Match:          goldenMatches(),
		Clean:          goldenCleans(),
	}

	goldencapture.Check(t, "tests/testdata/policy/policy.json", goldencapture.JSON(t, golden))
}
