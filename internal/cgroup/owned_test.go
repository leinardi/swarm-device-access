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
	"bytes"
	"errors"
	"path/filepath"
	"slices"
	"testing"

	"github.com/cilium/ebpf/asm"
	"golang.org/x/sys/unix"
)

var runtimeFixtures = []string{"runc", "crun", "systemd"}

func mustCanonical(t *testing.T, insts asm.Instructions) []byte {
	t.Helper()

	raw, err := canonicalBytes(insts)
	if err != nil {
		t.Fatal(err)
	}

	return raw
}

func mustEmit(t *testing.T, rules []DeviceRule, nonce uint64, original []byte) []byte {
	t.Helper()

	raw, err := emitOwned(rules, nonce, original)
	if err != nil {
		t.Fatal(err)
	}

	return raw
}

func knownMeta() progMeta {
	return progMeta{mapIDsKnown: true}
}

// allowNullOriginal is a runtime-style original that allows exactly
// c 1:3 (any access) and denies everything else.
func allowNullOriginal() asm.Instructions {
	return asm.Instructions{
		asm.LoadMem(asm.R2, asm.R1, 0, asm.Word),
		asm.And.Imm32(asm.R2, 0xffff),
		asm.LoadMem(asm.R4, asm.R1, 4, asm.Word),
		asm.LoadMem(asm.R5, asm.R1, 8, asm.Word),
		{
			OpCode:   asm.JNE.Op(asm.ImmSource),
			Dst:      asm.R2,
			Offset:   4,
			Constant: int64(unix.BPF_DEVCG_DEV_CHAR),
		},
		{OpCode: asm.JNE.Op(asm.ImmSource), Dst: asm.R4, Offset: 3, Constant: 1},
		{OpCode: asm.JNE.Op(asm.ImmSource), Dst: asm.R5, Offset: 2, Constant: 3},
		asm.Mov.Imm32(asm.R0, 1),
		asm.Return(),
		asm.Mov.Imm32(asm.R0, 0),
		asm.Return(),
	}
}

// TestFixtures_PassGateAndRoundTrip checks, for real runc, crun and systemd
// device filters, that the original passes the gate, reloads to the same
// bytes, is not mistaken for a wrapper, and that the kernel's dump of the
// daemon's wrapper around it strips back to exactly the original.
func TestFixtures_PassGateAndRoundTrip(t *testing.T) {
	for _, name := range runtimeFixtures {
		t.Run(name, func(t *testing.T) {
			base := filepath.Join("testdata", "devfilter", name)
			orig := readFixture(t, base+".orig")
			reloaded := readFixture(t, base+".reloaded")
			wrapped := readFixture(t, base+".wrapped")

			origInsts, err := fromCanonical(orig)
			if err != nil {
				t.Fatal(err)
			}

			err = checkWrappable(origInsts, knownMeta())
			if err != nil {
				t.Fatalf("gate rejects the %s original: %v", name, err)
			}

			if !bytes.Equal(reloaded, orig) {
				t.Fatal("reloading the original changed its dump")
			}

			_, owned, err := parseOwned(orig)
			if err != nil || owned {
				t.Fatalf("original parsed as owned=%t err=%v, want an opaque original", owned, err)
			}

			if !bytes.Equal(mustEmit(t, fixtureRules(), fixtureNonce, orig), wrapped) {
				t.Fatal("kernel dump of the wrapper differs from the emitted wrapper")
			}

			parsed, owned, err := parseOwned(wrapped)
			if err != nil || !owned {
				t.Fatalf("wrapper dump: owned=%t err=%v", owned, err)
			}

			if !bytes.Equal(parsed.original, orig) {
				t.Error("stripping the wrapper dump does not return the original")
			}

			if parsed.nonce != fixtureNonce {
				t.Errorf("nonce = %#x, want %#x", parsed.nonce, fixtureNonce)
			}
		})
	}
}

func TestParseOwned_ValidatedBlockRoundTrips(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	rules := []DeviceRule{rule("c", 10, 200, "rw"), rule("b", 8, 0, "rwm"), rule("c", 189, 3, "m")}

	parsed, owned, err := parseOwned(mustEmit(t, rules, 42, orig))
	if err != nil || !owned {
		t.Fatalf("owned=%t err=%v", owned, err)
	}

	if parsed.nonce != 42 || !bytes.Equal(parsed.original, orig) {
		t.Errorf(
			"parsed nonce %d original equal %t",
			parsed.nonce,
			bytes.Equal(parsed.original, orig),
		)
	}

	if !bytes.Equal(mustEmit(t, parsed.rules, 42, orig), mustEmit(t, rules, 42, orig)) {
		t.Errorf("parsed rules %+v do not regenerate the block", parsed.rules)
	}
}

func TestParseOwned_AbsentOrMarkerless(t *testing.T) {
	// A program that starts with the very init sequence the wrapper uses,
	// but has no header, is an opaque original.
	initOnly := &program{}
	initOnly.init()

	cases := map[string]asm.Instructions{
		"runtime original": allowNullOriginal(),
		"starts with init": slices.Concat(
			initOnly.insts,
			asm.Instructions{asm.Mov.Imm32(asm.R0, 1), asm.Return()},
		),
		"starts with a non-magic dead store": {
			asm.Mov.Imm32(asm.R0, 7), asm.Mov.Imm32(asm.R0, 0), asm.Return(),
		},
	}

	for name, insts := range cases {
		parsed, owned, err := parseOwned(mustCanonical(t, insts))
		if err != nil || owned || parsed != nil {
			t.Errorf("%s: owned=%t err=%v, want an opaque original", name, owned, err)
		}
	}

	runcOrig := readFixture(t, filepath.Join("testdata", "devfilter", "runc.orig"))

	_, owned, err := parseOwned(runcOrig)
	if err != nil || owned {
		t.Errorf("runc original: owned=%t err=%v", owned, err)
	}
}

// corrupt returns the wrapper with instruction idx replaced.
func corrupt(t *testing.T, raw []byte, idx int, ins asm.Instruction) []byte {
	t.Helper()

	insts, err := fromCanonical(raw)
	if err != nil {
		t.Fatal(err)
	}

	insts[idx] = ins

	return mustCanonical(t, insts)
}

func TestParseOwned_ConflictsNeverValidate(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	rules := []DeviceRule{rule("c", 10, 200, "rwm")}
	wrapped := mustEmit(t, rules, 99, orig)

	insts, err := fromCanonical(wrapped)
	if err != nil {
		t.Fatal(err)
	}

	blockLen, _ := markerValue(insts[hdrBlockLen])
	trailerAt := int(blockLen)

	cases := map[string][]byte{
		"header without trailer":  corrupt(t, wrapped, trailerAt, asm.Mov.Imm32(asm.R0, 0)),
		"trailer nonce differs":   corrupt(t, wrapped, trailerAt+2, markerInst(98)),
		"wrong block_len":         corrupt(t, wrapped, hdrBlockLen, markerInst(blockLen+1)),
		"block_len past the end":  corrupt(t, wrapped, hdrBlockLen, markerInst(uint32(len(insts)))),
		"wrong version":           corrupt(t, wrapped, hdrVersion, markerInst(2)),
		"digest mismatch":         corrupt(t, wrapped, len(insts)-2, asm.Mov.Imm32(asm.R0, 1)),
		"header field not marker": corrupt(t, wrapped, hdrNonceHi, asm.Mov.Imm32(asm.R1, 0)),
		// Parses as a rule block but a jump no longer lands where the
		// generator puts it.
		"non-regenerable block": corrupt(
			t,
			wrapped,
			ownedHeaderLen+ownedInitLen,
			asm.Instruction{
				OpCode:   asm.JNE.Op(asm.ImmSource),
				Dst:      asm.R2,
				Offset:   1,
				Constant: int64(unix.BPF_DEVCG_DEV_CHAR),
			},
		),
		"unparseable block": corrupt(
			t,
			wrapped,
			ownedHeaderLen+ownedInitLen,
			asm.Mov.Imm32(asm.R7, 0),
		),
		"truncated header": mustCanonical(
			t,
			asm.Instructions{markerInst(ownedMagicLo), asm.Mov.Imm32(asm.R0, 0), asm.Return()},
		),
	}

	for name, raw := range cases {
		_, owned, parseErr := parseOwned(raw)
		if owned || !errors.Is(parseErr, ErrOwnedBlockConflict) {
			t.Errorf("%s: owned=%t err=%v, want ErrOwnedBlockConflict", name, owned, parseErr)
		}
	}
}

func TestCheckWrappable_Subset(t *testing.T) {
	ok := allowNullOriginal()

	err := checkWrappable(ok, knownMeta())
	if err != nil {
		t.Fatalf("minimal original rejected: %v", err)
	}

	withTail := func(insts ...asm.Instruction) asm.Instructions {
		return slices.Concat(
			asm.Instructions{asm.Mov.Imm32(asm.R0, 0)},
			insts,
			asm.Instructions{asm.Return()},
		)
	}

	cases := map[string]struct {
		insts asm.Instructions
		meta  progMeta
	}{
		"map IDs unknown":  {ok, progMeta{}},
		"uses a map":       {ok, progMeta{mapIDsKnown: true, mapCount: 1}},
		"empty":            {nil, knownMeta()},
		"helper call":      {withTail(asm.FnGetCurrentPidTgid.Call()), knownMeta()},
		"64-bit immediate": {withTail(asm.LoadImm(asm.R3, 1<<40, asm.DWord)), knownMeta()},
		"stack store":      {withTail(asm.StoreMem(asm.RFP, -8, asm.R2, asm.Word)), knownMeta()},
		"load from R2":     {withTail(asm.LoadMem(asm.R3, asm.R2, 0, asm.Word)), knownMeta()},
		"ctx load at 12":   {withTail(asm.LoadMem(asm.R3, asm.R1, 12, asm.Word)), knownMeta()},
		"alu add":          {withTail(asm.Add.Imm(asm.R3, 1)), knownMeta()},
		"jump out of program": {
			withTail(asm.Instruction{OpCode: asm.Ja.Op(asm.ImmSource), Offset: 5}),
			knownMeta(),
		},
		"jump before start": {
			withTail(asm.Instruction{OpCode: asm.JEq.Op(asm.ImmSource), Dst: asm.R2, Offset: -5}),
			knownMeta(),
		},
	}

	for name, tc := range cases {
		err = checkWrappable(tc.insts, tc.meta)
		if !errors.Is(err, ErrProgramNotWrappable) {
			t.Errorf("%s: err = %v, want ErrProgramNotWrappable", name, err)
		}
	}
}

func TestKernelAtLeast(t *testing.T) {
	cases := map[string]bool{
		"4.16.0":            true,
		"4.15.18":           false,
		"5.4.0-100-generic": true,
		"3.99":              false,
		"junk":              false,
	}

	for release, want := range cases {
		if got := kernelAtLeast(release, 4, 16); got != want {
			t.Errorf("kernelAtLeast(%q) = %t, want %t", release, got, want)
		}
	}
}

func TestWrapper_Verdicts(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	wrapped := mustEmit(
		t,
		[]DeviceRule{rule("c", 10, 200, "rwm"), rule("c", 10, 201, "r")},
		1,
		orig,
	)

	const (
		read  = unix.BPF_DEVCG_ACC_READ
		write = unix.BPF_DEVCG_ACC_WRITE
	)

	cases := []struct {
		name string
		req  devRequest
		want uint64
	}{
		{"exact device allowed", charReq(10, 200, read|write), 1},
		{"other minor denied", charReq(10, 202, read), 0},
		{
			"other type denied",
			devRequest{typ: unix.BPF_DEVCG_DEV_BLOCK, access: read, major: 10, minor: 200},
			0,
		},
		{"r rule allows r", charReq(10, 201, read), 1},
		{"r rule with rw request denied", charReq(10, 201, read|write), 0},
		{"fall-through to original allows", charReq(1, 3, read|write), 1},
		{"fall-through to original denies", charReq(1, 5, read), 0},
	}

	for _, tc := range cases {
		if got := runDeviceFilter(t, wrapped, tc.req); got != tc.want {
			t.Errorf("%s: verdict %d, want %d", tc.name, got, tc.want)
		}
	}

	// The same holds on top of a real runtime filter: /dev/null (c 1:3) is
	// in every runtime's default set, and the wrapper keeps it.
	runcOrig := readFixture(t, filepath.Join("testdata", "devfilter", "runc.orig"))
	runcWrapped := mustEmit(t, []DeviceRule{rule("c", 10, 250, "rwm")}, 1, runcOrig)

	if got := runDeviceFilter(t, runcWrapped, charReq(1, 3, read|write)); got != 1 {
		t.Errorf("runc fall-through for /dev/null: verdict %d, want 1", got)
	}

	if got := runDeviceFilter(t, runcWrapped, charReq(10, 250, read)); got != 1 {
		t.Errorf("runc wrapper grant: verdict %d, want 1", got)
	}

	if got := runDeviceFilter(t, runcOrig, charReq(10, 250, read)); got != 0 {
		t.Errorf("runc original must deny c 10:250 for this test to mean anything, got %d", got)
	}
}

// ownedOps is a fakeOps whose attached programs carry the given code.
func ownedOps(t *testing.T, programs map[string][]byte) *fakeOps {
	t.Helper()

	names := make([]string, 0, len(programs))
	for name := range programs {
		names = append(names, name)
	}

	slices.Sort(names)

	ops := newFakeOps(names...)

	for name, raw := range programs {
		insts, err := fromCanonical(raw)
		if err != nil {
			t.Fatal(err)
		}

		ops.insts[name] = insts
	}

	return ops
}

func setRules(ops *fakeOps, rules []DeviceRule) error {
	c := &cgroupv2{ops: ops, replace: probeIn(replaceSupported)}

	return c.setDeviceRules(3, "/sys/fs/cgroup/test", rules)
}

func attachedRaw(t *testing.T, ops *fakeOps, name string) []byte {
	t.Helper()

	insts, _, err := ops.instructions(&fakeProg{name: name})
	if err != nil {
		t.Fatal(err)
	}

	return mustCanonical(t, insts)
}

func TestSet_ApplyTwiceKeepsLengthAndLineage(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	ops := ownedOps(t, map[string][]byte{"O1": orig})
	rules := []DeviceRule{rule("c", 10, 200, "rwm")}

	err := setRules(ops, rules)
	if err != nil {
		t.Fatal(err)
	}

	first := attachedRaw(t, ops, ops.attached[0])

	err = setRules(ops, rules)
	if err != nil {
		t.Fatal(err)
	}

	second := attachedRaw(t, ops, ops.attached[0])

	if len(second) != len(first) {
		t.Errorf(
			"program grew from %d to %d bytes on a second identical apply",
			len(first),
			len(second),
		)
	}

	firstParsed, _, _ := parseOwned(first)
	secondParsed, owned, err := parseOwned(second)

	if err != nil || !owned || secondParsed.nonce != firstParsed.nonce ||
		!bytes.Equal(secondParsed.original, orig) {
		t.Errorf(
			"second apply did not re-wrap the same original under the same nonce (err %v)",
			err,
		)
	}

	if len(ops.attached) != 1 {
		t.Errorf("attached = %q, want one program", ops.attached)
	}
}

func TestSet_WrapperIsNamedAndRestoreIsBare(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	ops := ownedOps(t, map[string][]byte{"O1": orig})

	err := setRules(ops, []DeviceRule{rule("c", 10, 200, "rwm")})
	if err != nil {
		t.Fatal(err)
	}

	if ops.names["N1"] != ownedProgramName {
		t.Errorf("wrapper name = %q, want %q", ops.names["N1"], ownedProgramName)
	}

	err = setRules(ops, nil)
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops, "load N1", "replace O1->N1", "load N2", "replace N1->N2")

	if !bytes.Equal(attachedRaw(t, ops, "N2"), orig) {
		t.Error("empty rules did not restore the bare original")
	}

	if ops.names["N2"] == ownedProgramName {
		t.Error("restored original must not carry the wrapper name")
	}

	ops.assertAllClosed(t)
}

func TestSet_EmptyRulesLeaveUnmarkedProgramsAlone(t *testing.T) {
	ops := ownedOps(t, map[string][]byte{"O1": mustCanonical(t, allowNullOriginal())})

	err := setRules(ops, nil)
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops)
	assertAttached(t, ops, "O1")
	ops.assertAllClosed(t)
}

func TestSet_EmptyRulesWithNothingAttachedIsANoOp(t *testing.T) {
	ops := newFakeOps()

	err := setRules(ops, nil)
	if err != nil {
		t.Fatalf("err = %v, want a no-op", err)
	}

	assertCalls(t, ops)
}

func TestSet_ConflictMutatesNothing(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	wrapped := mustEmit(t, []DeviceRule{rule("c", 10, 200, "rwm")}, 5, orig)

	insts, err := fromCanonical(wrapped)
	if err != nil {
		t.Fatal(err)
	}

	blockLen, _ := markerValue(insts[hdrBlockLen])
	broken := corrupt(t, wrapped, int(blockLen), asm.Mov.Imm32(asm.R0, 0))

	// O1 is fine; O2 conflicts. Nothing may happen to either.
	ops := ownedOps(t, map[string][]byte{"O1": orig, "O2": broken})

	for _, rules := range [][]DeviceRule{{rule("c", 10, 200, "rwm")}, nil} {
		err = setRules(ops, rules)
		if !errors.Is(err, ErrOwnedBlockConflict) {
			t.Fatalf("err = %v, want ErrOwnedBlockConflict", err)
		}
	}

	assertCalls(t, ops)
	assertAttached(t, ops, "O1", "O2")
	ops.assertAllClosed(t)
}

func TestSet_UnwrappableProgramMutatesNothing(t *testing.T) {
	ops := ownedOps(t, map[string][]byte{"O1": mustCanonical(t, allowNullOriginal())})
	ops.meta = progMeta{mapIDsKnown: true, mapCount: 1}

	err := setRules(ops, []DeviceRule{rule("c", 10, 200, "rwm")})
	if !errors.Is(err, ErrProgramNotWrappable) {
		t.Fatalf("err = %v, want ErrProgramNotWrappable", err)
	}

	assertCalls(t, ops)
	ops.assertAllClosed(t)
}

func TestSet_StripsOnlyValidatedBlocks(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	older := mustEmit(t, []DeviceRule{rule("c", 10, 200, "rwm")}, 7, orig)
	ops := ownedOps(t, map[string][]byte{"O1": older})

	err := setRules(ops, []DeviceRule{rule("b", 8, 0, "r")})
	if err != nil {
		t.Fatal(err)
	}

	parsed, owned, err := parseOwned(attachedRaw(t, ops, "N1"))
	if err != nil || !owned {
		t.Fatalf("owned=%t err=%v", owned, err)
	}

	if !bytes.Equal(parsed.original, orig) || parsed.nonce != 7 {
		t.Error("new wrapper does not wrap the stripped original under the old nonce")
	}

	if got := runDeviceFilter(
		t,
		attachedRaw(t, ops, "N1"),
		charReq(10, 200, unix.BPF_DEVCG_ACC_READ),
	); got != 0 {
		t.Error("grant from the replaced wrapper survived")
	}
}
