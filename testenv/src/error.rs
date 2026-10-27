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
// Adapted from swarm-scheduler-exporter/testenv/src/error.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The harness's one error type: a message that carries its context the way Go's wrapped errors
//! do (`sweep: remove container 1a2b3c4d5e6f: ...`).

use std::fmt;

use swarm_device_access::go_compat::errors;

/// A harness failure, with the chain of what was being done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Error(String);

impl Error {
    /// An error with this message.
    #[must_use]
    pub fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }

    /// This error with `context` in front, as Go's `fmt.Errorf("context: %w", err)`.
    #[must_use]
    pub fn context(self, context: impl fmt::Display) -> Self {
        Self(format!("{context}: {}", self.0))
    }

    /// Several errors as one, one message per line, as Go's `errors.Join` (the daemon's port of
    /// it); `None` when there are none.
    #[must_use]
    pub fn join(errors: Vec<Self>) -> Option<Self> {
        errors::join(errors.into_iter().map(|error| Some(error.0))).map(Self)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for Error {}
