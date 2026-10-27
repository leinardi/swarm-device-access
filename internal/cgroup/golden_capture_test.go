//go:build linux && goldencapture

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
	"encoding/hex"
	"fmt"
	"math"
	"os"
	"path/filepath"
	"testing"

	"github.com/cilium/ebpf/asm"
	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/goldencapture"
)

// devfilterDir is where the device filter goldens and the fixture copies live.
const devfilterDir = "tests/testdata/devfilter"

// devfilterGolden is golden.json: everything the Rust BPF code must reproduce byte for byte.
type devfilterGolden struct {
	Fixtures []fixtureGolden `json:"fixtures"`
	Gate     []gateGolden    `json:"gate"`
	Builder  []builderGolden `json:"builder"`
}

type fixtureGolden struct {
	Name string `json:"name"`
	// Canonical is the fixture decoded and encoded again: the canonical bytes of .orig.
	Canonical    []string        `json:"canonical"`
	Gate         *string         `json:"gate"`
	OrigParse    parseGolden     `json:"origParse"`
	WrappedParse parseGolden     `json:"wrappedParse"`
	Wrappers     []wrapperGolden `json:"wrappers"`
	Conflicts    []parseCase     `json:"conflicts"`
}

type wrapperGolden struct {
	Case    string       `json:"case"`
	Rules   []DeviceRule `json:"rules"`
	Nonce   string       `json:"nonce"`
	Wrapper []string     `json:"wrapper"`
	Error   *string      `json:"error"`
	Gate    *string      `json:"gate"`
	Parse   *parseGolden `json:"parse"`
}

type parseGolden struct {
	Owned bool         `json:"owned"`
	Nonce string       `json:"nonce,omitempty"`
	Rules []DeviceRule `json:"rules,omitempty"`
	// OriginalIsFixture reports whether the stripped original equals the fixture's bytes.
	OriginalIsFixture bool    `json:"originalIsFixture,omitempty"`
	Error             *string `json:"error"`
}

type parseCase struct {
	Case    string      `json:"case"`
	Program []string    `json:"program"`
	Parse   parseGolden `json:"parse"`
}

type gateGolden struct {
	Case        string   `json:"case"`
	Program     []string `json:"program"`
	MapIDsKnown bool     `json:"mapIdsKnown"`
	MapCount    int      `json:"mapCount"`
	Verdict     *string  `json:"verdict"`
}

type builderGolden struct {
	Case string `json:"case"`
	// Appends are the rules in the order appendDevice is called, each with its error.
	Appends     []appendGolden `json:"appends"`
	HasWildCard bool           `json:"hasWildCard"`
	Original    []string       `json:"original"`
	Program     []string       `json:"program"`
	Error       *string        `json:"error"`
	// AfterFinalize is the error of one more appendDevice after finalize.
	AfterFinalize *string `json:"afterFinalize"`
}

type appendGolden struct {
	Rule  DeviceRule `json:"rule"`
	Error *string    `json:"error"`
}

// TestGoldenDevfilter writes tests/testdata/devfilter: copies of the runc, crun and systemd
// fixtures, and golden.json with, for each fixture, its canonical bytes, its gate verdict, the
// parses of the original and of the kernel's dump of the wrapper, the wrapper emitted for each
// rule set and nonce of the corpus with its gate verdict and parse, and the parses of corrupted
// wrappers; then the gate verdicts of the subset cases of owned_test.go and the program builder
// output for the cases of ebpf_test.go.
func TestGoldenDevfilter(t *testing.T) {
	golden := devfilterGolden{}

	for _, name := range runtimeFixtures {
		for _, suffix := range []string{".orig", ".reloaded", ".wrapped"} {
			raw, err := os.ReadFile(filepath.Join("testdata", "devfilter", name+suffix))
			if err != nil {
				t.Fatal(err)
			}

			goldencapture.Check(t, devfilterDir+"/"+name+suffix, raw)
		}

		golden.Fixtures = append(golden.Fixtures, captureFixture(t, name))
	}

	golden.Gate = captureGate(t)
	golden.Builder = captureBuilder(t)

	goldencapture.Check(t, devfilterDir+"/golden.json", goldencapture.JSON(t, golden))
}

func captureFixture(t *testing.T, name string) fixtureGolden {
	t.Helper()

	base := filepath.Join("testdata", "devfilter", name)
	orig := readFixture(t, base+".orig")
	wrapped := readFixture(t, base+".wrapped")

	insts, err := fromCanonical(orig)
	if err != nil {
		t.Fatal(err)
	}

	result := fixtureGolden{
		Name:         name,
		Canonical:    hexLines(mustCanonical(t, insts)),
		Gate:         errText(checkWrappable(insts, knownMeta())),
		OrigParse:    parseResult(orig, orig),
		WrappedParse: parseResult(wrapped, orig),
	}

	for _, wrapper := range wrapperCorpus() {
		result.Wrappers = append(result.Wrappers, captureWrapper(t, wrapper, orig))
	}

	first, err := emitOwned(fixtureRules(), fixtureNonce, orig)
	if err != nil {
		t.Fatal(err)
	}

	for _, conflict := range conflictCorpus(t, first) {
		result.Conflicts = append(result.Conflicts, parseCase{
			Case:    conflict.name,
			Program: hexLines(conflict.raw),
			Parse:   parseResult(conflict.raw, orig),
		})
	}

	return result
}

type wrapperCase struct {
	name  string
	rules []DeviceRule
	nonce uint64
}

func ruleOf(allow bool, typ string, major, minor int64, access string) DeviceRule {
	return DeviceRule{Allow: allow, Type: typ, Major: &major, Minor: &minor, Access: access}
}

// wrapperCorpus is the rule sets and nonces each fixture is wrapped with: the fixture's own, an
// empty set, access subsets and a deny, wildcards (and rules a wildcard hides), the largest
// numbers (which do not regenerate), and the rules emitOwned refuses.
func wrapperCorpus() []wrapperCase {
	overflow := int64(math.MaxUint32) + 1

	return []wrapperCase{
		{"fixture rules", fixtureRules(), fixtureNonce},
		{"no rules", nil, 0},
		{"one read rule", []DeviceRule{rule("c", 1, 3, "r")}, 1},
		{"subsets and a deny", []DeviceRule{
			rule("c", 10, 200, "rw"), rule("b", 8, 0, "rwm"), rule("c", 189, 3, "m"),
			ruleOf(false, "c", 10, 201, "w"), rule("b", 259, 7, "wm"),
		}, 42},
		{"wildcards", []DeviceRule{
			ruleOf(false, "a", -1, -1, "rwm"), rule("c", 7, -1, "rwm"), rule("b", -1, -1, "r"),
			rule("c", -1, 5, "rw"),
		}, 0xffffffffffffffff},
		{"rules before a wildcard are dropped", []DeviceRule{
			rule("c", 1, 3, "rwm"), rule("a", -1, -1, "rwm"),
		}, 7},
		{
			"largest numbers",
			[]DeviceRule{rule("c", math.MaxUint32, math.MaxUint32, "rwm")},
			1 << 32,
		},
		{"major overflow", []DeviceRule{rule("c", overflow, 0, "rwm")}, 1},
		{"minor overflow", []DeviceRule{rule("c", 1, overflow, "rwm")}, 1},
		{"invalid type", []DeviceRule{rule("x", 1, 0, "rwm")}, 1},
		{"unknown access", []DeviceRule{rule("c", 1, 0, "rwx")}, 1},
		{
			"no major",
			[]DeviceRule{{Allow: true, Type: "c", Minor: new(int64(0)), Access: "rwm"}},
			1,
		},
		{
			"no minor",
			[]DeviceRule{{Allow: true, Type: "c", Major: new(int64(1)), Access: "rwm"}},
			1,
		},
	}
}

func captureWrapper(t *testing.T, wrapper wrapperCase, orig []byte) wrapperGolden {
	t.Helper()

	result := wrapperGolden{
		Case:  wrapper.name,
		Rules: wrapper.rules,
		Nonce: fmt.Sprintf("%016x", wrapper.nonce),
	}

	raw, err := emitOwned(wrapper.rules, wrapper.nonce, orig)
	if err != nil {
		result.Error = errText(err)

		return result
	}

	insts, err := fromCanonical(raw)
	if err != nil {
		t.Fatal(err)
	}

	parsed := parseResult(raw, orig)
	result.Wrapper = hexLines(raw)
	result.Gate = errText(checkWrappable(insts, knownMeta()))
	result.Parse = &parsed

	return result
}

type namedProgram struct {
	name string
	raw  []byte
}

// conflictCorpus is the corruptions of TestParseOwned_ConflictsNeverValidate applied to a wrapper
// around a fixture, and a truncated header.
func conflictCorpus(t *testing.T, wrapped []byte) []namedProgram {
	t.Helper()

	insts, err := fromCanonical(wrapped)
	if err != nil {
		t.Fatal(err)
	}

	blockLen, _ := markerValue(insts[hdrBlockLen])
	trailerAt := int(blockLen)
	firstBlock := ownedHeaderLen + ownedInitLen

	return []namedProgram{
		{"header without trailer", corrupt(t, wrapped, trailerAt, asm.Mov.Imm32(asm.R0, 0))},
		{"trailer nonce differs", corrupt(t, wrapped, trailerAt+2, markerInst(98))},
		{"wrong block_len", corrupt(t, wrapped, hdrBlockLen, markerInst(blockLen+1))},
		{
			"block_len past the end",
			corrupt(t, wrapped, hdrBlockLen, markerInst(uint32(len(insts)))),
		},
		{
			"block_len below the minimum",
			corrupt(t, wrapped, hdrBlockLen, markerInst(ownedHeaderLen)),
		},
		{"wrong version", corrupt(t, wrapped, hdrVersion, markerInst(2))},
		{"wrong magic_hi", corrupt(t, wrapped, hdrMagicHi, markerInst(0))},
		{"digest mismatch", corrupt(t, wrapped, len(insts)-2, asm.Mov.Imm32(asm.R0, 1))},
		{"header field not marker", corrupt(t, wrapped, hdrNonceHi, asm.Mov.Imm32(asm.R1, 0))},
		{"non-regenerable block", corrupt(t, wrapped, firstBlock, asm.Instruction{
			OpCode:   asm.JNE.Op(asm.ImmSource),
			Dst:      asm.R2,
			Offset:   1,
			Constant: int64(unix.BPF_DEVCG_DEV_CHAR),
		})},
		{"unknown device type", corrupt(t, wrapped, firstBlock, asm.Instruction{
			OpCode:   asm.JNE.Op(asm.ImmSource),
			Dst:      asm.R2,
			Offset:   4,
			Constant: 3,
		})},
		{"unparseable block", corrupt(t, wrapped, firstBlock, asm.Mov.Imm32(asm.R7, 0))},
		{"truncated header", mustCanonical(
			t,
			asm.Instructions{markerInst(ownedMagicLo), asm.Mov.Imm32(asm.R0, 0), asm.Return()},
		)},
	}
}

func parseResult(raw, orig []byte) parseGolden {
	parsed, owned, err := parseOwned(raw)

	result := parseGolden{Owned: owned, Error: errText(err)}
	if parsed != nil {
		result.Nonce = fmt.Sprintf("%016x", parsed.nonce)
		result.Rules = parsed.rules
		result.OriginalIsFixture = bytes.Equal(parsed.original, orig)
	}

	return result
}

// captureGate is TestCheckWrappable_Subset's programs with the gate's exact verdicts.
func captureGate(t *testing.T) []gateGolden {
	t.Helper()

	ok := allowNullOriginal()
	withTail := func(insts ...asm.Instruction) asm.Instructions {
		return append(append(asm.Instructions{asm.Mov.Imm32(asm.R0, 0)}, insts...), asm.Return())
	}

	cases := []struct {
		name  string
		insts asm.Instructions
		meta  progMeta
	}{
		{"minimal original", ok, knownMeta()},
		{"map IDs unknown", ok, progMeta{}},
		{"uses a map", ok, progMeta{mapIDsKnown: true, mapCount: 1}},
		{"empty", nil, knownMeta()},
		{"helper call", withTail(asm.FnGetCurrentPidTgid.Call()), knownMeta()},
		{"64-bit immediate", withTail(asm.LoadImm(asm.R3, 1<<40, asm.DWord)), knownMeta()},
		{"stack store", withTail(asm.StoreMem(asm.RFP, -8, asm.R2, asm.Word)), knownMeta()},
		{"load from R2", withTail(asm.LoadMem(asm.R3, asm.R2, 0, asm.Word)), knownMeta()},
		{"ctx load at 12", withTail(asm.LoadMem(asm.R3, asm.R1, 12, asm.Word)), knownMeta()},
		{"alu add", withTail(asm.Add.Imm(asm.R3, 1)), knownMeta()},
		{"sign-extending move", withTail(asm.Instruction{
			OpCode: asm.Mov.Op(asm.RegSource), Dst: asm.R3, Src: asm.R2, Offset: 8,
		}), knownMeta()},
		{
			"jump out of program",
			withTail(asm.Instruction{OpCode: asm.Ja.Op(asm.ImmSource), Offset: 5}),
			knownMeta(),
		},
		{"jump before start", withTail(asm.Instruction{
			OpCode: asm.JEq.Op(asm.ImmSource), Dst: asm.R2, Offset: -5,
		}), knownMeta()},
	}

	out := make([]gateGolden, 0, len(cases))
	for _, tc := range cases {
		program := []string{}
		if len(tc.insts) > 0 {
			program = hexLines(mustCanonical(t, tc.insts))
		}

		out = append(out, gateGolden{
			Case:        tc.name,
			Program:     program,
			MapIDsKnown: tc.meta.mapIDsKnown,
			MapCount:    tc.meta.mapCount,
			Verdict:     errText(checkWrappable(tc.insts, tc.meta)),
		})
	}

	return out
}

// captureBuilder is ebpf_test.go's cases, run to the end: appendDevice in order, then finalize
// over the original TestPrependDeviceFilter_RoundTrip uses, the result encoded with its jumps
// resolved, and one more appendDevice after finalize.
func captureBuilder(t *testing.T) []builderGolden {
	t.Helper()

	overflow := int64(math.MaxUint32) + 1
	cases := []struct {
		name  string
		rules []DeviceRule
	}{
		{"char device rwm", []DeviceRule{rule("c", 195, 0, "rwm")}},
		{"block device read-only", []DeviceRule{rule("b", 8, 1, "r")}},
		{"wildcard sets the flag", []DeviceRule{rule("a", -1, -1, "rwm"), rule("c", 1, 0, "rwm")}},
		{"no major no minor", []DeviceRule{rule("c", -1, -1, "rwm")}},
		{"nil major", []DeviceRule{{Allow: true, Type: "c", Minor: new(int64(0)), Access: "rwm"}}},
		{"nil minor", []DeviceRule{{Allow: true, Type: "c", Major: new(int64(1)), Access: "rwm"}}},
		{"deny", []DeviceRule{ruleOf(false, "c", 1, 3, "rwm")}},
		{"invalid type", []DeviceRule{rule("x", 1, 0, "rwm")}},
		{"major overflow", []DeviceRule{rule("c", overflow, 0, "rwm")}},
		{"minor overflow", []DeviceRule{rule("c", 1, overflow, "rwm")}},
		{"unknown access", []DeviceRule{rule("c", 1, 0, "rx")}},
		{
			"round trip, appended last to first",
			[]DeviceRule{rule("b", 8, 0, "rwm"), rule("c", 195, 0, "rwm")},
		},
		{"every access subset", []DeviceRule{
			rule("c", 1, 1, "r"), rule("c", 1, 2, "w"), rule("c", 1, 3, "m"), rule("c", 1, 4, "rw"),
			rule("c", 1, 5, "rm"), rule("c", 1, 6, "wm"), rule("c", 1, 7, ""),
		}},
		{"largest numbers", []DeviceRule{rule("c", math.MaxUint32, math.MaxUint32, "rwm")}},
	}

	original := asm.Instructions{asm.Mov.Imm32(asm.R0, 0), asm.Return()}

	out := make([]builderGolden, 0, len(cases))
	for _, tc := range cases {
		out = append(out, buildCase(t, tc.name, tc.rules, original))
	}

	_, err := PrependDeviceFilter([]DeviceRule{rule("c", 1, 3, "rwm")}, nil)
	out = append(out, builderGolden{
		Case:     "no original",
		Appends:  []appendGolden{},
		Original: []string{},
		Program:  []string{},
		Error:    errText(err),
	})

	return out
}

func buildCase(
	t *testing.T,
	name string,
	rules []DeviceRule,
	original asm.Instructions,
) builderGolden {
	t.Helper()

	result := builderGolden{
		Case:     name,
		Original: hexLines(mustCanonical(t, original)),
		Program:  []string{},
	}

	built := &program{}
	built.init()

	for _, dev := range rules {
		err := built.appendDevice(dev, "pfx")
		result.Appends = append(result.Appends, appendGolden{Rule: dev, Error: errText(err)})

		if err != nil && result.Error == nil {
			result.Error = errText(err)
		}
	}

	result.HasWildCard = built.hasWildCard

	if result.Error != nil {
		return result
	}

	insts, err := built.finalize(append(asm.Instructions{}, original...), "pfx")
	if err != nil {
		t.Fatal(err)
	}

	var buf bytes.Buffer

	err = insts.Marshal(&buf, hostByteOrder)
	if err != nil {
		t.Fatal(err)
	}

	result.Program = hexLines(buf.Bytes())
	result.AfterFinalize = errText(built.appendDevice(rule("c", 1, 0, "rwm"), "pfx"))

	return result
}

// hexLines is one 8-byte instruction per string, as the fixtures hold them.
func hexLines(raw []byte) []string {
	lines := make([]string, 0, len(raw)/8)
	for off := 0; off < len(raw); off += 8 {
		lines = append(lines, hex.EncodeToString(raw[off:min(off+8, len(raw))]))
	}

	return lines
}

func errText(err error) *string {
	if err == nil {
		return nil
	}

	text := err.Error()

	return &text
}
