//go:build linux && fixturecapture

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
	"encoding/hex"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"golang.org/x/sys/unix"
)

// TestCaptureFixtures records the golden fixtures under testdata/devfilter.
// It needs CAP_SYS_ADMIN/CAP_BPF and is not part of the normal test run; see
// testdata/devfilter/README.md for how to run it. For every device filter
// attached to SDA_CAPTURE_CGROUP it writes:
//
//   - <name>.orig: the translated dump, as the daemon reads it;
//   - <name>.reloaded: the dump of that original loaded back unchanged;
//   - <name>.wrapped: the dump of the wrapper around it for fixtureRules
//     with fixtureNonce.
//
// Nothing is attached or detached: programs are only loaded and dumped.
func TestCaptureFixtures(t *testing.T) {
	cgroupPath := os.Getenv("SDA_CAPTURE_CGROUP")
	outDir := os.Getenv("SDA_CAPTURE_OUT")
	name := os.Getenv("SDA_CAPTURE_NAME")

	if cgroupPath == "" || outDir == "" || name == "" {
		t.Skip("set SDA_CAPTURE_CGROUP, SDA_CAPTURE_OUT and SDA_CAPTURE_NAME")
	}

	handle, err := OpenCgroup(cgroupPath)
	if err != nil {
		t.Fatal(err)
	}
	defer handle.Close()

	ops := kernelOps{}

	progs, total, inaccessible, flags, err := ops.query(handle.fd)
	if err != nil {
		t.Fatal(err)
	}
	defer closeAll(progs)

	var uname unix.Utsname

	_ = unix.Uname(&uname)
	t.Logf("kernel %s: %d programs (%d inaccessible), attach flags %s",
		unix.ByteSliceToString(uname.Release[:]), total, inaccessible, describeAttachFlags(flags))

	for idx, prog := range progs {
		info, infoErr := prog.(kernelProg).Info()
		if infoErr != nil {
			t.Fatal(infoErr)
		}

		insts, meta, instErr := ops.instructions(prog)
		if instErr != nil {
			t.Fatal(instErr)
		}

		raw, rawErr := canonicalBytes(insts)
		if rawErr != nil {
			t.Fatal(rawErr)
		}

		base := filepath.Join(outDir, fmt.Sprintf("%s-%d", name, idx))
		t.Logf(
			"program %d: name %q, %d instructions, maps known %t count %d, gate: %v",
			idx,
			info.Name,
			len(insts),
			meta.mapIDsKnown,
			meta.mapCount,
			checkWrappable(insts, meta),
		)
		t.Logf("%v", insts)

		writeFixture(t, base+".orig", raw)
		writeFixture(t, base+".reloaded", loadAndDump(t, raw, ""))

		wrapped, emitErr := emitOwned(fixtureRules(), fixtureNonce, raw)
		if emitErr != nil {
			t.Fatal(emitErr)
		}

		writeFixture(t, base+".wrapped", loadAndDump(t, wrapped, ownedProgramName))
	}
}

func loadAndDump(t *testing.T, raw []byte, name string) []byte {
	t.Helper()

	c := &cgroupv2{ops: kernelOps{}}

	handle, err := c.loadCanonical(raw, name)
	if err != nil {
		t.Fatal(err)
	}
	defer handle.Close()

	info, err := handle.(kernelProg).Info()
	if err != nil {
		t.Fatal(err)
	}

	insts, err := info.Instructions()
	if err != nil {
		t.Fatal(err)
	}

	dumped, err := canonicalBytes(insts)
	if err != nil {
		t.Fatal(err)
	}

	return dumped
}

func writeFixture(t *testing.T, path string, raw []byte) {
	t.Helper()

	err := os.WriteFile(path, []byte(encodeFixture(raw)), 0o644)
	if err != nil {
		t.Fatal(err)
	}
}

// encodeFixture writes one 8-byte instruction per line as hex.
func encodeFixture(raw []byte) string {
	var out strings.Builder

	for off := 0; off < len(raw); off += 8 {
		out.WriteString(hex.EncodeToString(raw[off:min(off+8, len(raw))]))
		out.WriteByte('\n')
	}

	return out.String()
}
