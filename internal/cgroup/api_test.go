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
