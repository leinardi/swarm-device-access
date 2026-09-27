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

package config

import (
	"os"
	"path/filepath"
	"reflect"
	"strings"
	"testing"
)

func writeConfig(t *testing.T, content string) string {
	t.Helper()

	path := filepath.Join(t.TempDir(), "config.yaml")

	err := os.WriteFile(path, []byte(content), 0o600)
	if err != nil {
		t.Fatal(err)
	}

	return path
}

func TestLoadFile_ValidFile(t *testing.T) {
	t.Parallel()

	path := writeConfig(t, `
log-format: json
log-level: debug
log-time: true
docker-socket: /run/docker.sock
dry-run: false
policy-mode: all
device-allow:
  - /dev/nvidia*
  - /dev/dri/*
device-deny: []
metrics-addr: ""
debug-addr: "127.0.0.1:6060"
`)

	cfg, err := LoadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	yes, no := true, false
	want := FileSchema{
		LogFormat:    "json",
		LogLevel:     "debug",
		LogTime:      &yes,
		DockerSocket: "/run/docker.sock",
		DryRun:       &no,
		PolicyMode:   "all",
		DeviceAllow:  []string{"/dev/nvidia*", "/dev/dri/*"},
		DeviceDeny:   []string{},
		DebugAddr:    "127.0.0.1:6060",
	}

	if !reflect.DeepEqual(cfg, want) {
		t.Errorf("cfg = %+v, want %+v", cfg, want)
	}
}

func TestLoadFile_EmptyInputs(t *testing.T) {
	t.Parallel()

	cfg, err := LoadFile("")
	if err != nil || !reflect.DeepEqual(cfg, FileSchema{}) {
		t.Errorf("empty path: cfg = %+v, err = %v; want the zero config", cfg, err)
	}

	for name, content := range map[string]string{
		"empty file":       "",
		"only comments":    "# nothing set\n",
		"bare doc marker":  "---\n",
		"shipped template": "# log-level: info\n# device-deny: []\n",
	} {
		cfg, err = LoadFile(writeConfig(t, content))
		if err != nil || !reflect.DeepEqual(cfg, FileSchema{}) {
			t.Errorf("%s: cfg = %+v, err = %v; want the zero config", name, cfg, err)
		}
	}
}

func TestLoadFile_ShippedExample(t *testing.T) {
	t.Parallel()

	_, err := LoadFile(filepath.Join("..", "..", "deployments", "docker", "config.yaml"))
	if err != nil {
		t.Fatalf("the shipped example must load: %v", err)
	}
}

func TestLoadFile_UnreadablePath(t *testing.T) {
	t.Parallel()

	_, err := LoadFile(filepath.Join(t.TempDir(), "missing.yaml"))
	if err == nil || !strings.Contains(err.Error(), "read config file") {
		t.Fatalf("err = %v, want a read error", err)
	}
}

// TestLoadFile_Rejects covers every file the loader must refuse, with a
// fragment the error must contain.
func TestLoadFile_Rejects(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name, content, want string
	}{
		{"unknown key", "device_deny:\n  - /dev/sda\n", "field device_deny not found"},
		{"second document", "dry-run: true\n---\npolicy-mode: all\n", "multiple YAML documents"},
		{"empty second document", "dry-run: true\n---\n", "multiple YAML documents"},
		{"document after an empty one", "---\n---\ndry-run: true\n", "multiple YAML documents"},
		{"malformed trailing content", "dry-run: true\n---\n: : [\n", "trailing content"},
		{"duplicate key", "policy-mode: all\npolicy-mode: opt-in\n", "already defined"},
		{"root not a mapping", "- dry-run\n", "must be a mapping"},
		{"explicit null document", "~\n", "must be a mapping"},

		{"dry-run as a string", "dry-run: \"yes\"\n", `key "dry-run" must be a boolean`},
		{"dry-run null", "dry-run: null\n", `key "dry-run" must be a boolean`},
		{"policy-mode null", "policy-mode: ~\n", `key "policy-mode" must be a non-empty string`},
		{"policy-mode empty", "policy-mode: \"\"\n", `key "policy-mode" must be a non-empty string`},
		{"device-allow null", "device-allow:\n", `key "device-allow" must be a list`},
		{"device-deny null", "device-deny: null\n", `key "device-deny" must be a list`},
		{"device-allow null element", "device-allow: [null]\n", `key "device-allow" must be a list`},
		{"device-deny empty element", "device-deny: [\"\"]\n", `key "device-deny" must be a list`},
		{"device-deny scalar", "device-deny: /dev/sda\n", `key "device-deny" must be a list`},
		{"log-time null", "log-time: null\n", `key "log-time" must be a boolean`},
		{"metrics-addr number", "metrics-addr: 9090\n", `key "metrics-addr" must be a string`},

		{"null behind an alias", "x: &n null\ndevice-deny: *n\n", `key "device-deny" must be a list`},
		{"null element behind an alias", "x: &n null\ndevice-allow: [*n]\n", `key "device-allow" must be a list`},
		{"inline merge", "<<: {device-deny: null}\n", "YAML merge keys are not supported"},
		{"aliased merge", "base: &b {dry-run: false}\n<<: *b\n", "YAML merge keys are not supported"},

		{"unknown log-format", "log-format: xml\n", `key "log-format": unknown value "xml"`},
		{"retired log-level alias", "log-level: warning\n", `key "log-level": unknown value "warning"`},
		{"unknown policy-mode", "policy-mode: everything\n", `key "policy-mode"`},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			path := writeConfig(t, tc.content)

			_, err := LoadFile(path)
			if err == nil {
				t.Fatalf("LoadFile accepted %q", tc.content)
			}

			if !strings.Contains(err.Error(), tc.want) || !strings.Contains(err.Error(), path) {
				t.Errorf("err = %q, want it to name the file and contain %q", err, tc.want)
			}
		})
	}
}

// TestLoadFile_LogTimeAbsentVsNull documents the chosen behavior: an
// absent log-time leaves the flag's value in force, an explicit null is an
// error like any other key (not a silent "unset").
func TestLoadFile_LogTimeAbsentVsNull(t *testing.T) {
	t.Parallel()

	cfg, err := LoadFile(writeConfig(t, "log-level: info\n"))
	if err != nil || cfg.LogTime != nil {
		t.Errorf("absent: LogTime = %v, err = %v; want not set", cfg.LogTime, err)
	}

	_, err = LoadFile(writeConfig(t, "log-time: null\n"))
	if err == nil {
		t.Error("explicit null: want an error")
	}
}

func TestValidateEnums(t *testing.T) {
	t.Parallel()

	err := ValidateEnums("plain", "warn", "opt-in", "flag")
	if err != nil {
		t.Errorf("valid values: %v", err)
	}

	err = ValidateEnums("", "", "", "flag")
	if err != nil {
		t.Errorf("unset values: %v", err)
	}

	for _, alias := range []string{"warning", "fatal", "panic"} {
		err = ValidateEnums("", alias, "", "flag")
		if err == nil || !strings.Contains(err.Error(), `flag "log-level"`) {
			t.Errorf("log-level %q: err = %v, want it rejected naming the flag", alias, err)
		}
	}
}
