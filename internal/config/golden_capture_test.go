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

package config

import (
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/internal/goldencapture"
)

// The config golden: LoadFile's accept or reject result, with the decoded schema or the exact
// error text, for the loader test corpus and further yaml.v3 edge cases, and ValidateEnums. Each
// input is written to a file named config.yaml in a fresh directory; the file's path in an error
// reads {path}.

// pathPlaceholder stands for the config file's path in an error text.
const pathPlaceholder = "{path}"

// goldenSchema is a FileSchema, with a nil list or bool kept apart from an empty or false one.
type goldenSchema struct {
	LogFormat    string   `json:"logFormat"`
	LogLevel     string   `json:"logLevel"`
	LogTime      *bool    `json:"logTime"`
	DockerSocket string   `json:"dockerSocket"`
	DryRun       *bool    `json:"dryRun"`
	PolicyMode   string   `json:"policyMode"`
	DeviceAllow  []string `json:"deviceAllow"`
	DeviceDeny   []string `json:"deviceDeny"`
	MetricsAddr  string   `json:"metricsAddr"`
	DebugAddr    string   `json:"debugAddr"`
}

// goldenLoad is one LoadFile call: the file's content, or for the last two entries no file at all
// (an empty path, and a path naming nothing).
type goldenLoad struct {
	Name   string        `json:"name"`
	Input  string        `json:"input"`
	Schema *goldenSchema `json:"schema"`
	Error  string        `json:"error"`
}

type goldenEnums struct {
	LogFormat  string `json:"logFormat"`
	LogLevel   string `json:"logLevel"`
	PolicyMode string `json:"policyMode"`
	Source     string `json:"source"`
	Error      string `json:"error"`
}

type goldenConfig struct {
	Load          []goldenLoad  `json:"load"`
	Shipped       goldenLoad    `json:"shippedExample"`
	ValidateEnums []goldenEnums `json:"validateEnums"`
}

// goldenInputs is the loader corpus: TestLoadFile_*'s inputs first, then the yaml.v3 edge cases.
func goldenInputs() [][2]string {
	return [][2]string{
		{"valid file", "\nlog-format: json\nlog-level: debug\nlog-time: true\n" +
			"docker-socket: /run/docker.sock\ndry-run: false\npolicy-mode: all\n" +
			"device-allow:\n  - /dev/nvidia*\n  - /dev/dri/*\ndevice-deny: []\n" +
			"metrics-addr: \"\"\ndebug-addr: \"127.0.0.1:6060\"\n"},
		{"empty file", ""},
		{"only comments", "# nothing set\n"},
		{"bare doc marker", "---\n"},
		{"shipped template", "# log-level: info\n# device-deny: []\n"},
		{"log-time absent", "log-level: info\n"},
		{"unknown key", "device_deny:\n  - /dev/sda\n"},
		{"second document", "dry-run: true\n---\npolicy-mode: all\n"},
		{"empty second document", "dry-run: true\n---\n"},
		{"document after an empty one", "---\n---\ndry-run: true\n"},
		{"malformed trailing content", "dry-run: true\n---\n: : [\n"},
		{"duplicate key", "policy-mode: all\npolicy-mode: opt-in\n"},
		{"root not a mapping", "- dry-run\n"},
		{"explicit null document", "~\n"},
		{"dry-run as a string", "dry-run: \"yes\"\n"},
		{"dry-run null", "dry-run: null\n"},
		{"policy-mode null", "policy-mode: ~\n"},
		{"policy-mode empty", "policy-mode: \"\"\n"},
		{"device-allow null", "device-allow:\n"},
		{"device-deny null", "device-deny: null\n"},
		{"device-allow null element", "device-allow: [null]\n"},
		{"device-deny empty element", "device-deny: [\"\"]\n"},
		{"device-deny scalar", "device-deny: /dev/sda\n"},
		{"log-time null", "log-time: null\n"},
		{"metrics-addr number", "metrics-addr: 9090\n"},
		{"null behind an alias", "x: &n null\ndevice-deny: *n\n"},
		{"null element behind an alias", "x: &n null\ndevice-allow: [*n]\n"},
		{"inline merge", "<<: {device-deny: null}\n"},
		{"aliased merge", "base: &b {dry-run: false}\n<<: *b\n"},
		{"unknown log-format", "log-format: xml\n"},
		{"retired log-level alias", "log-level: warning\n"},
		{"unknown policy-mode", "policy-mode: everything\n"},

		// Booleans: YAML 1.1 spellings, case, numbers.
		{"bool yes", "dry-run: yes\n"},
		{"bool no", "dry-run: no\n"},
		{"bool on", "dry-run: on\n"},
		{"bool off", "dry-run: off\n"},
		{"bool y", "dry-run: y\n"},
		{"bool True", "dry-run: True\n"},
		{"bool TRUE", "log-time: TRUE\n"},
		{"bool tRUE", "log-time: tRUE\n"},
		{"bool one", "log-time: 1\n"},
		{"bool zero", "dry-run: 0\n"},
		{"bool quoted true", "dry-run: \"true\"\n"},
		{"bool single quoted", "dry-run: 'false'\n"},
		// Null and empty values.
		{"null word", "null\n"},
		{"tilde value", "metrics-addr: ~\n"},
		{"Null value", "metrics-addr: Null\n"},
		{"NULL value", "debug-addr: NULL\n"},
		{"empty value", "metrics-addr:\n"},
		{"empty string addr", "metrics-addr: ''\n"},
		{"empty list", "device-allow: []\n"},
		{"empty block list", "device-deny:\n  []\n"},
		{"empty document end", "...\n"},
		{"start and end markers", "---\n...\n"},
		{"comments around marker", "# c\n---\n# c\n"},
		{"document end after content", "dry-run: true\n...\n"},
		{"document end then content", "dry-run: true\n...\n---\npolicy-mode: all\n"},
		{"explicit start", "--- \ndry-run: true\n"},
		// Anchors and aliases.
		{"anchor unused", "policy-mode: &m all\nlog-level: info\n"},
		{"list alias", "device-allow: &l [/dev/a]\ndevice-deny: *l\n"},
		{"scalar alias", "log-level: &v debug\nlog-format: *v\n"},
		{"element alias", "device-allow: [&e /dev/a, *e]\n"},
		{"alias to mapping", "x: &m {a: 1}\ndevice-deny: *m\n"},
		{"alias undefined", "device-deny: *missing\n"},
		{"self alias", "device-allow: &a [*a]\n"},
		{"merge in a list", "device-allow: [{<<: {a: b}}]\n"},
		{"quoted merge key", "\"<<\": x\n"},
		// Duplicate and unknown keys.
		{"duplicate list key", "device-allow: [/dev/a]\ndevice-allow: [/dev/b]\n"},
		{"duplicate through alias", "a: &k dry-run\n*k : true\ndry-run: false\n"},
		{"case-different key", "Dry-Run: true\n"},
		{"underscore key", "dry_run: true\n"},
		{"numeric key", "1: x\n"},
		{"null key", "~: x\n"},
		{"complex key", "? [a]\n: x\n"},
		// Explicit tags.
		{"tag bool", "dry-run: !!bool true\n"},
		{"tag bool on string", "dry-run: !!bool \"true\"\n"},
		{"tag str on bool", "dry-run: !!str true\n"},
		{"tag str", "policy-mode: !!str all\n"},
		{"tag int addr", "metrics-addr: !!int 9090\n"},
		{"tag str number", "metrics-addr: !!str 9090\n"},
		{"tag custom", "policy-mode: !custom all\n"},
		{"tag seq", "device-allow: !!seq [/dev/a]\n"},
		{"tag map on root", "!!map {dry-run: true}\n"},
		{"tag null", "metrics-addr: !!null ''\n"},
		{"tag str null", "metrics-addr: !!str null\n"},
		{"tag binary", "metrics-addr: !!binary aGk=\n"},
		{"tag float", "metrics-addr: !!float 1\n"},
		{"tag timestamp", "metrics-addr: 2026-01-02\n"},
		// Scalars and collections.
		{"flow mapping root", "{dry-run: true, policy-mode: all}\n"},
		{"hex number addr", "metrics-addr: 0x10\n"},
		{"octal number addr", "metrics-addr: 0o17\n"},
		{"float addr", "debug-addr: 1.5\n"},
		{"colon addr", "metrics-addr: :9090\n"},
		{"ip addr", "debug-addr: 127.0.0.1:6060\n"},
		{"ipv6 addr", "metrics-addr: \"[::]:9090\"\n"},
		{"spaced flow list", "device-allow: [ /dev/a , /dev/b ]\n"},
		{"block list with comment", "device-allow:\n  # GPUs\n  - /dev/dri/*\n"},
		{"nested list", "device-allow: [[/dev/a]]\n"},
		{"mapping element", "device-allow: [{a: b}]\n"},
		{"number element", "device-deny: [1]\n"},
		{"bool element", "device-deny: [true]\n"},
		{"literal block", "docker-socket: |\n  /run/docker.sock\n"},
		{"folded block", "docker-socket: >-\n  /run/docker.sock\n"},
		{"glob outside dev", "device-allow: [/etc/*]\n"},
		{"glob not clean", "device-deny: [/dev//sda]\n"},
		{"glob bad syntax", "device-deny: [\"/dev/[a\"]\n"},
		{"debug-addr set", "debug-addr: localhost:6060\n"},
		{"policy-mode opt-in", "policy-mode: opt-in\n"},
		{"log-format plain", "log-format: plain\n"},
		{"log-format upper", "log-format: JSON\n"},
		{"log-level fatal", "log-level: fatal\n"},
		// Syntax errors and odd bytes.
		{"unclosed flow", "dry-run: [\n"},
		{"tab indent", "device-allow:\n\t- /dev/a\n"},
		{"unterminated quote", "policy-mode: 'all\n"},
		{"bad indentation", "dry-run: true\n  policy-mode: all\n"},
		{"crlf line ends", "dry-run: true\r\npolicy-mode: all\r\n"},
		{"byte order mark", "\ufeffdry-run: true\n"},
		{"no final newline", "dry-run: true"},
		{"trailing spaces", "dry-run: true   \n"},
		{"directive", "%YAML 1.2\n---\ndry-run: true\n"},
		{"bad directive", "%YAML 2.0\n---\ndry-run: true\n"},
		{"scalar root", "hello\n"},
		{"integer root", "42\n"},
		{"empty flow mapping root", "{}\n"},
		// An alias as a key: its anchor name is what the shape pass looks up, and the decode
		// reads the key it names.
		{"alias key named as a key", "a: &dry-run x\n*dry-run : \"yes\"\n"},
		{"alias key quoted bool", "a: &k dry-run\n*k : \"yes\"\n"},
		{"alias key null", "a: &k dry-run\n*k : null\n"},
		{"alias key int", "a: &k dry-run\n*k : 2\n"},
		{"alias key list", "a: &k dry-run\n*k : [x]\n"},
		{"alias key scalar list", "a: &k device-allow\n*k : x\n"},
		{"alias key null element", "a: &k device-allow\n*k : [~, x]\n"},
		{"alias key null list", "a: &k device-allow\n*k : ~\n"},
		{"alias key bad int tag", "a: &k log-level\n*k : !!int abc\n"},
		{"alias key mapping", "a: &k log-level\n*k : {b: c}\n"},
		{"alias key bool tag on yes", "a: &k dry-run\n*k : !!bool yes\n"},
		{"alias key float tag", "a: &k dry-run\n*k : !!float 1\n"},
		{"alias key null tag", "a: &k dry-run\n*k : !!null ''\n"},
		{"alias key long string", "a: &k dry-run\n*k : !!str abcdefghijkl\n"},
		{"binary key", "!!binary aGk=: x\n"},
		{"bad binary key", "!!binary aGk: x\n"},
		{"two sequence keys", "[a]: x\n[b]: y\n"},
		{"quoted duplicate key", "x: 1\n\"x\": 2\n"},
		{"block scalar key", "? |\n  log-level\n: debug\n"},
		{"unknown anchor after the document", "dry-run: true\n---\nb: *nope\n"},
		{"directive 1.1", "%YAML 1.1\n---\ndry-run: true\n"},
		{"comment then document end", "# c\n...\n"},
		{"repeated document end", "dry-run: true # c\n...\n...\n"},
		{
			"excessive aliasing",
			"b: &b [" + strings.Repeat("/dev/x, ", 1100) + "/dev/x]\ndevice-allow: *b\n",
		},
		{
			"aliases within the limit",
			"a: &x /dev/a\ndevice-allow: [" + strings.Repeat("*x, ", 1200) + "*x]\n",
		},
		// resolve's number and timestamp edges, against a string key.
		{"float out of range", "metrics-addr: 1e999\n"},
		{"no such day", "metrics-addr: 2026-02-29\n"},
		{"leap day", "metrics-addr: 2024-02-29\n"},
		{"hex float", "metrics-addr: 0x1p-2\n"},
		{"dot float with underscore", "debug-addr: .5_5\n"},
		{"uint64 max", "debug-addr: 18446744073709551615\n"},
		{"leading zero not octal", "debug-addr: 09\n"},
		{"timestamp without zone", "debug-addr: 2026-01-02T03:04:05\n"},
		{"timestamp with space", "debug-addr: 2026-01-02 3:4:5.5\n"},
		// libyaml's depth limit, and nesting just within it.
		{"deep block nesting", "device-allow:\n" + strings.Repeat("- ", 20000) + "x\n"},
		{"block nesting within the limit", "device-allow:\n" + strings.Repeat("- ", 9990) + "x\n"},
		{"deep flow nesting", "device-allow: " + strings.Repeat("[", 20000) + "\n"},
		{
			"flow nesting within the limit",
			"device-allow: " + strings.Repeat("[", 9990) + strings.Repeat("]", 9990) + "\n",
		},
		// Directives sit between documents only.
		{"directive text in a quoted value", "debug-addr: \"a\n%YAML 1.2 b\"\n"},
		{"directive text in a root scalar", "\"a\n%YAML 1.2 b\"\n"},
		{"unindented quoted continuation", "debug-addr: \"a\nb\"\n"},
		{"directive after a document end", "dry-run: true\n...\n%YAML 1.2\n---\nb: 1\n"},
		{"directive after an implicit end", "dry-run: true\n%YAML 1.2\n---\nb: 1\n"},
		{"directive after a comment", "# c\n%YAML 1.2\n---\ndry-run: true\n"},
		// yaml.v3 keeps anchors across documents.
		{"alias to an earlier document", "docker-socket: &a /x\n---\nb: *a\n"},
	}
}

func toGoldenSchema(schema *FileSchema) *goldenSchema {
	return &goldenSchema{
		LogFormat: schema.LogFormat, LogLevel: schema.LogLevel, LogTime: schema.LogTime,
		DockerSocket: schema.DockerSocket, DryRun: schema.DryRun, PolicyMode: schema.PolicyMode,
		DeviceAllow: schema.DeviceAllow, DeviceDeny: schema.DeviceDeny,
		MetricsAddr: schema.MetricsAddr, DebugAddr: schema.DebugAddr,
	}
}

// loadGolden loads path, replacing the path in an error with pathPlaceholder.
func loadGolden(name, input, path string) goldenLoad {
	result := goldenLoad{Name: name, Input: input}

	schema, err := LoadFile(path)
	if err != nil {
		result.Error = strings.ReplaceAll(err.Error(), path, pathPlaceholder)

		return result
	}

	result.Schema = toGoldenSchema(&schema)

	return result
}

// TestGoldenConfig captures tests/testdata/config/loader.json.
func TestGoldenConfig(t *testing.T) {
	var golden goldenConfig

	for _, input := range goldenInputs() {
		path := filepath.Join(t.TempDir(), "config.yaml")

		err := os.WriteFile(path, []byte(input[1]), 0o600)
		if err != nil {
			t.Fatal(err)
		}

		golden.Load = append(golden.Load, loadGolden(input[0], input[1], path))
	}

	// No file at all: an empty path is the zero config without a read, and a missing file is a
	// read error.
	golden.Load = append(golden.Load,
		loadGolden("empty path", "", ""),
		loadGolden("missing file", "", filepath.Join(t.TempDir(), "config.yaml")),
	)

	shipped := filepath.Join(goldencapture.Root(t), "deployments", "docker", "config.yaml")

	content, err := os.ReadFile(shipped)
	if err != nil {
		t.Fatal(err)
	}

	golden.Shipped = loadGolden("deployments/docker/config.yaml", string(content), shipped)

	for _, enums := range [][3]string{
		{"", "", ""},
		{"text", "info", "all"},
		{"plain", "warn", "opt-in"},
		{"json", "debug", ""},
		{"xml", "", ""},
		{"", "warning", ""},
		{"", "fatal", ""},
		{"", "panic", ""},
		{"", "", "everything"},
		{"TEXT", "", ""},
		{"", "INFO", ""},
		{"", "", "ALL"},
		{"xml", "bogus", "none"},
	} {
		for _, source := range []string{"flag", "config file"} {
			golden.ValidateEnums = append(golden.ValidateEnums, goldenEnums{
				LogFormat: enums[0], LogLevel: enums[1], PolicyMode: enums[2], Source: source,
				Error: errorText(ValidateEnums(enums[0], enums[1], enums[2], source)),
			})
		}
	}

	goldencapture.Check(t, "tests/testdata/config/loader.json", goldencapture.JSON(t, golden))
}

func errorText(err error) string {
	if err == nil {
		return ""
	}

	return err.Error()
}
