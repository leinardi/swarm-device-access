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

// Package gitenv gives meta-tests an environment for git commands that cannot reach the
// repository being committed.
//
// The Go tests also run inside a pre-commit hook, and git exports its own location to hooks:
// GIT_INDEX_FILE always, GIT_DIR and others in some setups, absolute in a linked worktree. A git
// command run in a scratch repository with that environment acts on the real one instead -- a git
// add there writes into the index of the commit being made. Isolated strips those variables and
// the user's own configuration, so a scratch repository is only ever itself.
package gitenv

import (
	"os"
	"slices"
	"strings"
)

// repositoryVars point git at a repository other than the working directory's.
var repositoryVars = []string{
	"GIT_DIR",
	"GIT_INDEX_FILE",
	"GIT_WORK_TREE",
	"GIT_OBJECT_DIRECTORY",
	"GIT_ALTERNATE_OBJECT_DIRECTORIES",
	"GIT_COMMON_DIR",
	"GIT_PREFIX",
	"GIT_NAMESPACE",
	"GIT_CEILING_DIRECTORIES",
}

// Isolated returns the process environment without the variables that redirect git, without the
// user's global and system configuration, and with extra appended.
func Isolated(extra ...string) []string {
	env := make([]string, 0, len(os.Environ())+len(extra)+2)

	for _, entry := range os.Environ() {
		name, _, _ := strings.Cut(entry, "=")
		if slices.Contains(repositoryVars, name) || strings.HasPrefix(name, "GIT_CONFIG") {
			continue
		}

		env = append(env, entry)
	}

	env = append(env, "GIT_CONFIG_GLOBAL=/dev/null", "GIT_CONFIG_NOSYSTEM=1")

	return append(env, extra...)
}
