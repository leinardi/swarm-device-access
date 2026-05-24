/*
 * Copyright 2026 Roberto Leinardi
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

package policy_test

import (
	"testing"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

// TestEnabled covers the full opt-in/all decision matrix (proposal items 1–3).
func TestEnabled(t *testing.T) {
	t.Parallel()

	trueVal := true
	falseVal := false

	cases := []struct {
		name   string
		mode   policy.Mode
		enable *bool // nil = label absent
		want   bool
	}{
		// opt-in mode
		{"opt-in unlabelled skipped", policy.ModeOptIn, nil, false},
		{"opt-in enable=true processed", policy.ModeOptIn, &trueVal, true},
		{"opt-in enable=false skipped", policy.ModeOptIn, &falseVal, false},
		// all mode
		{"all unlabelled processed", policy.ModeAll, nil, true},
		{"all enable=true processed", policy.ModeAll, &trueVal, true},
		{"all enable=false skipped", policy.ModeAll, &falseVal, false},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			g := policy.Global{Mode: tc.mode}
			c := policy.Container{Enable: tc.enable}
			got := g.Enabled(c)

			if got != tc.want {
				t.Errorf(
					"Enabled() = %v, want %v (mode=%s enable=%v)",
					got,
					tc.want,
					tc.mode,
					tc.enable,
				)
			}
		})
	}
}

// TestDeviceAllowed covers items 4–6: deny priority and narrowing semantics.
func TestDeviceAllowed(t *testing.T) {
	t.Parallel()

	cases := []struct {
		name           string
		globalAllow    []string
		globalDeny     []string
		containerAllow []string
		containerDeny  []string
		path           string
		want           bool
	}{
		// empty lists = allow all
		{"no restrictions", nil, nil, nil, nil, "/dev/nvidia0", true},
		// global allow
		{"global allow matches", []string{"/dev/nvidia*"}, nil, nil, nil, "/dev/nvidia0", true},
		{"global allow no match", []string{"/dev/nvidia*"}, nil, nil, nil, "/dev/sda", false},
		// global deny beats container allow (item 4)
		{
			"global deny beats container allow",
			nil,
			[]string{"/dev/dri/*"},
			[]string{"/dev/dri/*"},
			nil,
			"/dev/dri/renderD128", false,
		},
		// container deny beats global allow (item 5)
		{
			"container deny beats global allow",
			[]string{"/dev/dri/*"},
			nil,
			nil,
			[]string{"/dev/dri/renderD129"},
			"/dev/dri/renderD129", false,
		},
		// container allow narrows global allow (item 6)
		{
			"container allow narrows global allow — path in both",
			[]string{"/dev/dri/*"},
			nil,
			[]string{"/dev/dri/card0"},
			nil,
			"/dev/dri/card0", true,
		},
		{
			"container allow narrows global allow — path not in container allow",
			[]string{"/dev/dri/*"},
			nil,
			[]string{"/dev/dri/card0"},
			nil,
			"/dev/dri/renderD128", false,
		},
		// deny wins over allow
		{
			"global deny wins over global allow",
			[]string{"/dev/*"},
			[]string{"/dev/null"},
			nil,
			nil,
			"/dev/null",
			false,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			g := policy.Global{DeviceAllow: tc.globalAllow, DeviceDeny: tc.globalDeny}
			c := policy.Container{DeviceAllow: tc.containerAllow, DeviceDeny: tc.containerDeny}
			got := g.DeviceAllowed(c, tc.path)

			if got != tc.want {
				t.Errorf("DeviceAllowed(%q) = %v, want %v", tc.path, got, tc.want)
			}
		})
	}
}

// TestParseContainer covers label parsing and invalid-glob fail-closed (item 7).
func TestParseContainer(t *testing.T) {
	t.Parallel()

	trueVal := true
	falseVal := false

	cases := []struct {
		name       string
		labels     map[string]string
		wantEnable *bool
		wantAllow  []string
		wantDeny   []string
		wantErr    bool
	}{
		{"nil labels", nil, nil, nil, nil, false},
		{"no policy labels", map[string]string{"other": "value"}, nil, nil, nil, false},
		{"enable=true", map[string]string{policy.LabelEnable: "true"}, &trueVal, nil, nil, false},
		{
			"enable=false",
			map[string]string{policy.LabelEnable: "false"},
			&falseVal,
			nil,
			nil,
			false,
		},
		{
			"enable=1 (truthy)",
			map[string]string{policy.LabelEnable: "1"},
			&trueVal,
			nil,
			nil,
			false,
		},
		{
			"enable=invalid fails closed",
			map[string]string{policy.LabelEnable: "yes"},
			nil,
			nil,
			nil,
			true,
		},
		{
			"device-allow single",
			map[string]string{policy.LabelDeviceAllow: "/dev/nvidia*"},
			nil,
			[]string{"/dev/nvidia*"},
			nil, false,
		},
		{
			"device-allow comma-separated",
			map[string]string{policy.LabelDeviceAllow: "/dev/dri/*, /dev/null"},
			nil,
			[]string{"/dev/dri/*", "/dev/null"},
			nil, false,
		},
		{
			"device-deny set",
			map[string]string{policy.LabelDeviceDeny: "/dev/dri/renderD129"},
			nil, nil,
			[]string{"/dev/dri/renderD129"},
			false,
		},
		{
			"invalid glob in device-allow fails closed",
			map[string]string{policy.LabelDeviceAllow: "/dev/nvidia["},
			nil, nil, nil, true,
		},
		{
			"invalid glob in device-deny fails closed",
			map[string]string{policy.LabelDeviceDeny: "/dev/bad["},
			nil, nil, nil, true,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got, err := policy.ParseContainer(tc.labels)
			if (err != nil) != tc.wantErr {
				t.Fatalf("ParseContainer() err=%v, wantErr=%v", err, tc.wantErr)
			}

			if tc.wantErr {
				return
			}

			if tc.wantEnable == nil && got.Enable != nil {
				t.Errorf("Enable = %v, want nil", *got.Enable)
			}

			if tc.wantEnable != nil {
				if got.Enable == nil {
					t.Errorf("Enable = nil, want %v", *tc.wantEnable)
				} else if *got.Enable != *tc.wantEnable {
					t.Errorf("Enable = %v, want %v", *got.Enable, *tc.wantEnable)
				}
			}

			if !sliceEqual(got.DeviceAllow, tc.wantAllow) {
				t.Errorf("DeviceAllow = %v, want %v", got.DeviceAllow, tc.wantAllow)
			}

			if !sliceEqual(got.DeviceDeny, tc.wantDeny) {
				t.Errorf("DeviceDeny = %v, want %v", got.DeviceDeny, tc.wantDeny)
			}
		})
	}
}

// TestValidateGlobs covers item 7 for the global policy.
func TestValidateGlobs(t *testing.T) {
	t.Parallel()

	cases := []struct {
		patterns []string
		wantErr  bool
	}{
		{nil, false},
		{[]string{"/dev/nvidia*", "/dev/dri/*"}, false},
		{[]string{"/dev/nvidia["}, true},
		{[]string{"/dev/valid", "/dev/bad["}, true},
	}

	for _, tc := range cases {
		err := policy.ValidateGlobs(tc.patterns)
		if (err != nil) != tc.wantErr {
			t.Errorf("ValidateGlobs(%v) err=%v, wantErr=%v", tc.patterns, err, tc.wantErr)
		}
	}
}

// TestGlobalValidate covers mode + glob validation.
func TestGlobalValidate(t *testing.T) {
	t.Parallel()

	cases := []struct {
		name    string
		g       policy.Global
		wantErr bool
	}{
		{"valid opt-in", policy.Global{Mode: policy.ModeOptIn}, false},
		{"valid all", policy.Global{Mode: policy.ModeAll}, false},
		{"invalid mode", policy.Global{Mode: "invalid"}, true},
		{
			"bad allow glob",
			policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/bad["}},
			true,
		},
		{
			"bad deny glob",
			policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/bad["}},
			true,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			err := tc.g.Validate()
			if (err != nil) != tc.wantErr {
				t.Errorf("Validate() err=%v, wantErr=%v", err, tc.wantErr)
			}
		})
	}
}

func sliceEqual(a, b []string) bool {
	if len(a) != len(b) {
		return false
	}

	for i := range a {
		if a[i] != b[i] {
			return false
		}
	}

	return true
}
