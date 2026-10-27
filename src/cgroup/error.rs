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
//! so the text is the 1.x contract. The classifiers (`is_drift`, `is_filter_missing`, ...) answer
//! what Go's `errors.Is` answered on the sentinel, through every wrap and join.

use std::fmt;
use std::io;

use super::asm::DecodeError;
use super::owned::{CanonicalError, EmitError, GateError, NonceError, OwnedError};
use super::v2ops::{OpsError, describe_attach_flags};
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
    /// The cgroup v2 query of the attached device filters failed.
    Query(OpsError),
    /// Go's `ErrFiltersInaccessible`: some attached programs cannot be opened (an LSM label,
    /// typically), so nothing can be planned safely.
    FiltersInaccessible { inaccessible: usize, total: usize },
    /// Go's `ErrFilterMissing`: no device filter is attached and none is cached to rebuild.
    FilterMissing,
    /// Go's `ErrUnsupportedAttachMode`: the filters are attached without `BPF_F_ALLOW_MULTI`;
    /// the flags as found.
    UnsupportedAttachMode(u32),
    /// A kernel operation of the cgroup v2 controller failed; its own text.
    Ops(OpsError),
    /// An attached program's instructions do not encode.
    Canonical(CanonicalError),
    /// An attached program cannot be taken apart; Go's `ErrOwnedBlockConflict` among them.
    Owned(OwnedError),
    /// Go's `ErrProgramNotWrappable` from the gate.
    Gate(GateError),
    /// A program's canonical bytes do not decode.
    Decode(DecodeError),
    /// A wrapper cannot be emitted.
    Emit(EmitError),
    /// The nonce of a new wrapper lineage cannot be drawn.
    Nonce(NonceError),
    /// Step `step` of the `steps` of a cgroup v2 pass failed; the earlier ones are done.
    Change {
        step: usize,
        steps: usize,
        source: Box<Error>,
    },
    /// The atomic replace failed on a kernel that supports it.
    Replace(OpsError),
    /// The attach of the new program of a pair failed; nothing changed.
    AttachNew(OpsError),
    /// The detach of the replaced program failed after the new one was attached.
    DetachOriginal(OpsError),
    /// The detach of the new program, undoing a pair whose original stayed, failed too.
    Rollback(OpsError),
}

impl Error {
    /// Whether it is, or joins, Go's `ErrDeviceRulesDrift`: a retry recomputes from the file and
    /// converges.
    #[must_use]
    pub fn is_drift(&self) -> bool {
        self.any(&|error| matches!(error, Self::Drift(_)))
    }

    /// Whether it is Go's `ErrFilterMissing`: no device filter to keep the grants in, and none
    /// cached to rebuild.
    #[must_use]
    pub fn is_filter_missing(&self) -> bool {
        self.any(&|error| matches!(error, Self::FilterMissing))
    }

    /// Whether it is Go's `ErrFiltersInaccessible`.
    #[must_use]
    pub fn is_filters_inaccessible(&self) -> bool {
        self.any(&|error| matches!(error, Self::FiltersInaccessible { .. }))
    }

    /// Whether it is Go's `ErrUnsupportedAttachMode`.
    #[must_use]
    pub fn is_unsupported_attach_mode(&self) -> bool {
        self.any(&|error| matches!(error, Self::UnsupportedAttachMode(_)))
    }

    /// Whether it is Go's `ErrOwnedBlockConflict`.
    #[must_use]
    pub fn is_owned_block_conflict(&self) -> bool {
        self.any(&|error| matches!(error, Self::Owned(owned) if owned.is_conflict()))
    }

    /// Whether it is Go's `ErrProgramNotWrappable`: the gate refused a program, or the kernel is
    /// too old to read one back.
    #[must_use]
    pub fn is_program_not_wrappable(&self) -> bool {
        self.any(&|error| match error {
            Self::Gate(_) => true,
            Self::Query(ops)
            | Self::Ops(ops)
            | Self::Replace(ops)
            | Self::AttachNew(ops)
            | Self::DetachOriginal(ops)
            | Self::Rollback(ops) => ops.is_not_wrappable(),
            Self::DenyRule
            | Self::ConcreteRule(_)
            | Self::UnknownAccess(_)
            | Self::ListEntry { .. }
            | Self::ReadList(_)
            | Self::File(_)
            | Self::Revoke { .. }
            | Self::Grant { .. }
            | Self::Verify(_)
            | Self::Drift(_)
            | Self::Joined(_)
            | Self::FiltersInaccessible { .. }
            | Self::FilterMissing
            | Self::UnsupportedAttachMode(_)
            | Self::Canonical(_)
            | Self::Owned(_)
            | Self::Decode(_)
            | Self::Emit(_)
            | Self::Nonce(_)
            | Self::Change { .. } => false,
        })
    }

    /// Whether `matches` holds for this error or any error it wraps or joins, as Go's
    /// `errors.Is` walks the chain.
    fn any(&self, matches: &impl Fn(&Self) -> bool) -> bool {
        if matches(self) {
            return true;
        }
        match self {
            Self::Joined(errors) => errors.iter().any(|error| error.any(matches)),
            Self::Verify(source) | Self::Change { source, .. } => source.any(matches),
            Self::DenyRule
            | Self::ConcreteRule(_)
            | Self::UnknownAccess(_)
            | Self::ListEntry { .. }
            | Self::ReadList(_)
            | Self::File(_)
            | Self::Revoke { .. }
            | Self::Grant { .. }
            | Self::Drift(_)
            | Self::Query(_)
            | Self::FiltersInaccessible { .. }
            | Self::FilterMissing
            | Self::UnsupportedAttachMode(_)
            | Self::Ops(_)
            | Self::Canonical(_)
            | Self::Owned(_)
            | Self::Gate(_)
            | Self::Decode(_)
            | Self::Emit(_)
            | Self::Nonce(_)
            | Self::Replace(_)
            | Self::AttachNew(_)
            | Self::DetachOriginal(_)
            | Self::Rollback(_) => false,
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
            Self::Query(source) => write!(
                formatter,
                "unable to find any existing device filters attached to the cgroup: {source}"
            ),
            Self::FiltersInaccessible {
                inaccessible,
                total,
            } => write!(
                formatter,
                "device filters attached to the cgroup cannot be accessed: {inaccessible} of \
                 {total} programs"
            ),
            Self::FilterMissing => formatter.write_str("no device filter attached to the cgroup"),
            Self::UnsupportedAttachMode(flags) => write!(
                formatter,
                "device filter attached without BPF_F_ALLOW_MULTI: attach mode {}",
                describe_attach_flags(*flags)
            ),
            Self::Ops(source) => source.fmt(formatter),
            Self::Canonical(source) => source.fmt(formatter),
            Self::Owned(source) => source.fmt(formatter),
            Self::Gate(source) => source.fmt(formatter),
            Self::Decode(source) => source.fmt(formatter),
            Self::Emit(source) => source.fmt(formatter),
            Self::Nonce(source) => source.fmt(formatter),
            Self::Change {
                step,
                steps,
                source,
            } => write!(
                formatter,
                "device filter change {step} of {steps}: {source}"
            ),
            Self::Replace(source) => {
                write!(
                    formatter,
                    "unable to replace device filters program: {source}"
                )
            }
            Self::AttachNew(source) => write!(
                formatter,
                "unable to attach new device filters program: {source}"
            ),
            Self::DetachOriginal(source) => write!(
                formatter,
                "unable to detach original device filters program: {source}"
            ),
            Self::Rollback(source) => {
                write!(formatter, "roll back new device filters program: {source}")
            }
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
            Self::Verify(source) | Self::Change { source, .. } => Some(source.as_ref()),
            Self::Query(source)
            | Self::Ops(source)
            | Self::Replace(source)
            | Self::AttachNew(source)
            | Self::DetachOriginal(source)
            | Self::Rollback(source) => Some(source),
            Self::Canonical(source) => Some(source),
            Self::Owned(source) => Some(source),
            Self::Gate(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::Emit(source) => Some(source),
            Self::Nonce(source) => Some(source),
            Self::DenyRule
            | Self::ConcreteRule(_)
            | Self::UnknownAccess(_)
            | Self::ListEntry { cause: None, .. }
            | Self::Drift(_)
            | Self::Joined(_)
            | Self::FiltersInaccessible { .. }
            | Self::FilterMissing
            | Self::UnsupportedAttachMode(_) => None,
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
