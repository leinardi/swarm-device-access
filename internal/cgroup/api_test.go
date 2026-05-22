//go:build linux

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

package cgroup

import (
	"encoding/json"
	"os"
	"strings"
	"testing"
)

// TestDeviceRule_JSON_RoundTrip locks the inlined DeviceRule struct's JSON
// shape to match what specs.LinuxDeviceCgroup produced before inlining, so
// any future drift (renamed field, missing omitempty) is caught.
func TestDeviceRule_JSON_RoundTrip(t *testing.T) {
	major := int64(254)
	minor := int64(0)

	cases := []struct {
		name string
		rule DeviceRule
		want string
	}{
		{
			name: "full rule",
			rule: DeviceRule{
				Allow:  true,
				Type:   "c",
				Major:  &major,
				Minor:  &minor,
				Access: "rwm",
			},
			want: `{"allow":true,"type":"c","major":254,"minor":0,"access":"rwm"}`,
		},
		{
			name: "deny rule with no minor",
			rule: DeviceRule{
				Allow: false,
				Type:  "b",
				Major: &major,
			},
			want: `{"allow":false,"type":"b","major":254}`,
		},
		{
			name: "wildcard rule (all fields zero)",
			rule: DeviceRule{Allow: true},
			want: `{"allow":true}`,
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			marshaled, err := json.Marshal(tc.rule)
			if err != nil {
				t.Fatalf("marshal: %v", err)
			}

			if string(marshaled) != tc.want {
				t.Errorf("marshal = %s, want %s", marshaled, tc.want)
			}

			var unmarshaled DeviceRule
			if err := json.Unmarshal(marshaled, &unmarshaled); err != nil {
				t.Fatalf("unmarshal: %v", err)
			}

			if unmarshaled.Allow != tc.rule.Allow ||
				unmarshaled.Type != tc.rule.Type ||
				unmarshaled.Access != tc.rule.Access {
				t.Errorf("scalar fields lost in round trip: got %+v, want %+v",
					unmarshaled, tc.rule)
			}
		})
	}
}

func TestNew_InvalidVersion(t *testing.T) {
	_, err := New(-1)
	if err == nil {
		t.Error("New(-1) must return an error")
	}

	_, err = New(99)
	if err == nil {
		t.Error("New(99) must return an error")
	}
}

func TestNew_ValidVersions(t *testing.T) {
	v1, err := New(1)
	if err != nil {
		t.Errorf("New(1) returned err: %v", err)
	}

	if v1 == nil {
		t.Error("New(1) returned nil Interface")
	}

	v2, err := New(2)
	if err != nil {
		t.Errorf("New(2) returned err: %v", err)
	}

	if v2 == nil {
		t.Error("New(2) returned nil Interface")
	}
}

func TestNew_InvalidVersionIncludesValue(t *testing.T) {
	_, err := New(42)
	if err == nil {
		t.Fatal("New(42) must return an error")
	}

	if !strings.Contains(err.Error(), "42") {
		t.Errorf("error %q does not include the invalid version number", err.Error())
	}
}

func TestScanCGroupVersion(t *testing.T) {
	cases := []struct {
		fixture    string
		wantVer    int
		wantErrSub string
	}{
		{"testdata/cgroup_v1.txt", 1, ""},
		{"testdata/cgroup_v2.txt", 2, ""},
		// hybrid: both "" and "devices" present; devices takes priority (found["devices"] checked first)
		{"testdata/cgroup_hybrid.txt", 1, ""},
		{"testdata/cgroup_no_match.txt", -1, "no devices or unified"},
		{"testdata/cgroup_malformed.txt", -1, "malformed"},
	}

	for _, tc := range cases {
		t.Run(tc.fixture, func(t *testing.T) {
			f, err := os.Open(tc.fixture)
			if err != nil {
				t.Fatalf("open fixture: %v", err)
			}
			defer f.Close()

			got, err := scanCGroupVersion(f, tc.fixture)
			if tc.wantErrSub != "" {
				if err == nil {
					t.Fatalf("expected error containing %q, got nil", tc.wantErrSub)
				}
				if !strings.Contains(err.Error(), tc.wantErrSub) {
					t.Errorf("error %q does not contain %q", err.Error(), tc.wantErrSub)
				}
				return
			}
			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}
			if got != tc.wantVer {
				t.Errorf("got version %d, want %d", got, tc.wantVer)
			}
		})
	}
}
