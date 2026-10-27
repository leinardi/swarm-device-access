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

// Modified by Roberto Leinardi in 2026: ported to Rust from internal/cgroup/v2.go.

//! cgroup v2: where the unified hierarchy is mounted and the process's cgroup in it.

use super::api::{ParseError, scan_lines};

/// The mountinfo entry of the unified hierarchy: the mount's root (prefix) and its mount point.
/// The filesystem type is the third field from the end.
pub(super) fn scan_mount_info_v2(
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
        if parts[parts.len() - 3] != "cgroup2" {
            continue;
        }
        // The mount prefix must not be a relative path.
        if parts[3].starts_with("/..") {
            return Err(ParseError::RelativeMountPrefix(parts[3].to_owned()));
        }
        return Ok((parts[3].to_owned(), parts[4].to_owned()));
    }
    Err(ParseError::NoCgroup2Fs)
}

/// The process's cgroup in the unified hierarchy, with `prefix` stripped unless it is `/`.
pub(super) fn scan_proc_cgroup_v2(
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
        if !controllers.is_empty() {
            continue;
        }
        if prefix == "/" {
            return Ok((*root).to_owned());
        }
        return Ok(root.strip_prefix(prefix).unwrap_or(root).to_owned());
    }
    Err(ParseError::NoCgroupV2Entry)
}

#[cfg(test)]
mod tests;
