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

package cgroup

import (
	"errors"
	"math"
	"strings"
	"testing"

	"github.com/cilium/ebpf/asm"
)

//nolint:modernize // ptr64 takes address of value; new(int64) would return zero-value
func ptr64(v int64) *int64 { return &v }

func newProgram(t *testing.T) *program {
	t.Helper()

	p := &program{}
	p.init()

	return p
}

func TestAppendDevice_CharDevice_RWM(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{
		Allow:  true,
		Type:   "c",
		Major:  ptr64(195),
		Minor:  ptr64(0),
		Access: "rwm",
	}

	err := p.appendDevice(rule, "pfx")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	// hasType=true (1 JNE), hasAccess=false (rwm skips), hasMajor=true (1 JNE),
	// hasMinor=true (1 JNE), acceptBlock (2 insts) = 5 appended to init's 5 = 10 total.
	if len(p.insts) != 10 {
		t.Errorf("instruction count = %d, want 10", len(p.insts))
	}
}

func TestAppendDevice_BlockDevice_ReadOnly(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{
		Allow:  true,
		Type:   "b",
		Major:  ptr64(8),
		Minor:  ptr64(1),
		Access: "r",
	}

	err := p.appendDevice(rule, "pfx")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	// hasType=true (1), hasAccess=true r≠rwm (3: Mov+And+JNE.Reg),
	// hasMajor=true (1), hasMinor=true (1), acceptBlock (2) = 8 appended + 5 init = 13 total.
	if len(p.insts) != 13 {
		t.Errorf("instruction count = %d, want 13", len(p.insts))
	}
}

func TestAppendDevice_Wildcard_SetsFlag(t *testing.T) {
	p := newProgram(t)
	wildcard := DeviceRule{
		Allow:  true,
		Type:   "a",
		Major:  ptr64(-1),
		Minor:  ptr64(-1),
		Access: "rwm",
	}

	err := p.appendDevice(wildcard, "pfx")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	if !p.hasWildCard {
		t.Error("hasWildCard should be set after appending wildcard rule")
	}

	// Subsequent appends are no-ops when hasWildCard is set.
	before := len(p.insts)

	rule := DeviceRule{Allow: true, Type: "c", Major: ptr64(1), Minor: ptr64(0), Access: "rwm"}

	err = p.appendDevice(rule, "pfx")
	if err != nil {
		t.Fatalf("unexpected error after wildcard: %v", err)
	}

	if len(p.insts) != before {
		t.Errorf("instructions grew after wildcard: %d → %d", before, len(p.insts))
	}
}

func TestAppendDevice_NoMajorNoMinor(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{
		Allow:  true,
		Type:   "c",
		Major:  ptr64(-1), // -1 → hasMajor=false
		Minor:  ptr64(-1), // -1 → hasMinor=false
		Access: "rwm",
	}

	err := p.appendDevice(rule, "pfx")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	// hasType=true (1), hasAccess=false, hasMajor=false, hasMinor=false,
	// acceptBlock (2) = 3 appended + 5 init = 8 total.
	if len(p.insts) != 8 {
		t.Errorf("instruction count = %d, want 8", len(p.insts))
	}
}

func TestAppendDevice_NilMajor(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{Allow: true, Type: "c", Minor: ptr64(0), Access: "rwm"}

	err := p.appendDevice(rule, "pfx")
	if !errors.Is(err, errNoMajor) {
		t.Fatalf("appendDevice() err = %v, want %v", err, errNoMajor)
	}
}

func TestAppendDevice_NilMinor(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{Allow: true, Type: "c", Major: ptr64(1), Access: "rwm"}

	err := p.appendDevice(rule, "pfx")
	if !errors.Is(err, errNoMinor) {
		t.Fatalf("appendDevice() err = %v, want %v", err, errNoMinor)
	}
}

func TestAppendDevice_Deny(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{
		Allow:  false,
		Type:   "c",
		Major:  ptr64(1),
		Minor:  ptr64(3),
		Access: "rwm",
	}

	err := p.appendDevice(rule, "pfx")
	if err != nil {
		t.Fatalf("unexpected error: %v", err)
	}

	// Same shape as allow but acceptBlock emits R0=0; instruction count is the same.
	if len(p.insts) != 10 {
		t.Errorf("instruction count = %d, want 10", len(p.insts))
	}
}

func TestAppendDevice_InvalidType(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{
		Allow:  true,
		Type:   "x",
		Major:  ptr64(1),
		Minor:  ptr64(0),
		Access: "rwm",
	}

	err := p.appendDevice(rule, "pfx")
	if err == nil {
		t.Fatal("expected error for invalid type, got nil")
	}

	if !strings.Contains(err.Error(), "invalid DeviceType") {
		t.Errorf("error %q does not mention invalid DeviceType", err.Error())
	}
}

func TestAppendDevice_MajorOverflow(t *testing.T) {
	p := newProgram(t)
	overflow := int64(math.MaxUint32) + 1
	rule := DeviceRule{
		Allow:  true,
		Type:   "c",
		Major:  &overflow,
		Minor:  ptr64(0),
		Access: "rwm",
	}

	err := p.appendDevice(rule, "pfx")
	if err == nil {
		t.Fatal("expected error for major overflow, got nil")
	}

	if !strings.Contains(err.Error(), "invalid major") {
		t.Errorf("error %q does not mention invalid major", err.Error())
	}
}

func TestAppendDevice_MinorOverflow(t *testing.T) {
	p := newProgram(t)
	overflow := int64(math.MaxUint32) + 1
	rule := DeviceRule{
		Allow:  true,
		Type:   "c",
		Major:  ptr64(1),
		Minor:  &overflow,
		Access: "rwm",
	}

	err := p.appendDevice(rule, "pfx")
	if err == nil {
		t.Fatal("expected error for minor overflow, got nil")
	}

	if !strings.Contains(err.Error(), "invalid minor") {
		t.Errorf("error %q does not mention invalid minor", err.Error())
	}

	// Confirm the error contains the minor value, not the major value (the bug we fixed).
	if !strings.Contains(err.Error(), "4294967296") {
		t.Errorf("error %q does not contain the offending minor value 4294967296", err.Error())
	}
}

func TestAppendDevice_AfterFinalize(t *testing.T) {
	p := newProgram(t)
	rule := DeviceRule{Allow: true, Type: "c", Major: ptr64(1), Minor: ptr64(0), Access: "rwm"}

	err := p.appendDevice(rule, "pfx")
	if err != nil {
		t.Fatalf("first append: %v", err)
	}

	_, err = p.finalize(asm.Instructions{asm.Return()}, "pfx")
	if err != nil {
		t.Fatalf("finalize: %v", err)
	}

	// appendDevice after finalize must return an error.
	err = p.appendDevice(rule, "pfx")
	if err == nil {
		t.Fatal("expected error after finalize, got nil")
	}

	if !strings.Contains(err.Error(), "finalized") {
		t.Errorf("error %q does not mention finalized", err.Error())
	}
}

func TestPrependDeviceFilter_RoundTrip(t *testing.T) {
	rules := []DeviceRule{
		{Allow: true, Type: "c", Major: ptr64(195), Minor: ptr64(0), Access: "rwm"},
		{Allow: true, Type: "b", Major: ptr64(8), Minor: ptr64(0), Access: "rwm"},
	}

	insts, err := PrependDeviceFilter(rules, nil)
	if err != nil {
		t.Fatalf("PrependDeviceFilter: %v", err)
	}

	if len(insts) == 0 {
		t.Error("PrependDeviceFilter returned empty instructions")
	}
}
