//go:build goldencapture

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

// Package goldencapture writes the golden files the Rust port is checked against, from the Go
// code that is the behavioral authority until the cutover (docs/rust-port.md, "Go golden
// captures"). It is built only with the goldencapture tag, so neither the daemon nor a normal
// go test run ever compiles it.
//
// With SDA_GOLDEN_UPDATE=1 a capture test writes its files under tests/testdata; without it the
// test compares what Go produces now against the committed files and fails on any difference, so
// a Go change that would move a golden is caught by `make golden-check`.
package goldencapture

import (
	"bytes"
	"encoding/json"
	"os"
	"path/filepath"
	"testing"
)

// UpdateEnv is the environment variable that turns a check into a rewrite.
const UpdateEnv = "SDA_GOLDEN_UPDATE"

// Modes of what a capture creates: directories and files only the owner writes.
const (
	dirMode  = 0o750
	fileMode = 0o600
)

// Root returns the repository root: the nearest directory above the working directory that holds
// go.mod. A go test runs in its package's directory.
func Root(t *testing.T) string {
	t.Helper()

	dir, err := os.Getwd()
	if err != nil {
		t.Fatalf("working directory: %v", err)
	}

	for {
		_, statErr := os.Stat(filepath.Join(dir, "go.mod"))
		if statErr == nil {
			return dir
		}

		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("no go.mod above the working directory")
		}

		dir = parent
	}
}

// Check writes content to the slash-separated path relative to the repository root when
// UpdateEnv is 1, and otherwise fails the test unless the committed file holds exactly content.
func Check(t *testing.T, rel string, content []byte) {
	t.Helper()

	path := filepath.Join(Root(t), filepath.FromSlash(rel))

	if os.Getenv(UpdateEnv) == "1" {
		err := os.MkdirAll(filepath.Dir(path), dirMode)
		if err != nil {
			t.Fatalf("create the directory of %s: %v", rel, err)
		}

		err = os.WriteFile(path, content, fileMode)
		if err != nil {
			t.Fatalf("write %s: %v", rel, err)
		}

		return
	}

	committed, err := os.ReadFile(path)
	if err != nil {
		t.Fatalf("read %s: %v (run with %s=1 to capture it)", rel, err, UpdateEnv)
	}

	if !bytes.Equal(committed, content) {
		t.Errorf("%s differs from what Go produces now; review the change and rerun with %s=1 "+
			"to recapture it\n--- committed\n%s\n--- now\n%s", rel, UpdateEnv, committed, content)
	}
}

// JSON encodes value as indented JSON with a final newline, HTML characters left as they are, so
// a golden reads the way its strings are written.
func JSON(t *testing.T, value any) []byte {
	t.Helper()

	var buffer bytes.Buffer

	encoder := json.NewEncoder(&buffer)
	encoder.SetEscapeHTML(false)
	encoder.SetIndent("", "  ")

	err := encoder.Encode(value)
	if err != nil {
		t.Fatalf("encode the golden: %v", err)
	}

	return buffer.Bytes()
}
