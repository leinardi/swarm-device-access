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

//! The daemon-wide and per-container device-access policy (`internal/policy`).
//!
//! The container's `swarm-device-access.*` labels are untrusted input: anyone who can create a
//! container sets them. They can only narrow what the global policy allows, deny always wins over
//! allow, and a malformed label is an error the caller answers with the empty desired set.
//!
//! The Go `Global` holds its mode as a string, so an empty or unknown mode had to process nothing
//! (`Enabled`'s default branch). Here a [`Global`] is made only by [`Global::new`], which runs Go's
//! `Validate`, so a policy with such a mode cannot exist; [`Mode`] names exactly the two modes.
//! Every list keeps Go's nil apart from an empty one (`None` against `Some(vec![])`), since the
//! two log differently in JSON.

#[cfg(test)]
mod tests;

use std::collections::HashMap;
use std::fmt;
use std::hash::BuildHasher;

use crate::go_compat::filepath::{self, BadPattern};
use crate::go_compat::parse_bool::parse_bool;
use crate::go_compat::quote::quote;
use crate::go_compat::strings::trim_space;

/// The prefix of every policy label.
pub const LABEL_PREFIX: &str = "swarm-device-access.";
/// `swarm-device-access.enable`: opts a container in or out.
pub const LABEL_ENABLE: &str = "swarm-device-access.enable";
/// `swarm-device-access.device-allow`: narrows the allowed device globs.
pub const LABEL_DEVICE_ALLOW: &str = "swarm-device-access.device-allow";
/// `swarm-device-access.device-deny`: adds denied device globs.
pub const LABEL_DEVICE_DENY: &str = "swarm-device-access.device-deny";

/// The known policy labels.
const KNOWN_LABELS: [&str; 3] = [LABEL_ENABLE, LABEL_DEVICE_ALLOW, LABEL_DEVICE_DENY];

/// What every glob must start with: only paths under `/dev` are ever matched against one.
const DEV_PREFIX: &str = "/dev/";

/// Which containers the daemon processes by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// `opt-in`: only containers labelled `swarm-device-access.enable=true`.
    OptIn,
    /// `all`: every container not labelled `swarm-device-access.enable=false`.
    All,
}

impl Mode {
    /// The mode's name, as the flag and the config file spell it.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OptIn => "opt-in",
            Self::All => "all",
        }
    }
}

impl fmt::Display for Mode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Go's `ParseMode`: exactly `opt-in` or `all`.
///
/// # Errors
///
/// [`PolicyError::UnknownMode`] for any other value, in any case or with any space.
pub fn parse_mode(mode: &str) -> Result<Mode, PolicyError> {
    match mode {
        "opt-in" => Ok(Mode::OptIn),
        "all" => Ok(Mode::All),
        _ => Err(PolicyError::UnknownMode {
            mode: mode.to_owned(),
        }),
    }
}

/// A glob that is malformed, not clean or not under `/dev/`: it would never match a device path,
/// which for a deny means a device the operator meant to deny stays grantable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GlobError {
    /// `filepath.Match` refuses the pattern.
    BadPattern {
        /// The pattern.
        pattern: String,
        /// `filepath.ErrBadPattern`.
        source: BadPattern,
    },
    /// `filepath.Clean` would change the pattern.
    NotClean {
        /// The pattern.
        pattern: String,
    },
    /// The pattern does not start with `/dev/`.
    OutsideDev {
        /// The pattern.
        pattern: String,
    },
}

impl fmt::Display for GlobError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadPattern { pattern, source } => {
                write!(
                    formatter,
                    "invalid glob pattern {}: {source}",
                    quote(pattern)
                )
            }
            Self::NotClean { pattern } => {
                write!(
                    formatter,
                    "glob pattern is not a clean path: {}",
                    quote(pattern)
                )
            }
            Self::OutsideDev { pattern } => {
                write!(
                    formatter,
                    "glob pattern must start with /dev/: {}",
                    quote(pattern)
                )
            }
        }
    }
}

impl std::error::Error for GlobError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::BadPattern { source, .. } => Some(source),
            Self::NotClean { .. } | Self::OutsideDev { .. } => None,
        }
    }
}

/// A policy that cannot be used, with Go's error text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyError {
    /// A mode other than `opt-in` or `all`.
    UnknownMode {
        /// The mode as given.
        mode: String,
    },
    /// A `swarm-device-access.enable` value that is not a `strconv.ParseBool` spelling.
    NotABoolean {
        /// The label key.
        label: &'static str,
        /// The label value.
        value: String,
    },
    /// A bad glob in a container label.
    LabelGlob {
        /// The label key.
        label: &'static str,
        /// What is wrong with the glob.
        source: GlobError,
    },
    /// A bad glob in the global `device-allow` list.
    DeviceAllow(GlobError),
    /// A bad glob in the global `device-deny` list.
    DeviceDeny(GlobError),
}

impl fmt::Display for PolicyError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownMode { mode } => write!(
                formatter,
                "unknown policy mode {}: must be \"opt-in\" or \"all\"",
                quote(mode)
            ),
            Self::NotABoolean { label, value } => write!(
                formatter,
                "invalid value for label {}: {} is not a boolean",
                quote(label),
                quote(value)
            ),
            Self::LabelGlob { label, source } => {
                write!(
                    formatter,
                    "invalid glob in label {}: {source}",
                    quote(label)
                )
            }
            Self::DeviceAllow(source) => write!(formatter, "device-allow: {source}"),
            Self::DeviceDeny(source) => write!(formatter, "device-deny: {source}"),
        }
    }
}

impl std::error::Error for PolicyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::LabelGlob { source, .. }
            | Self::DeviceAllow(source)
            | Self::DeviceDeny(source) => Some(source),
            Self::UnknownMode { .. } | Self::NotABoolean { .. } => None,
        }
    }
}

/// Go's `ValidateGlobs`: each pattern in order must be well-formed, clean and under `/dev/`,
/// checked in that order.
///
/// # Errors
///
/// The first pattern that fails, with what is wrong with it.
pub fn validate_globs(patterns: &[String]) -> Result<(), GlobError> {
    for pattern in patterns {
        if let Err(source) = filepath::matches(pattern, "") {
            return Err(GlobError::BadPattern {
                pattern: pattern.clone(),
                source,
            });
        }
        if filepath::clean(pattern) != *pattern {
            return Err(GlobError::NotClean {
                pattern: pattern.clone(),
            });
        }
        if !pattern.starts_with(DEV_PREFIX) {
            return Err(GlobError::OutsideDev {
                pattern: pattern.clone(),
            });
        }
    }
    Ok(())
}

/// The daemon-wide policy, from the flags and the config file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Global {
    mode: Mode,
    device_allow: Option<Vec<String>>,
    device_deny: Option<Vec<String>>,
}

impl Global {
    /// Go's `Global{...}.Validate()`: the mode, then the allow globs, then the deny globs.
    ///
    /// # Errors
    ///
    /// The first check that fails: [`PolicyError::UnknownMode`], [`PolicyError::DeviceAllow`] or
    /// [`PolicyError::DeviceDeny`].
    pub fn new(
        mode: &str,
        device_allow: Option<Vec<String>>,
        device_deny: Option<Vec<String>>,
    ) -> Result<Self, PolicyError> {
        let mode = parse_mode(mode)?;
        validate_globs(device_allow.as_deref().unwrap_or_default())
            .map_err(PolicyError::DeviceAllow)?;
        validate_globs(device_deny.as_deref().unwrap_or_default())
            .map_err(PolicyError::DeviceDeny)?;
        Ok(Self {
            mode,
            device_allow,
            device_deny,
        })
    }

    /// The mode.
    #[must_use]
    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// The allow globs; `None` is Go's nil list.
    #[must_use]
    pub fn device_allow(&self) -> Option<&[String]> {
        self.device_allow.as_deref()
    }

    /// The deny globs; `None` is Go's nil list.
    #[must_use]
    pub fn device_deny(&self) -> Option<&[String]> {
        self.device_deny.as_deref()
    }

    /// Go's `Enabled`: whether a container is processed. `enable=false` always skips; opt-in
    /// processes only `enable=true`; all processes everything else.
    #[must_use]
    pub fn enabled(&self, container: &Container) -> bool {
        if container.enable == Some(false) {
            return false;
        }
        match self.mode {
            Mode::OptIn => container.enable == Some(true),
            Mode::All => true,
        }
    }

    /// Go's `DeviceAllowed`: not [`Global::denied`] and [`Global::authorized`]. The global
    /// policy is the most a container gets; its labels only narrow it, and deny wins over allow.
    #[must_use]
    pub fn device_allowed(&self, container: &Container, path: &str) -> bool {
        !self.denied(container, path) && self.authorized(container, path)
    }

    /// Go's `Denied`: whether `path` matches a global or a per-container deny glob. A device is
    /// checked under every name it was found by, and one match on any of them denies it.
    #[must_use]
    pub fn denied(&self, container: &Container, path: &str) -> bool {
        match_any(self.device_deny(), path) || match_any(container.device_deny(), path)
    }

    /// Go's `Authorized`: whether `path` passes the global and the per-container allow lists,
    /// each allowing everything when empty.
    #[must_use]
    pub fn authorized(&self, container: &Container, path: &str) -> bool {
        if !is_empty(self.device_allow()) && !match_any(self.device_allow(), path) {
            return false;
        }
        is_empty(container.device_allow()) || match_any(container.device_allow(), path)
    }

    /// Go's `ExplicitlyAllowed`: [`Global::device_allowed`], and matched by at least one allow
    /// glob, so it is false when no allow glob is configured at all.
    #[must_use]
    pub fn explicitly_allowed(&self, container: &Container, path: &str) -> bool {
        (!is_empty(self.device_allow()) || !is_empty(container.device_allow()))
            && self.device_allowed(container, path)
    }
}

/// The per-container policy, from the container's labels.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Container {
    enable: Option<bool>,
    device_allow: Option<Vec<String>>,
    device_deny: Option<Vec<String>>,
}

impl Container {
    /// Go's `ParseContainer`: the `swarm-device-access.*` labels of a container. An empty
    /// `device-allow` or `device-deny` label is no list; one holding only commas and space is an
    /// empty list; both narrow nothing.
    ///
    /// # Errors
    ///
    /// [`PolicyError::NotABoolean`] for an `enable` value that is not a `strconv.ParseBool`
    /// spelling, the empty one included, else [`PolicyError::LabelGlob`] for the first bad glob of
    /// `device-allow`, then of `device-deny`. The caller must then fail closed.
    pub fn parse<S: BuildHasher>(labels: &HashMap<String, String, S>) -> Result<Self, PolicyError> {
        let enable = match labels.get(LABEL_ENABLE) {
            None => None,
            Some(value) => Some(parse_bool(value).map_err(|_| PolicyError::NotABoolean {
                label: LABEL_ENABLE,
                value: value.clone(),
            })?),
        };
        let device_allow = parse_glob_list(labels, LABEL_DEVICE_ALLOW)?;
        let device_deny = parse_glob_list(labels, LABEL_DEVICE_DENY)?;
        Ok(Self {
            enable,
            device_allow,
            device_deny,
        })
    }

    /// The `enable` label: `None` when absent.
    #[must_use]
    pub fn enable(&self) -> Option<bool> {
        self.enable
    }

    /// The allow globs; `None` is Go's nil list.
    #[must_use]
    pub fn device_allow(&self) -> Option<&[String]> {
        self.device_allow.as_deref()
    }

    /// The deny globs; `None` is Go's nil list.
    #[must_use]
    pub fn device_deny(&self) -> Option<&[String]> {
        self.device_deny.as_deref()
    }
}

/// Go's `parseGlobList`: the comma-separated globs of `label`, validated; an absent or empty
/// label is no list.
fn parse_glob_list<S: BuildHasher>(
    labels: &HashMap<String, String, S>,
    label: &'static str,
) -> Result<Option<Vec<String>>, PolicyError> {
    let raw = labels.get(label).map_or("", String::as_str);
    if raw.is_empty() {
        return Ok(None);
    }
    let patterns = split_and_trim(raw);
    validate_globs(&patterns).map_err(|source| PolicyError::LabelGlob { label, source })?;
    Ok(Some(patterns))
}

/// Go's `splitAndTrim`: split on commas, `strings.TrimSpace` each part, drop the empty ones.
fn split_and_trim(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(trim_space)
        .filter(|part| !part.is_empty())
        .map(str::to_owned)
        .collect()
}

/// Go's `matchAny`: whether `path` matches a pattern; a malformed pattern matches nothing.
fn match_any(patterns: Option<&[String]>, path: &str) -> bool {
    patterns
        .unwrap_or_default()
        .iter()
        .any(|pattern| filepath::matches(pattern, path) == Ok(true))
}

/// `len(list) == 0`, for a nil or an empty list alike.
fn is_empty(patterns: Option<&[String]>) -> bool {
    patterns.is_none_or(<[String]>::is_empty)
}

/// Go's `UnknownLabels`: the `swarm-device-access.*` keys that are not policy labels, sorted;
/// empty when there are none (Go's nil).
#[must_use]
pub fn unknown_labels<S: BuildHasher>(labels: &HashMap<String, String, S>) -> Vec<String> {
    let mut unknown: Vec<String> = labels
        .keys()
        .filter(|key| key.starts_with(LABEL_PREFIX) && !KNOWN_LABELS.contains(&key.as_str()))
        .cloned()
        .collect();
    unknown.sort();
    unknown
}
