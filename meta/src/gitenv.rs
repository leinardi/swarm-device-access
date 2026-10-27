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
// Adapted from swarm-scheduler-exporter/meta/src/gitenv.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! An environment for git commands that cannot reach the repository being committed.
//!
//! The meta-tests also run inside a pre-commit hook, and git exports its own location to hooks:
//! `GIT_INDEX_FILE` always, `GIT_DIR` and others in some setups, absolute in a linked worktree. A
//! git command run in a scratch repository with that environment acts on the real one instead -- a
//! `git add` there writes into the index of the commit being made. [`isolate`] strips those
//! variables and the user's own configuration, so a scratch repository is only ever itself. The
//! Rust side of `test/internal/gitenv`, which stays until the cutover.

use std::env;
use std::process::Command;

/// The variables that point git at a repository other than the working directory's.
const REPOSITORY_VARS: &[&str] = &[
    "GIT_DIR",
    "GIT_INDEX_FILE",
    "GIT_WORK_TREE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

/// Removes from `command` the variables that redirect git and the user's global and system
/// configuration.
pub fn isolate(command: &mut Command) -> &mut Command {
    for (name, _) in env::vars_os() {
        let redirects = name
            .to_str()
            .is_some_and(|name| REPOSITORY_VARS.contains(&name) || name.starts_with("GIT_CONFIG"));
        if redirects {
            command.env_remove(&name);
        }
    }
    command
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
}
