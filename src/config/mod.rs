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

//! The daemon's configuration (`internal/config`): the strict config-file loader and the
//! generation-numbered store of the hot-reloadable settings.
//!
//! The file is checked as Go checks it, in two passes over one document (`load_file`): every
//! known key must have exactly its shape, so yaml.v3's readings of a quoted `"yes"` as true, an
//! explicit `null` as "not set" or `[null]` as `[""]` (allow-all) never reach a setting; merge
//! keys, unknown keys, duplicate keys and a second document are refused; the enumerated values
//! are validated last. Every refusal names the file. The YAML syntax errors keep Go's wrapping,
//! and their text after `yaml: ` is saphyr-parser's (Q6).
//!
//! Only the [`Publisher`] returned with a [`Store`] replaces its [`Runtime`], and only the
//! processor holds it (`meta/tests/single_publisher.rs`).

mod resolve;
#[cfg(test)]
mod tests;
mod yaml;

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::sync::{Arc, PoisonError, RwLock};

use crate::go_compat::errno::io_error_text;
use crate::go_compat::quote::quote;
use crate::policy::{self, Global, PolicyError};

pub use yaml::{DecodeError, Shape, ShapeError, SyntaxError};

/// The accepted `log-format` values.
pub const LOG_FORMATS: [&str; 3] = ["text", "json", "plain"];
/// The accepted `log-level` values.
pub const LOG_LEVELS: [&str; 4] = ["debug", "info", "warn", "error"];

/// The settings the config file may set (`FileSchema`). An empty string, `None` for a bool and
/// `None` for a list mean "not set in the file"; `Some(vec![])` is an explicit empty list, which
/// overrides the flag.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FileSchema {
    /// `log-format`.
    pub log_format: String,
    /// `log-level`.
    pub log_level: String,
    /// `log-time`.
    pub log_time: Option<bool>,
    /// `docker-socket`.
    pub docker_socket: String,
    /// `dry-run`.
    pub dry_run: Option<bool>,
    /// `policy-mode`.
    pub policy_mode: String,
    /// `device-allow`.
    pub device_allow: Option<Vec<String>>,
    /// `device-deny`.
    pub device_deny: Option<Vec<String>>,
    /// `metrics-addr`.
    pub metrics_addr: String,
    /// `debug-addr`.
    pub debug_addr: String,
}

/// Why the config file could not be loaded, with Go's text.
#[derive(Debug)]
pub enum LoadError {
    /// The file could not be read: `read config file "<path>": <op> <path>: <errno>`.
    Read {
        /// The path.
        path: String,
        /// The system call that failed, `open` or `read`, as Go's `PathError` names it.
        operation: &'static str,
        /// The error.
        source: io::Error,
    },
    /// The first document is not valid YAML: `parse config file "<path>": yaml: ...`.
    Parse {
        /// The path.
        path: String,
        /// The syntax error.
        source: SyntaxError,
    },
    /// The file parsed but is refused: `config file "<path>": ...`.
    File {
        /// The path.
        path: String,
        /// Why.
        source: FileError,
    },
}

impl fmt::Display for LoadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Read {
                path,
                operation,
                source,
            } => write!(
                formatter,
                "read config file {}: {operation} {path}: {}",
                quote(path),
                io_error_text(source)
            ),
            Self::Parse { path, source } => {
                write!(formatter, "parse config file {}: {source}", quote(path))
            }
            Self::File { path, source } => {
                write!(formatter, "config file {}: {source}", quote(path))
            }
        }
    }
}

impl std::error::Error for LoadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::Parse { source, .. } => Some(source),
            Self::File { source, .. } => Some(source),
        }
    }
}

/// Why a file that parsed is refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileError {
    /// The shape pass.
    Shape(ShapeError),
    /// The strict decode: `decode: yaml: ...`.
    Decode(DecodeError),
    /// What follows the first document does not parse: `parse trailing content: yaml: ...`.
    TrailingContent(SyntaxError),
    /// A second document.
    MultipleDocuments,
    /// An enumerated value.
    Enum(EnumError),
}

impl fmt::Display for FileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Shape(source) => source.fmt(formatter),
            Self::Decode(source) => write!(formatter, "decode: {source}"),
            Self::TrailingContent(source) => write!(formatter, "parse trailing content: {source}"),
            Self::MultipleDocuments => formatter.write_str("multiple YAML documents"),
            Self::Enum(source) => source.fmt(formatter),
        }
    }
}

impl std::error::Error for FileError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Shape(source) => Some(source),
            Self::Decode(source) => Some(source),
            Self::TrailingContent(source) => Some(source),
            Self::Enum(source) => Some(source),
            Self::MultipleDocuments => None,
        }
    }
}

/// An enumerated setting with a value outside its set (`ValidateEnums`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EnumError {
    /// What set the value: `key` for the file, `flag` or `setting` for the effective settings.
    pub source: String,
    /// The key.
    pub key: &'static str,
    /// What is wrong with the value.
    pub problem: EnumProblem,
}

/// What is wrong with an enumerated value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnumProblem {
    /// Not one of `accepted`.
    UnknownValue {
        /// The value.
        value: String,
        /// The accepted values.
        accepted: &'static [&'static str],
    },
    /// A policy mode `policy::parse_mode` refuses.
    PolicyMode(PolicyError),
}

impl fmt::Display for EnumError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{} {}: ", self.source, quote(self.key))?;
        match &self.problem {
            EnumProblem::UnknownValue { value, accepted } => write!(
                formatter,
                "unknown value {}, must be one of [{}]",
                quote(value),
                accepted.join(" ")
            ),
            EnumProblem::PolicyMode(source) => source.fmt(formatter),
        }
    }
}

impl std::error::Error for EnumError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match &self.problem {
            EnumProblem::PolicyMode(source) => Some(source),
            EnumProblem::UnknownValue { .. } => None,
        }
    }
}

/// `ValidateEnums`: `log-format`, `log-level` and `policy-mode`, in that order; an empty value is
/// not set and passes. `source` names what set them in the error.
///
/// # Errors
///
/// The first value outside its set.
pub fn validate_enums(
    log_format: &str,
    log_level: &str,
    policy_mode: &str,
    source: &str,
) -> Result<(), EnumError> {
    let error = |key, problem| EnumError {
        source: source.to_owned(),
        key,
        problem,
    };
    if !log_format.is_empty() && !LOG_FORMATS.contains(&log_format) {
        return Err(error(
            "log-format",
            EnumProblem::UnknownValue {
                value: log_format.to_owned(),
                accepted: &LOG_FORMATS,
            },
        ));
    }
    if !log_level.is_empty() && !LOG_LEVELS.contains(&log_level) {
        return Err(error(
            "log-level",
            EnumProblem::UnknownValue {
                value: log_level.to_owned(),
                accepted: &LOG_LEVELS,
            },
        ));
    }
    if !policy_mode.is_empty() {
        policy::parse_mode(policy_mode)
            .map_err(|source| error("policy-mode", EnumProblem::PolicyMode(source)))?;
    }
    Ok(())
}

/// `LoadFile`: the config file at `path`, or the empty schema for an empty path.
///
/// # Errors
///
/// A read error, a syntax error in the first document, or a refusal of the file (see
/// [`FileError`]), in Go's order: shape, decode, trailing content, enums.
pub fn load_file(path: &str) -> Result<FileSchema, LoadError> {
    if path.is_empty() {
        return Ok(FileSchema::default());
    }
    let data = read_file(path)?;
    let parse_error = |source| LoadError::Parse {
        path: path.to_owned(),
        source,
    };
    let file_error = |source| LoadError::File {
        path: path.to_owned(),
        source,
    };
    // Go's yaml.v3 reads UTF-8 (and UTF-16 with a byte order mark); anything else is a syntax
    // error here.
    let text = String::from_utf8(data).map_err(|_| parse_error(SyntaxError::invalid_utf8()))?;
    let mut reader = yaml::Reader::new(&text).map_err(parse_error)?;
    let tree = reader.document().map_err(parse_error)?;
    let single_document = |reader: &mut yaml::Reader<'_>| match reader.at_end() {
        Ok(true) => Ok(()),
        Ok(false) => Err(file_error(FileError::MultipleDocuments)),
        Err(source) => Err(file_error(FileError::TrailingContent(source))),
    };
    if tree.is_empty_document() {
        // Still exactly one document: "---" followed by a second document is not empty.
        single_document(&mut reader)?;
        return Ok(FileSchema::default());
    }
    tree.check_shapes()
        .map_err(|source| file_error(FileError::Shape(source)))?;
    let schema = tree
        .decode()
        .map_err(|source| file_error(FileError::Decode(source)))?;
    single_document(&mut reader)?;
    validate_enums(
        &schema.log_format,
        &schema.log_level,
        &schema.policy_mode,
        "key",
    )
    .map_err(|source| file_error(FileError::Enum(source)))?;
    Ok(schema)
}

/// `os.ReadFile`, with the failing call named as Go's `PathError` names it.
fn read_file(path: &str) -> Result<Vec<u8>, LoadError> {
    let read_error = |operation, source| LoadError::Read {
        path: path.to_owned(),
        operation,
        source,
    };
    let mut file = File::open(path).map_err(|source| read_error("open", source))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)
        .map_err(|source| read_error("read", source))?;
    Ok(data)
}

/// The hot-reloadable settings (`Runtime`), written at startup and on each accepted `SIGHUP`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Runtime {
    /// Whether rules are only logged.
    pub dry_run: bool,
    /// The device policy.
    pub policy: Global,
}

/// One published runtime and its generation.
#[derive(Debug)]
struct Snapshot {
    runtime: Arc<Runtime>,
    generation: u64,
}

/// Read access to the current [`Runtime`]. Only the [`Publisher`] made with it replaces it.
#[derive(Debug)]
pub struct Store {
    current: RwLock<Snapshot>,
}

impl Store {
    /// The current runtime.
    #[must_use]
    pub fn load(&self) -> Arc<Runtime> {
        self.snapshot().0
    }

    /// The current runtime and its generation.
    #[must_use]
    pub fn snapshot(&self) -> (Arc<Runtime>, u64) {
        // Under panic = "abort" a poisoned lock is only reachable in tests; the snapshot is
        // replaced whole, so it is consistent either way.
        let current = self.current.read().unwrap_or_else(PoisonError::into_inner);
        (Arc::clone(&current.runtime), current.generation)
    }

    /// The generation of the current runtime: 1 at the start, one more per publication.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.snapshot().1
    }
}

/// The one way to replace the runtime of its [`Store`]. It cannot be cloned.
#[derive(Debug)]
pub struct Publisher {
    store: Arc<Store>,
}

impl Publisher {
    /// Replaces the runtime and returns its generation. The generation is taken under the same
    /// lock that publishes, so generations follow the order the runtimes become visible.
    pub fn publish(&self, runtime: Runtime) -> u64 {
        let mut current = self
            .store
            .current
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        let generation = current.generation + 1;
        *current = Snapshot {
            runtime: Arc::new(runtime),
            generation,
        };
        generation
    }
}

/// `NewStore`: a store holding `initial` as generation 1, and its publisher.
#[must_use]
pub fn new_store(initial: Runtime) -> (Arc<Store>, Publisher) {
    let store = Arc::new(Store {
        current: RwLock::new(Snapshot {
            runtime: Arc::new(initial),
            generation: 1,
        }),
    });
    (Arc::clone(&store), Publisher { store })
}
