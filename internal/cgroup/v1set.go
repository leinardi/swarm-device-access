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
	"bufio"
	"bytes"
	"cmp"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"
	"strconv"
	"strings"

	"golang.org/x/sys/unix"
)

// Access bits of a cgroup v1 device exception.
const (
	accRead  byte = 1 << iota // r
	accWrite                  // w
	accMknod                  // m
)

const (
	devicesList  = "devices.list"
	devicesAllow = "devices.allow"
	devicesDeny  = "devices.deny"

	// wildcardNum stands for "*" in a major or minor number.
	wildcardNum = -1
)

var (
	// ErrDeviceRulesDrift reports that devices.list, re-read after a Set,
	// does not show exactly the daemon-owned bits the Set established (an
	// external writer, or a write the kernel accepted without effect). The
	// ledger already reflects every successful write, so a retry recomputes
	// from the file and converges.
	ErrDeviceRulesDrift = errors.New("cgroup v1 device exceptions differ from the daemon-owned set")

	errMissingLedger = errors.New("cgroup v1 requires a ledger")
	errDenyRule      = errors.New("cgroup v1 set accepts allow rules only")
	errConcreteRule  = errors.New("cgroup v1 set needs a c or b rule with concrete major and minor")
	errListEntry     = errors.New("malformed devices.list entry")
	errUnknownAccess = errors.New("unknown device access")
)

// devBit is one access bit of one device: the unit the v1 ledger owns.
type devBit struct {
	typ    byte // 'c' or 'b'
	major  int64
	minor  int64
	access byte // exactly one of accRead, accWrite, accMknod
}

func (b devBit) line() string {
	return fmt.Sprintf("%c %d:%d %s", b.typ, b.major, b.minor, accessString(b.access))
}

func compareBits(left, right devBit) int {
	return cmp.Or(
		cmp.Compare(left.typ, right.typ),
		cmp.Compare(left.major, right.major),
		cmp.Compare(left.minor, right.minor),
		cmp.Compare(left.access, right.access),
	)
}

// listEntry is one devices.list line; typ may be 'a' (all) and major or
// minor may be wildcardNum. access is a mask.
type listEntry struct {
	typ    byte
	major  int64
	minor  int64
	access byte
}

func (e listEntry) covers(bit devBit) bool {
	return (e.typ == 'a' || e.typ == bit.typ) &&
		(e.major == wildcardNum || e.major == bit.major) &&
		(e.minor == wildcardNum || e.minor == bit.minor) &&
		e.access&bit.access != 0
}

func covered(entries []listEntry, bit devBit) bool {
	return slices.ContainsFunc(entries, func(e listEntry) bool { return e.covers(bit) })
}

func accessString(mask byte) string {
	var out strings.Builder

	if mask&accRead != 0 {
		out.WriteByte('r')
	}

	if mask&accWrite != 0 {
		out.WriteByte('w')
	}

	if mask&accMknod != 0 {
		out.WriteByte('m')
	}

	return out.String()
}

func parseAccess(access string) (byte, error) {
	var mask byte

	for _, r := range access {
		switch r {
		case 'r':
			mask |= accRead
		case 'w':
			mask |= accWrite
		case 'm':
			mask |= accMknod
		default:
			return 0, fmt.Errorf("%w %q", errUnknownAccess, r)
		}
	}

	return mask, nil
}

func parseNum(field string) (int64, error) {
	if field == "*" {
		return wildcardNum, nil
	}

	num, err := strconv.ParseInt(field, 10, 64)
	if err != nil {
		return 0, fmt.Errorf("device number %q: %w", field, err)
	}

	return num, nil
}

// parseDevicesList parses devices.list ("c 1:3 rwm", "b 8:* r", "a *:* rwm").
func parseDevicesList(data []byte) ([]listEntry, error) {
	var entries []listEntry

	scanner := bufio.NewScanner(bytes.NewReader(data))
	for scanner.Scan() {
		line := strings.TrimSpace(scanner.Text())
		if line == "" {
			continue
		}

		fields := strings.Fields(line)
		if len(fields) != 3 || len(fields[0]) != 1 || !strings.Contains("abc", fields[0]) {
			return nil, fmt.Errorf("%w: %q", errListEntry, line)
		}

		numbers := strings.Split(fields[1], ":")
		if len(numbers) != 2 {
			return nil, fmt.Errorf("%w: %q", errListEntry, line)
		}

		major, err := parseNum(numbers[0])
		if err != nil {
			return nil, fmt.Errorf("%w: %q: %w", errListEntry, line, err)
		}

		minor, err := parseNum(numbers[1])
		if err != nil {
			return nil, fmt.Errorf("%w: %q: %w", errListEntry, line, err)
		}

		access, err := parseAccess(fields[2])
		if err != nil {
			return nil, fmt.Errorf("%w: %q: %w", errListEntry, line, err)
		}

		entries = append(
			entries,
			listEntry{typ: fields[0][0], major: major, minor: minor, access: access},
		)
	}

	err := scanner.Err()
	if err != nil {
		return nil, fmt.Errorf("read devices.list: %w", err)
	}

	return entries, nil
}

// withoutOwned returns entries with the owned bits removed from the exact
// (non-wildcard) entries they appear in. devices.list merges the runtime's
// access bits and ours into one line per device, so this is what separates
// the runtime's baseline from the daemon's grants. Wildcard entries are
// always baseline: the daemon never writes one.
func withoutOwned(entries []listEntry, owned map[devBit]struct{}) []listEntry {
	out := make([]listEntry, 0, len(entries))

	for _, entry := range entries {
		if entry.typ != 'a' && entry.major != wildcardNum && entry.minor != wildcardNum {
			for bit := range owned {
				if bit.typ == entry.typ && bit.major == entry.major && bit.minor == entry.minor {
					entry.access &^= bit.access
				}
			}
		}

		if entry.access != 0 {
			out = append(out, entry)
		}
	}

	return out
}

// expandRules turns allow rules into single-access-bit units.
func expandRules(rules []DeviceRule) (map[devBit]struct{}, error) {
	bits := make(map[devBit]struct{})

	for _, rule := range rules {
		if !rule.Allow {
			return nil, errDenyRule
		}

		if (rule.Type != "c" && rule.Type != "b") || rule.Major == nil || rule.Minor == nil ||
			*rule.Major < 0 || *rule.Minor < 0 {
			return nil, fmt.Errorf("%w: %+v", errConcreteRule, rule)
		}

		access, err := parseAccess(rule.Access)
		if err != nil {
			return nil, err
		}

		for _, one := range []byte{accRead, accWrite, accMknod} {
			if access&one != 0 {
				bits[devBit{typ: rule.Type[0], major: *rule.Major, minor: *rule.Minor, access: one}] = struct{}{}
			}
		}
	}

	return bits, nil
}

func sortedBits(set map[devBit]struct{}) []devBit {
	out := make([]devBit, 0, len(set))
	for bit := range set {
		out = append(out, bit)
	}

	slices.SortFunc(out, compareBits)

	return out
}

// v1files reads and writes the devices.* files relative to a cgroup
// directory descriptor. It is a seam for failure injection in tests.
type v1files interface {
	read(dirFD int, name string) ([]byte, error)
	write(dirFD int, name, line string) error
}

// openatFiles is the production v1files: every access is openat on the
// directory descriptor, never a path lookup.
type openatFiles struct{}

func (openatFiles) read(dirFD int, name string) ([]byte, error) {
	fd, err := unix.Openat(dirFD, name, unix.O_RDONLY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", name, err)
	}

	file := os.NewFile(uintptr(fd), name)
	defer file.Close()

	data, err := io.ReadAll(file)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", name, err)
	}

	return data, nil
}

func (openatFiles) write(dirFD int, name, line string) error {
	fd, err := unix.Openat(dirFD, name, unix.O_WRONLY|unix.O_CLOEXEC, 0)
	if err != nil {
		return fmt.Errorf("open %s: %w", name, err)
	}

	file := os.NewFile(uintptr(fd), name)
	defer file.Close()

	_, err = file.WriteString(line)
	if err != nil {
		return fmt.Errorf("write %q to %s: %w", line, name, err)
	}

	return nil
}

func (c *cgroupv1) readList(dirFD int) ([]listEntry, error) {
	data, err := c.files.read(dirFD, devicesList)
	if err != nil {
		return nil, err
	}

	return parseDevicesList(data)
}

// SetDeviceRules makes the daemon-owned device exceptions of the cgroup
// equal exactly rules.
//
// The runtime's own exceptions (the baseline) are what devices.list shows
// minus the bits the ledger says this daemon added. Only bits outside the
// baseline become owned, so a grant the runtime already made is never
// recorded and therefore never revoked. Revocations are written before
// grants and a failed revocation stops the call: going from A to B with a
// failed revoke of A must not end as A plus B. Every successful write
// updates the ledger at once, so a partial failure leaves it matching the
// files, and a retry recomputes from devices.list.
func (c *cgroupv1) SetDeviceRules(handle *CgroupHandle, rules []DeviceRule) error {
	desired, err := expandRules(rules)
	if err != nil {
		return err
	}

	id := handle.Identity()

	entries, err := c.readList(handle.fd)
	if err != nil {
		return err
	}

	ownedOld := c.ledger.owned(id)
	baseline := withoutOwned(entries, ownedOld)

	ownedNew := make(map[devBit]struct{})

	for bit := range desired {
		if !covered(baseline, bit) {
			ownedNew[bit] = struct{}{}
		}
	}

	// Owned bits that the baseline now covers by itself (the runtime granted
	// them since) are forgotten, not denied: denying them would take away
	// the runtime's grant.
	forgotten := make(map[devBit]struct{})

	for _, bit := range sortedBits(ownedOld) {
		if _, keep := ownedNew[bit]; keep {
			continue
		}

		if covered(baseline, bit) {
			c.ledger.remove(id, bit)
			forgotten[bit] = struct{}{}

			continue
		}

		err = c.files.write(handle.fd, devicesDeny, bit.line())
		if err != nil {
			return fmt.Errorf("revoke %q: %w", bit.line(), err)
		}

		c.ledger.remove(id, bit)
	}

	var errs []error

	for _, bit := range sortedBits(ownedNew) {
		// Written whenever the bit is not effective now, not merely when the
		// ledger lacks it: an external writer may have removed an owned bit,
		// and an identical Set must restore it.
		if !covered(entries, bit) {
			err = c.files.write(handle.fd, devicesAllow, bit.line())
			if err != nil {
				errs = append(errs, fmt.Errorf("grant %q: %w", bit.line(), err))

				continue
			}
		}

		c.ledger.add(id, bit)
	}

	errs = append(errs, c.verify(handle.fd, ownedOld, ownedNew, forgotten))

	return errors.Join(errs...)
}

// verify re-reads devices.list and checks that, among the bits the daemon
// owned before or owns now, exactly the owned-now bits are effective.
func (c *cgroupv1) verify(dirFD int, ownedOld, ownedNew, forgotten map[devBit]struct{}) error {
	after, err := c.readList(dirFD)
	if err != nil {
		return fmt.Errorf("verify: %w", err)
	}

	checked := make(map[devBit]struct{}, len(ownedOld)+len(ownedNew))

	for bit := range ownedOld {
		if _, gone := forgotten[bit]; !gone {
			checked[bit] = struct{}{}
		}
	}

	for bit := range ownedNew {
		checked[bit] = struct{}{}
	}

	var mismatched []string

	for _, bit := range sortedBits(checked) {
		_, want := ownedNew[bit]
		if covered(after, bit) != want {
			mismatched = append(mismatched, fmt.Sprintf("%s (want %t)", bit.line(), want))
		}
	}

	if len(mismatched) > 0 {
		return fmt.Errorf("%w: %s", ErrDeviceRulesDrift, strings.Join(mismatched, ", "))
	}

	return nil
}
