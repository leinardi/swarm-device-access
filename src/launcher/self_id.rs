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

// Adapted from swarm-rootless-docker/src/launcher/mod.rs at 16913a4d87d8,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The launcher's own container ID, from its mount table: Go's `selfIDFromMountinfo` and
//! `parseSelfID`. Docker bind-mounts each container's `hostname`, `hosts` and `resolv.conf`
//! from `…/containers/<ID>/` onto `/etc`, whatever its data root (`/var/lib/docker`,
//! `/mnt/.ix-apps/docker`, …).
//!
//! The table is read as Go's `bufio.Scanner` reads it: line by line, a trailing `\r` dropped,
//! fields split on white space, and a line of 64 KiB or more an error.

use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};

use crate::go_compat::errno::io_error_text;

/// The launcher's own mount table.
pub const SELF_MOUNTINFO: &str = "/proc/self/mountinfo";

/// Where Docker mounts the per-container files.
const CONTAINER_FILE_TARGETS: [&str; 3] = ["/etc/hostname", "/etc/hosts", "/etc/resolv.conf"];

/// The per-container files, as the last element of the mount's root.
const CONTAINER_FILES: [&str; 3] = ["hostname", "hosts", "resolv.conf"];

/// The fields of a mountinfo line up to the mount point: mount ID, parent ID, major:minor, root,
/// mount point.
const MOUNTINFO_FIELDS: usize = 5;

/// The length of a container ID.
const CONTAINER_ID_LEN: usize = 64;

/// `bufio.MaxScanTokenSize`: the longest line, its newline included, a default scanner reads.
const MAX_SCAN_TOKEN_SIZE: usize = 64 * 1024;

/// Why the launcher could not identify its own container.
#[derive(Debug)]
pub enum SelfIdError {
    /// The mount table could not be opened: `open /proc/self/mountinfo: <Go's PathError>`.
    Open(io::Error),
    /// Reading it failed: `read mount table: <cause>`.
    Read(String),
    /// Go's `errSelfNotFound`.
    NotFound,
    /// Go's `errSelfAmbiguous`.
    Ambiguous,
}

impl fmt::Display for SelfIdError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            // Go wraps the os.Open PathError, which names the path itself.
            Self::Open(error) => write!(
                formatter,
                "open {SELF_MOUNTINFO}: open {SELF_MOUNTINFO}: {}",
                io_error_text(error)
            ),
            Self::Read(cause) => write!(formatter, "read mount table: {cause}"),
            Self::NotFound => formatter.write_str(
                "no Docker container ID in the mount table; is the launcher running in a \
                 container?",
            ),
            Self::Ambiguous => {
                formatter.write_str("more than one Docker container ID in the mount table")
            }
        }
    }
}

impl std::error::Error for SelfIdError {}

/// Go's `selfIDFromMountinfo`: the ID of the container whose mount table this is.
///
/// # Errors
///
/// [`SelfIdError`] when the table cannot be read, names no container, or names two.
pub fn self_id_from_mountinfo() -> Result<String, SelfIdError> {
    let mut file = File::open(SELF_MOUNTINFO).map_err(SelfIdError::Open)?;
    let mut table = Vec::new();
    file.read_to_end(&mut table)
        .map_err(|error| SelfIdError::Read(read_error_text(&error)))?;
    parse_self_id(&table)
}

/// Go's `read /proc/self/mountinfo` `PathError` of a read that failed after the open.
fn read_error_text(error: &io::Error) -> String {
    format!("read {SELF_MOUNTINFO}: {}", io_error_text(error))
}

/// Go's `parseSelfID`: the container ID in the root of the mounts at `/etc/hostname`,
/// `/etc/hosts` and `/etc/resolv.conf`. No match, or two different IDs, is an error: the
/// launcher must never guess which image it runs.
///
/// # Errors
///
/// [`SelfIdError`] when a line is too long, no container is named, or two are.
pub fn parse_self_id(table: &[u8]) -> Result<String, SelfIdError> {
    let mut found: Option<String> = None;
    for line in scan_lines(table)? {
        let text = String::from_utf8_lossy(line);
        let fields: Vec<&str> = text.split_whitespace().collect();
        if fields.len() < MOUNTINFO_FIELDS || !CONTAINER_FILE_TARGETS.contains(&fields[4]) {
            continue;
        }
        let Some(id) = container_file_owner(fields[3]) else {
            continue;
        };
        match &found {
            Some(earlier) if earlier != id => return Err(SelfIdError::Ambiguous),
            Some(_) | None => found = Some(id.to_owned()),
        }
    }
    found.ok_or(SelfIdError::NotFound)
}

/// `bufio.ScanLines` over `table`: the lines without their `\n` and a final `\r`, and Go's
/// `token too long` for one that does not fit the scanner's buffer.
fn scan_lines(table: &[u8]) -> Result<Vec<&[u8]>, SelfIdError> {
    let mut lines = Vec::new();
    let mut rest = table;
    while !rest.is_empty() {
        let newline = rest.iter().position(|byte| *byte == b'\n');
        let length = newline.unwrap_or(rest.len());
        if length >= MAX_SCAN_TOKEN_SIZE {
            return Err(SelfIdError::Read(
                "bufio.Scanner: token too long".to_owned(),
            ));
        }
        let line = &rest[..length];
        lines.push(line.strip_suffix(b"\r").unwrap_or(line));
        rest = &rest[newline.map_or(rest.len(), |end| end + 1)..];
    }
    Ok(lines)
}

/// Go's `containerFileRoot`, `/containers/([0-9a-f]{64})/(?:hostname|hosts|resolv\.conf)$`: the
/// container ID of a root ending in a per-container file.
fn container_file_owner(root: &str) -> Option<&str> {
    let mut parts = root.rsplitn(4, '/');
    let file = parts.next()?;
    let id = parts.next()?;
    let containers = parts.next()?;
    // What precedes `/containers/`, possibly empty; the slash before it must be there.
    parts.next()?;
    let is_id = id.len() == CONTAINER_ID_LEN
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    (CONTAINER_FILES.contains(&file) && containers == "containers" && is_id).then_some(id)
}

#[cfg(test)]
mod tests;
