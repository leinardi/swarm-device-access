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

// Modified by Roberto Leinardi in 2026: ported to Rust from internal/cgroup/v1.go.

//! cgroup v1: where the `devices` hierarchy is mounted and the process's cgroup in it, and the
//! controller ([`CgroupV1`]) whose `set_device_rules` is in `v1set.rs`.

use std::sync::Arc;

use super::api::{DeviceRule, Interface, ParseError, scan_lines};
use super::error::Error;
use super::handle::CgroupHandle;
use super::ledger::Ledger;
use super::v1set::{self, OpenatFiles, V1Files};
use crate::go_compat::filepath;

/// The cgroup v1 device controller: `devices.allow` and `devices.deny` written through the
/// cgroup's directory descriptor, with the [`Ledger`] of the exceptions the daemon added.
#[derive(Debug)]
pub struct CgroupV1 {
    pub(super) ledger: Arc<Ledger>,
    pub(super) files: Box<dyn V1Files>,
}

impl CgroupV1 {
    /// The controller recording its grants in `ledger`, which must outlive it (one per
    /// processor), as Go's `New(1, ledger, cache)` makes it.
    #[must_use]
    pub fn new(ledger: Arc<Ledger>) -> Self {
        Self {
            ledger,
            files: Box::new(OpenatFiles),
        }
    }
}

impl Interface for CgroupV1 {
    fn set_device_rules(&self, handle: &CgroupHandle, rules: &[DeviceRule]) -> Result<(), Error> {
        v1set::set_device_rules(self, handle, rules)
    }
}

/// The mountinfo entry of the cgroup v1 `devices` hierarchy: the mount's root (prefix) and its
/// mount point. The filesystem type is the third field from the end, and the mount point's last
/// element must be `devices`.
pub(super) fn scan_mount_info_v1(
    data: &[u8],
    file: &'static str,
) -> Result<(String, String), ParseError> {
    for line in scan_lines(data, file) {
        let line = line?;
        // Split on single spaces, as Go's strings.Split: empty fields count.
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() < 5 {
            return Err(ParseError::MalformedMountinfoEntry(line));
        }
        if parts[parts.len() - 3] != "cgroup" {
            continue;
        }
        if filepath::base(parts[4]) != "devices" {
            continue;
        }
        // The mount prefix must not be a relative path.
        if parts[3].starts_with("/..") {
            return Err(ParseError::RelativeMountPrefix(parts[3].to_owned()));
        }
        return Ok((parts[3].to_owned(), parts[4].to_owned()));
    }
    Err(ParseError::NoCgroupV1Fs)
}

/// The process's cgroup in the `devices` hierarchy, with `prefix` stripped unless it is `/`.
pub(super) fn scan_proc_cgroup_v1(
    data: &[u8],
    file: &'static str,
    prefix: &str,
) -> Result<String, ParseError> {
    for line in scan_lines(data, file) {
        let line = line?;
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        let [_, controllers, root] = parts.as_slice() else {
            return Err(ParseError::MalformedCgroupEntry(line));
        };
        if *controllers != "devices" {
            continue;
        }
        if prefix == "/" {
            return Ok((*root).to_owned());
        }
        return Ok(root.strip_prefix(prefix).unwrap_or(root).to_owned());
    }
    Err(ParseError::NoDevicesCgroup)
}

#[cfg(test)]
mod tests;
