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
	"fmt"
	"os"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"syscall"
	"testing"
)

// devicesModel stands in for the kernel behind a cgroup v1 directory made
// of t.TempDir() files. Reads and writes go through the real openat code
// (openatFiles) on the directory descriptor; after every successful write
// the model applies cgroup v1 exception semantics (allow ORs access into the
// exact entry, deny clears it, wildcard entries are never touched by an
// exact write) and rewrites devices.list.
type devicesModel struct {
	t       *testing.T
	dir     string
	entries []listEntry

	// fail maps "allow <line>", "deny <line>" or "read" to an injected error.
	fail map[string]error
	// ignoreWrites makes the kernel accept writes without effect.
	ignoreWrites bool
	writes       []string
}

func newDevicesModel(t *testing.T, list string) *devicesModel {
	t.Helper()

	entries, err := parseDevicesList([]byte(list))
	if err != nil {
		t.Fatal(err)
	}

	model := &devicesModel{t: t, dir: t.TempDir(), entries: entries, fail: map[string]error{}}

	for _, name := range []string{devicesAllow, devicesDeny} {
		err = os.WriteFile(filepath.Join(model.dir, name), nil, 0o600)
		if err != nil {
			t.Fatal(err)
		}
	}

	model.sync()

	return model
}

func formatEntry(entry listEntry) string {
	num := func(n int64) string {
		if n == wildcardNum {
			return "*"
		}

		return strconv.FormatInt(n, 10)
	}

	return fmt.Sprintf(
		"%c %s:%s %s",
		entry.typ,
		num(entry.major),
		num(entry.minor),
		accessString(entry.access),
	)
}

func (m *devicesModel) sync() {
	m.t.Helper()

	lines := make([]string, 0, len(m.entries))
	for _, entry := range m.entries {
		lines = append(lines, formatEntry(entry)+"\n")
	}

	err := os.WriteFile(filepath.Join(m.dir, devicesList), []byte(strings.Join(lines, "")), 0o600)
	if err != nil {
		m.t.Fatal(err)
	}
}

func (m *devicesModel) list() string {
	lines := make([]string, 0, len(m.entries))
	for _, entry := range m.entries {
		lines = append(lines, formatEntry(entry))
	}

	return strings.Join(lines, ", ")
}

// apply mutates the exact entry for line, as the kernel does.
func (m *devicesModel) apply(allow bool, line string) {
	m.t.Helper()

	parsed, err := parseDevicesList([]byte(line))
	if err != nil || len(parsed) != 1 {
		m.t.Fatalf("bad rule line %q: %v", line, err)
	}

	rule := parsed[0]

	idx := slices.IndexFunc(m.entries, func(e listEntry) bool {
		return e.typ == rule.typ && e.major == rule.major && e.minor == rule.minor
	})

	switch {
	case allow && idx < 0:
		m.entries = append(m.entries, rule)
	case allow:
		m.entries[idx].access |= rule.access
	case idx >= 0:
		m.entries[idx].access &^= rule.access
		if m.entries[idx].access == 0 {
			m.entries = slices.Delete(m.entries, idx, idx+1)
		}
	}

	m.sync()
}

func (m *devicesModel) read(dirFD int, name string) ([]byte, error) {
	err := m.fail["read"]
	if err != nil {
		return nil, err
	}

	return openatFiles{}.read(dirFD, name)
}

func (m *devicesModel) write(dirFD int, name, line string) error {
	verb := map[string]string{devicesAllow: "allow", devicesDeny: "deny"}[name]
	call := verb + " " + line
	m.writes = append(m.writes, call)

	failErr := m.fail[call]
	if failErr != nil {
		return failErr
	}

	err := openatFiles{}.write(dirFD, name, line)
	if err != nil {
		return err
	}

	if !m.ignoreWrites {
		m.apply(name == devicesAllow, line)
	}

	return nil
}

func (m *devicesModel) writesWith(verb string) []string {
	var out []string

	for _, call := range m.writes {
		if strings.HasPrefix(call, verb+" ") {
			out = append(out, call)
		}
	}

	return out
}

// setup opens the model's directory as a real cgroup handle.
func (m *devicesModel) setup(ledger *Ledger) (*cgroupv1, *CgroupHandle) {
	m.t.Helper()

	handle, err := OpenCgroup(m.dir)
	if err != nil {
		m.t.Fatal(err)
	}

	m.t.Cleanup(func() { _ = handle.Close() })

	return &cgroupv1{ledger: ledger, files: m}, handle
}

func rule(typ string, major, minor int64, access string) DeviceRule {
	return DeviceRule{Allow: true, Type: typ, Major: &major, Minor: &minor, Access: access}
}

func bit(typ byte, major, minor int64, access byte) devBit {
	return devBit{typ: typ, major: major, minor: minor, access: access}
}

func ownedLines(ledger *Ledger, id Identity) []string {
	owned := ledger.owned(id)
	out := make([]string, 0, len(owned))

	for _, one := range sortedBits(owned) {
		out = append(out, one.line())
	}

	return out
}

func TestCovered_Table(t *testing.T) {
	cases := []struct {
		list string
		bit  devBit
		want bool
	}{
		{"c 10:200 rw", bit('c', 10, 200, accRead), true},
		{"c 10:200 rw", bit('c', 10, 200, accMknod), false},
		{"c 10:200 rw", bit('c', 10, 201, accRead), false},
		{"c 10:200 rw", bit('b', 10, 200, accRead), false},
		{"a *:* rwm", bit('b', 8, 0, accWrite), true},
		{"a *:* m", bit('c', 1, 3, accRead), false},
		{"c 1:* m", bit('c', 1, 3, accMknod), true},
		{"c 1:* m", bit('c', 1, 3, accRead), false},
		{"c 1:* m", bit('c', 2, 3, accMknod), false},
		{"b 8:* r", bit('b', 8, 16, accRead), true},
		{"b 8:* r", bit('b', 8, 16, accWrite), false},
		{"c *:* m", bit('c', 99, 1, accMknod), true},
		{"c *:5 w", bit('c', 99, 5, accWrite), true},
	}

	for _, tc := range cases {
		entries, err := parseDevicesList([]byte(tc.list))
		if err != nil {
			t.Fatal(err)
		}

		if got := covered(entries, tc.bit); got != tc.want {
			t.Errorf("covered(%q, %s) = %t, want %t", tc.list, tc.bit.line(), got, tc.want)
		}
	}
}

func TestParseDevicesList_RejectsMalformed(t *testing.T) {
	for _, bad := range []string{"x 1:3 rwm", "c 1 rwm", "c a:3 rwm", "c 1:3 rwx", "c 1:3"} {
		_, err := parseDevicesList([]byte(bad))
		if !errors.Is(err, errListEntry) {
			t.Errorf("parse %q: err = %v, want errListEntry", bad, err)
		}
	}
}

// TestSet_AccessSubsetAndSuperset checks that only the bits the baseline
// lacks are written and owned: a superset request over a partial baseline
// owns the difference, a subset request owns nothing.
func TestSet_AccessSubsetAndSuperset(t *testing.T) {
	model := newDevicesModel(t, "c 1:* m\nc 10:200 r\n")
	ledger := &Ledger{}
	cg, handle := model.setup(ledger)

	err := cg.SetDeviceRules(
		handle,
		[]DeviceRule{rule("c", 10, 200, "rw"), rule("c", 1, 3, "rwm"), rule("c", 1, 5, "m")},
	)
	if err != nil {
		t.Fatal(err)
	}

	want := []string{"c 1:3 r", "c 1:3 w", "c 10:200 w"}
	if got := ownedLines(ledger, handle.Identity()); !slices.Equal(got, want) {
		t.Errorf("owned = %q, want %q", got, want)
	}

	if got := model.writesWith(
		"allow",
	); !slices.Equal(
		got,
		[]string{"allow c 1:3 r", "allow c 1:3 w", "allow c 10:200 w"},
	) {
		t.Errorf("allow writes = %q", got)
	}
}

func TestSet_GrantTwiceThenEmptyRevokesAndKeepsBaseline(t *testing.T) {
	model := newDevicesModel(t, "c 10:200 m\n")
	ledger := &Ledger{}
	cg, handle := model.setup(ledger)
	rules := []DeviceRule{rule("c", 10, 200, "rwm")}

	for range 2 {
		err := cg.SetDeviceRules(handle, rules)
		if err != nil {
			t.Fatal(err)
		}
	}

	if got := model.list(); got != "c 10:200 rwm" {
		t.Fatalf("after grants list = %q", got)
	}

	if got := model.writesWith("allow"); len(got) != 2 {
		t.Errorf("allow writes = %q, want r and w once each", got)
	}

	err := cg.SetDeviceRules(handle, nil)
	if err != nil {
		t.Fatal(err)
	}

	if got := model.list(); got != "c 10:200 m" {
		t.Errorf("after Set(empty) list = %q, want baseline m only", got)
	}

	if got := ownedLines(ledger, handle.Identity()); len(got) != 0 {
		t.Errorf("owned after revoke = %q", got)
	}

	if got := model.writesWith(
		"deny",
	); !slices.Equal(
		got,
		[]string{"deny c 10:200 r", "deny c 10:200 w"},
	) {
		t.Errorf("deny writes = %q", got)
	}
}

func TestSet_RedundantGrantIsNotRecordedOrRevoked(t *testing.T) {
	model := newDevicesModel(t, "c 1:3 rwm\n")
	ledger := &Ledger{}
	cg, handle := model.setup(ledger)

	err := cg.SetDeviceRules(handle, []DeviceRule{rule("c", 1, 3, "rwm")})
	if err != nil {
		t.Fatal(err)
	}

	if got := ownedLines(ledger, handle.Identity()); len(got) != 0 {
		t.Errorf("owned = %q, want nothing (runtime already grants /dev/null)", got)
	}

	err = cg.SetDeviceRules(handle, nil)
	if err != nil {
		t.Fatal(err)
	}

	if len(model.writes) != 0 {
		t.Errorf("writes = %q, want none", model.writes)
	}

	if got := model.list(); got != "c 1:3 rwm" {
		t.Errorf("list = %q, baseline touched", got)
	}
}

// assertLedgerMatchesFiles checks that the ledger holds exactly the bits
// that are effective and not part of baselineList.
func assertLedgerMatchesFiles(
	t *testing.T,
	model *devicesModel,
	ledger *Ledger,
	id Identity,
	baselineList string,
) {
	t.Helper()

	baseline, err := parseDevicesList([]byte(baselineList))
	if err != nil {
		t.Fatal(err)
	}

	effective := make(map[devBit]struct{})

	for _, entry := range model.entries {
		for _, one := range []byte{accRead, accWrite, accMknod} {
			candidate := devBit{typ: entry.typ, major: entry.major, minor: entry.minor, access: one}
			if entry.access&one != 0 && !covered(baseline, candidate) {
				effective[candidate] = struct{}{}
			}
		}
	}

	effectiveOwned := make([]string, 0, len(effective))
	for _, owned := range sortedBits(effective) {
		effectiveOwned = append(effectiveOwned, owned.line())
	}

	if got := ownedLines(ledger, id); !slices.Equal(got, effectiveOwned) {
		t.Errorf(
			"ledger %q does not match effective non-baseline bits %q (list %q)",
			got,
			effectiveOwned,
			model.list(),
		)
	}
}

// TestSet_InjectedFailureAtEachStepKeepsLedgerConsistent runs an A to B
// transition with every write, in turn, failing, and checks that the
// ledger still describes the files after the failed call.
func TestSet_InjectedFailureAtEachStepKeepsLedgerConsistent(t *testing.T) {
	const baselineList = "c 1:3 rwm\n"

	steps := []string{
		"deny c 10:200 r", "deny c 10:200 w", "deny c 10:200 m",
		"allow c 10:201 r", "allow c 10:201 w", "allow c 10:201 m",
	}

	for _, failing := range steps {
		t.Run(failing, func(t *testing.T) {
			model := newDevicesModel(t, baselineList)
			ledger := &Ledger{}
			cg, handle := model.setup(ledger)

			err := cg.SetDeviceRules(handle, []DeviceRule{rule("c", 10, 200, "rwm")})
			if err != nil {
				t.Fatal(err)
			}

			model.fail[failing] = errInjected

			err = cg.SetDeviceRules(handle, []DeviceRule{rule("c", 10, 201, "rwm")})
			if !errors.Is(err, errInjected) {
				t.Fatalf("err = %v, want injected", err)
			}

			assertLedgerMatchesFiles(t, model, ledger, handle.Identity(), baselineList)

			// A retry without the fault converges on B.
			delete(model.fail, failing)

			err = cg.SetDeviceRules(handle, []DeviceRule{rule("c", 10, 201, "rwm")})
			if err != nil {
				t.Fatalf("retry: %v", err)
			}

			if got := model.list(); got != "c 1:3 rwm, c 10:201 rwm" {
				t.Errorf("after retry list = %q", got)
			}

			assertLedgerMatchesFiles(t, model, ledger, handle.Identity(), baselineList)
		})
	}
}

func TestSet_DenyFailureSkipsEveryGrant(t *testing.T) {
	model := newDevicesModel(t, "")
	ledger := &Ledger{}
	cg, handle := model.setup(ledger)

	err := cg.SetDeviceRules(handle, []DeviceRule{rule("c", 10, 200, "rw")})
	if err != nil {
		t.Fatal(err)
	}

	model.writes = nil
	model.fail["deny c 10:200 w"] = errInjected

	err = cg.SetDeviceRules(handle, []DeviceRule{rule("c", 10, 201, "rwm")})
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v, want injected", err)
	}

	if got := model.writesWith("allow"); len(got) != 0 {
		t.Errorf("allow writes after a failed revoke = %q, want none", got)
	}

	if got := ownedLines(ledger, handle.Identity()); !slices.Equal(got, []string{"c 10:200 w"}) {
		t.Errorf("owned = %q, want the bit whose deny failed", got)
	}
}

func TestSet_RestoresExternallyRemovedOwnedBit(t *testing.T) {
	model := newDevicesModel(t, "")
	ledger := &Ledger{}
	cg, handle := model.setup(ledger)
	rules := []DeviceRule{rule("c", 10, 200, "rw")}

	err := cg.SetDeviceRules(handle, rules)
	if err != nil {
		t.Fatal(err)
	}

	model.apply(false, "c 10:200 r")
	model.writes = nil

	err = cg.SetDeviceRules(handle, rules)
	if err != nil {
		t.Fatal(err)
	}

	if !slices.Equal(model.writes, []string{"allow c 10:200 r"}) {
		t.Errorf("writes = %q, want the removed bit restored", model.writes)
	}

	if got := model.list(); got != "c 10:200 rw" {
		t.Errorf("list = %q", got)
	}
}

func TestSet_VerifyMismatchIsReported(t *testing.T) {
	model := newDevicesModel(t, "")
	model.ignoreWrites = true
	cg, handle := model.setup(&Ledger{})

	err := cg.SetDeviceRules(handle, []DeviceRule{rule("c", 10, 200, "r")})
	if !errors.Is(err, ErrDeviceRulesDrift) {
		t.Fatalf("err = %v, want ErrDeviceRulesDrift", err)
	}
}

func TestSet_RejectsRulesThatAreNotConcreteAllows(t *testing.T) {
	model := newDevicesModel(t, "")
	cg, handle := model.setup(&Ledger{})

	deny := rule("c", 10, 200, "r")
	deny.Allow = false

	for _, bad := range []DeviceRule{deny, rule("a", 10, 200, "r"), rule("c", -1, 200, "r"), rule("c", 10, 200, "x")} {
		err := cg.SetDeviceRules(handle, []DeviceRule{bad})
		if err == nil {
			t.Errorf("rule %+v accepted", bad)
		}
	}

	if len(model.writes) != 0 {
		t.Errorf("writes = %q, want none", model.writes)
	}
}

// TestSet_LedgerIsKeyedByInode models a cgroup removed and recreated at the
// same path: the new directory (other inode) owns nothing, so an empty Set
// on it revokes nothing, while the old identity keeps its record.
func TestSet_LedgerIsKeyedByInode(t *testing.T) {
	ledger := &Ledger{}

	oldModel := newDevicesModel(t, "")
	oldCg, oldHandle := oldModel.setup(ledger)

	err := oldCg.SetDeviceRules(oldHandle, []DeviceRule{rule("c", 10, 200, "r")})
	if err != nil {
		t.Fatal(err)
	}

	newModel := newDevicesModel(t, "c 10:200 r\n")
	newCg, newHandle := newModel.setup(ledger)
	newHandle.identity.Path = oldHandle.Identity().Path

	if newHandle.Identity() == oldHandle.Identity() {
		t.Fatal("test needs distinct inodes")
	}

	err = newCg.SetDeviceRules(newHandle, nil)
	if err != nil {
		t.Fatal(err)
	}

	if len(newModel.writes) != 0 {
		t.Errorf(
			"recreated cgroup got writes %q; it must start with nothing owned",
			newModel.writes,
		)
	}

	if got := ownedLines(ledger, oldHandle.Identity()); !slices.Equal(got, []string{"c 10:200 r"}) {
		t.Errorf("old identity owned = %q", got)
	}
}

func TestOpenCgroup_RecordsInode(t *testing.T) {
	dir := t.TempDir()

	handle, err := OpenCgroup(dir)
	if err != nil {
		t.Fatal(err)
	}
	defer handle.Close()

	info, err := os.Stat(dir)
	if err != nil {
		t.Fatal(err)
	}

	stat, ok := info.Sys().(*syscall.Stat_t)
	if !ok {
		t.Fatal("no stat_t")
	}

	if handle.Identity() != (Identity{Path: dir, Inode: stat.Ino}) {
		t.Errorf("identity = %+v, want path %q inode %d", handle.Identity(), dir, stat.Ino)
	}
}
