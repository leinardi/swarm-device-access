/*
 * Copyright (c) 2021, NVIDIA CORPORATION.  All rights reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// Modified by Roberto Leinardi in 2026: ported to Rust from internal/cgroup/api.go.

//! The device rule, the controller interface, and where a process's device cgroup lives, parsed
//! from its `cgroup` and `mountinfo` files.

use std::fmt;

use serde::{Deserialize, Serialize};

use super::error::Error;
use super::handle::CgroupHandle;
use super::{v1, v2};
use crate::go_compat::quote::quote;
use crate::go_compat::scanner::{self, TokenTooLong};

/// One device rule, the shape of runtime-spec's `LinuxDeviceCgroup`, which the Go daemon inlined:
/// the JSON keys and the omission of an empty type or access and of an absent number are that
/// type's. Only tests encode or decode it; Go's decoder also matched keys case-insensitively.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceRule {
    #[serde(default)]
    pub allow: bool,
    #[serde(rename = "type", default, skip_serializing_if = "String::is_empty")]
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub major: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minor: Option<i64>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access: String,
}

impl DeviceRule {
    /// The rule as Go's `%+v` prints the struct, with each number in place of the address Go
    /// prints for a set pointer (Q7).
    pub(crate) fn go_struct_text(&self) -> String {
        let number =
            |value: Option<i64>| value.map_or_else(|| "<nil>".to_owned(), |n| n.to_string());
        format!(
            "{{Allow:{} Type:{} Major:{} Minor:{} Access:{}}}",
            self.allow,
            self.kind,
            number(self.major),
            number(self.minor),
            self.access
        )
    }
}

/// A device cgroup controller.
pub trait Interface: Send + Sync {
    /// Makes the daemon-owned device grants of the cgroup behind `handle` equal exactly `rules`.
    /// Every read and write goes through the handle's descriptor, never the path.
    ///
    /// # Errors
    ///
    /// [`Error`] when a rule cannot be expressed, a file cannot be read or written, or the
    /// result differs from what was set.
    fn set_device_rules(&self, handle: &CgroupHandle, rules: &[DeviceRule]) -> Result<(), Error>;
}

/// The cgroup version of a process's device cgroup.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CgroupVersion {
    /// The `devices` controller of the legacy hierarchy; a hybrid host resolves here.
    V1,
    /// The unified hierarchy, with a BPF device filter.
    V2,
}

impl CgroupVersion {
    /// The version as Go's `int` held it: 1 or 2.
    #[must_use]
    pub fn number(self) -> u8 {
        match self {
            Self::V1 => 1,
            Self::V2 => 2,
        }
    }
}

/// Where a process's device cgroup lives, as described by its `/proc/<pid>/cgroup` and
/// `/proc/<pid>/mountinfo`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcCgroup {
    pub version: CgroupVersion,
    /// The root of the cgroup mount, from mountinfo.
    pub mount_prefix: String,
    /// Where the hierarchy is mounted, from mountinfo.
    pub mount_point: String,
    /// The process's cgroup below `mount_prefix`.
    pub root: String,
}

/// Why a process's `cgroup` or `mountinfo` file does not say where its device cgroup is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// `malformed cgroup entry: <line>`: a line without two colons.
    MalformedCgroupEntry(String),
    /// `malformed mountinfo entry: <line>`: a line of fewer than five space-separated fields.
    MalformedMountinfoEntry(String),
    /// `relative path in mount prefix: <prefix>`.
    RelativeMountPrefix(String),
    /// Neither a `devices` nor a unified entry in the cgroup file.
    NoDeviceOrUnifiedCgroup,
    /// No `cgroup` mount whose mount point is named `devices`.
    NoCgroupV1Fs,
    /// No `devices` entry in the cgroup file.
    NoDevicesCgroup,
    /// No `cgroup2` mount.
    NoCgroup2Fs,
    /// No unified entry in the cgroup file.
    NoCgroupV2Entry,
    /// `read "<file>": bufio.Scanner: token too long`.
    Read {
        file: &'static str,
        source: TokenTooLong,
    },
}

impl fmt::Display for ParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MalformedCgroupEntry(line) => write!(formatter, "malformed cgroup entry: {line}"),
            Self::MalformedMountinfoEntry(line) => {
                write!(formatter, "malformed mountinfo entry: {line}")
            }
            Self::RelativeMountPrefix(prefix) => {
                write!(formatter, "relative path in mount prefix: {prefix}")
            }
            Self::NoDeviceOrUnifiedCgroup => {
                formatter.write_str("no devices or unified cgroup entries found")
            }
            // NVIDIA's spelling, kept: the text is the 1.x contract.
            Self::NoCgroupV1Fs => formatter.write_str(
                "no cgroup filesystem mounted for the devices subsytem in mountinfo file",
            ),
            Self::NoDevicesCgroup => formatter.write_str("no devices cgroup entries found"),
            Self::NoCgroup2Fs => formatter.write_str("no cgroup2 filesystem in mountinfo file"),
            Self::NoCgroupV2Entry => formatter.write_str("no cgroupv2 entries in file"),
            Self::Read { file, source } => write!(formatter, "read {}: {source}", quote(file)),
        }
    }
}

impl std::error::Error for ParseError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Read { source, .. } => Some(source),
            Self::MalformedCgroupEntry(_)
            | Self::MalformedMountinfoEntry(_)
            | Self::RelativeMountPrefix(_)
            | Self::NoDeviceOrUnifiedCgroup
            | Self::NoCgroupV1Fs
            | Self::NoDevicesCgroup
            | Self::NoCgroup2Fs
            | Self::NoCgroupV2Entry => None,
        }
    }
}

/// The names the parse errors give the two files, as Go's `ParseProcCgroup` does.
pub(super) const CGROUP_FILE: &str = "cgroup";
pub(super) const MOUNTINFO_FILE: &str = "mountinfo";

/// Resolves the device cgroup from the contents of a process's `cgroup` and `mountinfo` files.
/// Taking contents rather than a pid lets the caller read them through a `/proc/<pid>` descriptor
/// it has pinned.
///
/// # Errors
///
/// [`ParseError`] when the files do not say where the device cgroup is.
pub fn parse_proc_cgroup(cgroup: &[u8], mountinfo: &[u8]) -> Result<ProcCgroup, ParseError> {
    let version = scan_cgroup_version(cgroup, CGROUP_FILE)?;
    let (mount_prefix, mount_point, root) = match version {
        CgroupVersion::V1 => {
            let (prefix, point) = v1::scan_mount_info_v1(mountinfo, MOUNTINFO_FILE)?;
            let root = v1::scan_proc_cgroup_v1(cgroup, CGROUP_FILE, &prefix)?;
            (prefix, point, root)
        }
        CgroupVersion::V2 => {
            let (prefix, point) = v2::scan_mount_info_v2(mountinfo, MOUNTINFO_FILE)?;
            let root = v2::scan_proc_cgroup_v2(cgroup, CGROUP_FILE, &prefix)?;
            (prefix, point, root)
        }
    };
    Ok(ProcCgroup {
        version,
        mount_prefix,
        mount_point,
        root,
    })
}

/// The lines of a `/proc` file, as Go's `bufio.Scanner` reads them, as text.
pub(super) fn scan_lines<'data>(
    data: &'data [u8],
    file: &'static str,
) -> impl Iterator<Item = Result<String, ParseError>> + 'data {
    scanner::lines(data).map(move |line| {
        line.map(|line| String::from_utf8_lossy(line).into_owned())
            .map_err(|source| ParseError::Read { file, source })
    })
}

/// Which cgroup version the cgroup file describes: v1 when it has a `devices` entry (a hybrid host
/// included), v2 when it has a unified one. Every line must have two colons.
pub(super) fn scan_cgroup_version(
    data: &[u8],
    file: &'static str,
) -> Result<CgroupVersion, ParseError> {
    let mut devices = false;
    let mut unified = false;
    for line in scan_lines(data, file) {
        let line = line?;
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        let [_, controllers, _] = parts.as_slice() else {
            return Err(ParseError::MalformedCgroupEntry(line));
        };
        devices |= *controllers == "devices";
        unified |= controllers.is_empty();
    }
    if devices {
        Ok(CgroupVersion::V1)
    } else if unified {
        Ok(CgroupVersion::V2)
    } else {
        Err(ParseError::NoDeviceOrUnifiedCgroup)
    }
}

#[cfg(test)]
mod tests;
