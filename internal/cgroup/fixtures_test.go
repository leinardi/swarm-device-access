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
	"encoding/hex"
	"os"
	"strings"
	"testing"
)

// fixtureNonce and fixtureRules are the wrapper parameters the captured
// .wrapped fixtures were produced with.
const fixtureNonce uint64 = 0x0123456789abcdef

func fixtureRules() []DeviceRule {
	return []DeviceRule{
		rule("c", 10, 200, "rwm"),
		rule("b", 8, 0, "r"),
	}
}

func readFixture(t *testing.T, path string) []byte {
	t.Helper()

	data, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	raw, err := hex.DecodeString(strings.Join(strings.Fields(string(data)), ""))
	if err != nil {
		t.Fatalf("%s: %v", path, err)
	}

	return raw
}
