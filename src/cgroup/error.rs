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

//! The errors of setting a cgroup's device rules, worded as the Go daemon's: every one is logged,
//! so the text is the 1.x contract.

use std::fmt;
use std::io;

use crate::go_compat::errno::io_error_text;
use crate::go_compat::errors;
use crate::go_compat::parse_int::NumError;
use crate::go_compat::quote::{quote, quote_rune};
use crate::go_compat::scanner::TokenTooLong;

/// Why `set_device_rules` failed.
#[derive(Debug)]
pub enum Error {
    /// A deny rule: cgroup v1's set takes allow rules only.
    DenyRule,
    /// A rule that is not a `c` or `b` rule with concrete numbers, as Go's `%+v` prints it.
    ConcreteRule(String),
    /// An access letter other than `r`, `w` and `m`.
    UnknownAccess(char),
    /// A `devices.list` line that does not parse.
    ListEntry {
        line: String,
        cause: Option<ListEntryCause>,
    },
    /// `devices.list` has a line too long for Go's scanner.
    ReadList(TokenTooLong),
    /// A `devices.*` file could not be opened, read or written.
    File(FileError),
    /// The revocation of one owned bit failed; nothing was granted.
    Revoke { line: String, source: FileError },
    /// The grant of one bit failed.
    Grant { line: String, source: FileError },
    /// `devices.list` could not be read back after the writes.
    Verify(Box<Error>),
    /// Go's `ErrDeviceRulesDrift`: `devices.list`, re-read after a set, does not show exactly the
    /// daemon-owned bits the set established; each entry is `<line> (want <bool>)`.
    Drift(Vec<String>),
    /// Go's `errors.Join` of several failures.
    Joined(Vec<Error>),
}

impl Error {
    /// Whether it is, or joins, Go's `ErrDeviceRulesDrift`: a retry recomputes from the file and
    /// converges.
    #[must_use]
    pub fn is_drift(&self) -> bool {
        match self {
            Self::Drift(_) => true,
            Self::Joined(errors) => errors.iter().any(Self::is_drift),
            Self::DenyRule
            | Self::ConcreteRule(_)
            | Self::UnknownAccess(_)
            | Self::ListEntry { .. }
            | Self::ReadList(_)
            | Self::File(_)
            | Self::Revoke { .. }
            | Self::Grant { .. }
            | Self::Verify(_) => false,
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DenyRule => formatter.write_str("cgroup v1 set accepts allow rules only"),
            Self::ConcreteRule(rule) => write!(
                formatter,
                "cgroup v1 set needs a c or b rule with concrete major and minor: {rule}"
            ),
            Self::UnknownAccess(access) => {
                write!(formatter, "unknown device access {}", quote_rune(*access))
            }
            Self::ListEntry { line, cause } => {
                write!(formatter, "malformed devices.list entry: {}", quote(line))?;
                match cause {
                    Some(cause) => write!(formatter, ": {cause}"),
                    None => Ok(()),
                }
            }
            Self::ReadList(source) => write!(formatter, "read devices.list: {source}"),
            Self::File(source) => source.fmt(formatter),
            Self::Revoke { line, source } => write!(formatter, "revoke {}: {source}", quote(line)),
            Self::Grant { line, source } => write!(formatter, "grant {}: {source}", quote(line)),
            Self::Verify(source) => write!(formatter, "verify: {source}"),
            Self::Drift(mismatched) => write!(
                formatter,
                "cgroup v1 device exceptions differ from the daemon-owned set: {}",
                mismatched.join(", ")
            ),
            Self::Joined(errors) => formatter.write_str(
                &errors::join(errors.iter().map(|error| Some(error.to_string())))
                    .unwrap_or_default(),
            ),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::ListEntry {
                cause: Some(cause), ..
            } => Some(cause),
            Self::ReadList(source) => Some(source),
            Self::File(source) | Self::Revoke { source, .. } | Self::Grant { source, .. } => {
                Some(source)
            }
            Self::Verify(source) => Some(source.as_ref()),
            Self::DenyRule
            | Self::ConcreteRule(_)
            | Self::UnknownAccess(_)
            | Self::ListEntry { cause: None, .. }
            | Self::Drift(_)
            | Self::Joined(_) => None,
        }
    }
}

/// Why a `devices.list` line does not parse, after the line itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListEntryCause {
    /// `device number "<field>": <strconv.ParseInt error>`.
    Number { field: String, source: NumError },
    /// `unknown device access '<letter>'`.
    Access(char),
}

impl fmt::Display for ListEntryCause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Number { field, source } => {
                write!(formatter, "device number {}: {source}", quote(field))
            }
            Self::Access(access) => {
                write!(formatter, "unknown device access {}", quote_rune(*access))
            }
        }
    }
}

impl std::error::Error for ListEntryCause {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Number { source, .. } => Some(source),
            Self::Access(_) => None,
        }
    }
}

/// A `devices.*` file that could not be opened, read or written through the cgroup's
/// descriptor, worded as Go's `openat` and `os.File` errors.
#[derive(Debug)]
pub enum FileError {
    /// `open <name>: <errno>`.
    Open { name: String, source: io::Error },
    /// `read <name>: read <name>: <errno>`.
    Read { name: String, source: io::Error },
    /// `write "<line>" to <name>: write <name>: <errno>`.
    Write {
        line: String,
        name: String,
        source: io::Error,
    },
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { name, source } => {
                write!(formatter, "open {name}: {}", io_error_text(source))
            }
            Self::Read { name, source } => {
                write!(
                    formatter,
                    "read {name}: read {name}: {}",
                    io_error_text(source)
                )
            }
            Self::Write { line, name, source } => write!(
                formatter,
                "write {} to {name}: write {name}: {}",
                quote(line),
                io_error_text(source)
            ),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. } | Self::Read { source, .. } | Self::Write { source, .. } => {
                Some(source)
            }
        }
    }
}
