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

// Package licenseheaders_test drives scripts/check-license-headers.sh: on this repository, and on
// scratch repositories that hold each kind of file it must reject or let through.
package licenseheaders_test

import (
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/test/internal/gitenv"
	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

const scriptPath = "scripts/check-license-headers.sh"

const apacheBody = ` *
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
`

const goHeader = "//go:build linux\n\n/*\n * Copyright 2026 Roberto Leinardi.\n" + apacheBody +
	"\npackage x\n"

// nvidiaHeader is the header of the files preserved from NVIDIA's code.
const nvidiaHeader = "/*\n * Copyright (c) 2021, NVIDIA CORPORATION.  All rights reserved.\n" + apacheBody +
	"\npackage cgroup\n"

// mitHeader is another license altogether.
const mitHeader = `/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy
 */

package x
`

const shellHeader = `#!/usr/bin/env bash
#
# Copyright 2026 Roberto Leinardi.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
echo ok
`

const makefile = "MK_COMMON_FILES := docker.mk help.mk\n"

// makefileWithHeader is the scratch repositories' Makefile, which the check governs too.
const makefileWithHeader = "#\n# Copyright 2026 Roberto Leinardi.\n#\n" +
	"# Licensed under the Apache License, Version 2.0 (the \"License\");\n" +
	"# you may not use this file except in compliance with the License.\n" +
	"# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.\n" + makefile

// untrackedPrefix marks scratch files that are written but never added: the check reads tracked
// files only.
const untrackedPrefix = "untracked/"

// TestRepositoryHeaders runs the check on this repository.
func TestRepositoryHeaders(t *testing.T) {
	t.Parallel()

	root := toolgate.RepoRoot(t)
	readScript(t, root)
	readTrackedFiles(t, root)

	// The repository's own git environment is the right one here: under the commit hook it names
	// the index being committed.
	cmd := exec.CommandContext(t.Context(), scriptPath)
	cmd.Dir = root

	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Errorf("the repository fails its own license header check: %v\n%s", err, out)
	}
}

// TestHeaderCases runs the check on scratch repositories, one file kind at a time.
func TestHeaderCases(t *testing.T) {
	t.Parallel()

	script := readScript(t, toolgate.RepoRoot(t))
	nvidiaFile := "internal/cgroup/api.go"

	for name, test := range map[string]struct {
		files map[string]string
		pass  bool
	}{
		"go file with the header":         {map[string]string{"internal/x/x.go": goHeader}, true},
		"go file without a header":        {map[string]string{"internal/x/x.go": "package x\n"}, false},
		"go file with another license":    {map[string]string{"internal/x/x.go": mitHeader}, false},
		"another holder's copyright":      {map[string]string{"internal/x/x.go": strings.ReplaceAll(goHeader, "Roberto Leinardi", "Someone Else")}, false},
		"nvidia header on an nvidia file": {map[string]string{nvidiaFile: nvidiaHeader}, true},
		"nvidia header on any other file": {map[string]string{"internal/cgroup/new.go": nvidiaHeader}, false},
		"own header on an nvidia file":    {map[string]string{nvidiaFile: goHeader}, false},
		"shell script with the header":    {map[string]string{"scripts/a.sh": shellHeader}, true},
		"shell script without a header":   {map[string]string{"scripts/a.sh": "#!/usr/bin/env bash\necho\n"}, false},
		"dockerfile without a header":     {map[string]string{"deployments/docker/Dockerfile": "FROM scratch\n"}, false},
		"local make fragment, no header":  {map[string]string{".mk/local.mk": "x:\n"}, false},
		"makefile without a header":       {map[string]string{"Makefile": makefile}, false},
		// goHeader's last required line is its fourteenth: 11 lines before it put it on line 25,
		// the last line of the window, and one more pushes it out.
		"header ending on the last line of the window": {
			map[string]string{"internal/x/x.go": strings.Repeat("// filler\n", 11) + goHeader},
			true,
		},
		"header ending one line past the window": {
			map[string]string{"internal/x/x.go": strings.Repeat("// filler\n", 12) + goHeader},
			false,
		},
		"untracked file without a header": {map[string]string{untrackedPrefix + "scratch.go": "package x\n"}, true},
		"upstream make fragment":          {map[string]string{".mk/docker.mk": "x:\n", ".mk/help.mk": "y:\n"}, true},
		"bootstrap script":                {map[string]string{"scripts/bootstrap-mk-common.sh": "#!/bin/sh\n"}, true},
		"markdown and yaml":               {map[string]string{"README.md": "# x\n", "a.yaml": "a: 1\n"}, true},
	} {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			dir := scratchRepo(t, script, test.files)

			out, err := runIn(t, dir)
			if test.pass && err != nil {
				t.Errorf("rejected, want accepted: %v\n%s", err, out)
			}

			if !test.pass && err == nil {
				t.Errorf("accepted, want rejected:\n%s", out)
			}
		})
	}
}

// readTrackedFiles reads the index and every tracked file, so a change to any of them is part of
// the test cache key and a cached pass is not replayed after a headerless file is added.
func readTrackedFiles(t *testing.T, root string) {
	t.Helper()

	for _, args := range [][]string{{"rev-parse", "--git-path", "index"}, {"ls-files"}} {
		cmd := exec.CommandContext(t.Context(), "git", args...)
		cmd.Dir = root

		out, err := cmd.Output()
		if err != nil {
			t.Fatalf("git %v: %v", args, err)
		}

		for name := range strings.SplitSeq(strings.TrimSpace(string(out)), "\n") {
			if !filepath.IsAbs(name) {
				name = filepath.Join(root, name)
			}

			// A deleted tracked file is the script's concern, not this one's.
			_, _ = os.ReadFile(name)
		}
	}
}

// readScript reads the checker, which also makes it part of the test cache key: a program run only
// through exec is not, and an edited script would otherwise replay a cached pass.
func readScript(t *testing.T, root string) []byte {
	t.Helper()

	data, err := os.ReadFile(filepath.Join(root, scriptPath))
	if err != nil {
		t.Fatalf("read the checker: %v", err)
	}

	return data
}

// scratchRepo creates a git repository holding the checker, a Makefile and files.
func scratchRepo(t *testing.T, script []byte, files map[string]string) string {
	t.Helper()

	dir := t.TempDir()

	root, err := os.OpenRoot(dir)
	if err != nil {
		t.Fatalf("open %s: %v", dir, err)
	}

	defer func() { _ = root.Close() }()

	files[scriptPath] = string(script)
	if _, ok := files["Makefile"]; !ok {
		files["Makefile"] = makefileWithHeader
	}

	for name, content := range files {
		err = root.MkdirAll(filepath.Dir(name), 0o750)
		if err != nil {
			t.Fatalf("create the directory of %s: %v", name, err)
		}

		err = root.WriteFile(name, []byte(content), 0o600)
		if err != nil {
			t.Fatalf("write %s: %v", name, err)
		}
	}

	err = root.Chmod(scriptPath, 0o700)
	if err != nil {
		t.Fatalf("make the checker executable: %v", err)
	}

	git(t, dir, "init", "--quiet")

	for name := range files {
		if !strings.HasPrefix(name, untrackedPrefix) {
			git(t, dir, "add", "--", name)
		}
	}

	return dir
}

// runIn runs the checker kept at scriptPath inside the repository at dir.
func runIn(t *testing.T, dir string) (string, error) {
	t.Helper()

	// A relative command path is resolved against Dir.
	cmd := exec.CommandContext(t.Context(), scriptPath)
	cmd.Dir = dir
	cmd.Env = gitenv.Isolated()

	out, err := cmd.CombinedOutput()

	return string(out), err
}

func git(t *testing.T, dir string, args ...string) {
	t.Helper()

	cmd := exec.CommandContext(t.Context(), "git", args...)
	cmd.Dir = dir
	cmd.Env = gitenv.Isolated()

	out, err := cmd.CombinedOutput()
	if err != nil {
		t.Fatalf("git %v: %v\n%s", args, err, out)
	}
}
