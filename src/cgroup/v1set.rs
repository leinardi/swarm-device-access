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

//! cgroup v1's `set_device_rules`: the daemon-owned exceptions of `devices.list` made equal to the
//! desired rules through `devices.allow` and `devices.deny`.
//!
//! The runtime's own exceptions (the baseline) are what `devices.list` shows minus the bits the
//! ledger says this daemon added. Only bits outside the baseline become owned, so a grant the
//! runtime already made is never recorded and therefore never revoked. Revocations are written
//! before grants and a failed revocation stops the call: going from A to B with a failed revoke of
//! A must not end as A plus B. Every successful write updates the ledger at once, so a partial
//! failure leaves it matching the files, and a retry recomputes from `devices.list`.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::File;
use std::io::{Read as _, Write as _};
use std::os::fd::BorrowedFd;

use rustix::fs::{Mode, OFlags};

use super::api::DeviceRule;
use super::error::{Error, FileError, ListEntryCause};
use super::handle::CgroupHandle;
use super::v1::CgroupV1;
use crate::go_compat::parse_int::parse_int;
use crate::go_compat::scanner;
use crate::go_compat::strings::{fields, trim_space};

/// The access bits of a cgroup v1 device exception.
pub(super) const ACC_READ: u8 = 1;
pub(super) const ACC_WRITE: u8 = 1 << 1;
pub(super) const ACC_MKNOD: u8 = 1 << 2;
const ACCESS_BITS: [u8; 3] = [ACC_READ, ACC_WRITE, ACC_MKNOD];

pub(super) const DEVICES_LIST: &str = "devices.list";
pub(super) const DEVICES_ALLOW: &str = "devices.allow";
pub(super) const DEVICES_DENY: &str = "devices.deny";

/// `*` in a major or minor number.
pub(super) const WILDCARD: i64 = -1;

/// One access bit of one device: the unit the ledger owns. The derived order (type, major, minor,
/// access) is Go's `compareBits`, so writes go out in Go's order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct DevBit {
    /// `b'c'` or `b'b'`.
    pub(super) kind: u8,
    pub(super) major: i64,
    pub(super) minor: i64,
    /// Exactly one of the access bits.
    pub(super) access: u8,
}

impl DevBit {
    /// The `devices.allow` and `devices.deny` line: `c 1:3 r`.
    pub(super) fn line(self) -> String {
        format!(
            "{} {}:{} {}",
            char::from(self.kind),
            self.major,
            self.minor,
            access_string(self.access)
        )
    }
}

/// One `devices.list` line; `kind` may be `b'a'` (all) and a number may be [`WILDCARD`]. `access`
/// is a mask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct ListEntry {
    pub(super) kind: u8,
    pub(super) major: i64,
    pub(super) minor: i64,
    pub(super) access: u8,
}

impl ListEntry {
    pub(super) fn covers(self, bit: DevBit) -> bool {
        (self.kind == b'a' || self.kind == bit.kind)
            && (self.major == WILDCARD || self.major == bit.major)
            && (self.minor == WILDCARD || self.minor == bit.minor)
            && self.access & bit.access != 0
    }
}

pub(super) fn covered(entries: &[ListEntry], bit: DevBit) -> bool {
    entries.iter().any(|entry| entry.covers(bit))
}

pub(super) fn access_string(mask: u8) -> String {
    [(ACC_READ, 'r'), (ACC_WRITE, 'w'), (ACC_MKNOD, 'm')]
        .iter()
        .filter(|(bit, _)| mask & bit != 0)
        .map(|(_, letter)| letter)
        .collect()
}

/// The access mask of `rwm` letters; the first other letter is the error.
fn parse_access(access: &str) -> Result<u8, char> {
    access.chars().try_fold(0, |mask, letter| match letter {
        'r' => Ok(mask | ACC_READ),
        'w' => Ok(mask | ACC_WRITE),
        'm' => Ok(mask | ACC_MKNOD),
        _ => Err(letter),
    })
}

fn parse_num(field: &str) -> Result<i64, ListEntryCause> {
    if field == "*" {
        return Ok(WILDCARD);
    }
    parse_int(field, 10).map_err(|source| ListEntryCause::Number {
        field: field.to_owned(),
        source,
    })
}

/// Parses `devices.list` (`c 1:3 rwm`, `b 8:* r`, `a *:* rwm`).
pub(super) fn parse_devices_list(data: &[u8]) -> Result<Vec<ListEntry>, Error> {
    let mut entries = Vec::new();
    for line in scanner::lines(data) {
        let text = String::from_utf8_lossy(line.map_err(Error::ReadList)?);
        let line = trim_space(&text);
        if line.is_empty() {
            continue;
        }
        let malformed = |cause| Error::ListEntry {
            line: line.to_owned(),
            cause,
        };
        let parts = fields(line);
        let [kind, numbers, access] = parts.as_slice() else {
            return Err(malformed(None));
        };
        if !matches!(*kind, "a" | "b" | "c") {
            return Err(malformed(None));
        }
        let numbers: Vec<&str> = numbers.split(':').collect();
        let [major, minor] = numbers.as_slice() else {
            return Err(malformed(None));
        };
        let major = parse_num(major).map_err(|cause| malformed(Some(cause)))?;
        let minor = parse_num(minor).map_err(|cause| malformed(Some(cause)))?;
        let access = parse_access(access)
            .map_err(|letter| malformed(Some(ListEntryCause::Access(letter))))?;
        entries.push(ListEntry {
            kind: kind.as_bytes()[0],
            major,
            minor,
            access,
        });
    }
    Ok(entries)
}

/// `entries` with the owned bits removed from the exact (non-wildcard) entries they appear in.
/// `devices.list` merges the runtime's access bits and ours into one line per device, so this is
/// what separates the runtime's baseline from the daemon's grants. Wildcard entries are always
/// baseline: the daemon never writes one.
pub(super) fn without_owned(entries: &[ListEntry], owned: &BTreeSet<DevBit>) -> Vec<ListEntry> {
    entries
        .iter()
        .map(|entry| {
            let mut entry = *entry;
            if entry.kind != b'a' && entry.major != WILDCARD && entry.minor != WILDCARD {
                for bit in owned {
                    if bit.kind == entry.kind
                        && bit.major == entry.major
                        && bit.minor == entry.minor
                    {
                        entry.access &= !bit.access;
                    }
                }
            }
            entry
        })
        .filter(|entry| entry.access != 0)
        .collect()
}

/// The allow rules as single-access-bit units.
fn expand_rules(rules: &[DeviceRule]) -> Result<BTreeSet<DevBit>, Error> {
    let mut bits = BTreeSet::new();
    for rule in rules {
        if !rule.allow {
            return Err(Error::DenyRule);
        }
        let (Some(major), Some(minor)) = (rule.major, rule.minor) else {
            return Err(Error::ConcreteRule(rule.go_struct_text()));
        };
        if !matches!(rule.kind.as_str(), "c" | "b") || major < 0 || minor < 0 {
            return Err(Error::ConcreteRule(rule.go_struct_text()));
        }
        let access = parse_access(&rule.access).map_err(Error::UnknownAccess)?;
        for one in ACCESS_BITS {
            if access & one != 0 {
                bits.insert(DevBit {
                    kind: rule.kind.as_bytes()[0],
                    major,
                    minor,
                    access: one,
                });
            }
        }
    }
    Ok(bits)
}

/// Reads and writes the `devices.*` files relative to a cgroup directory descriptor: the seam the
/// tests inject failures through.
pub(crate) trait V1Files: Send + Sync + fmt::Debug {
    fn read(&self, dir: BorrowedFd<'_>, name: &str) -> Result<Vec<u8>, FileError>;
    fn write(&self, dir: BorrowedFd<'_>, name: &str, line: &str) -> Result<(), FileError>;
}

/// The production [`V1Files`]: every access is `openat` on the directory descriptor, never a path
/// lookup.
#[derive(Debug)]
pub(super) struct OpenatFiles;

impl V1Files for OpenatFiles {
    fn read(&self, dir: BorrowedFd<'_>, name: &str) -> Result<Vec<u8>, FileError> {
        let fd = rustix::fs::openat(dir, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|errno| FileError::Open {
                name: name.to_owned(),
                source: errno.into(),
            })?;
        let mut data = Vec::new();
        File::from(fd)
            .read_to_end(&mut data)
            .map_err(|source| FileError::Read {
                name: name.to_owned(),
                source,
            })?;
        Ok(data)
    }

    fn write(&self, dir: BorrowedFd<'_>, name: &str, line: &str) -> Result<(), FileError> {
        let fd = rustix::fs::openat(dir, name, OFlags::WRONLY | OFlags::CLOEXEC, Mode::empty())
            .map_err(|errno| FileError::Open {
                name: name.to_owned(),
                source: errno.into(),
            })?;
        File::from(fd)
            .write_all(line.as_bytes())
            .map_err(|source| FileError::Write {
                line: line.to_owned(),
                name: name.to_owned(),
                source,
            })
    }
}

fn read_list(cgroup: &CgroupV1, dir: BorrowedFd<'_>) -> Result<Vec<ListEntry>, Error> {
    let data = cgroup.files.read(dir, DEVICES_LIST).map_err(Error::File)?;
    parse_devices_list(&data)
}

/// Makes the daemon-owned device exceptions of the cgroup equal exactly `rules` (the module
/// comment says how).
pub(super) fn set_device_rules(
    cgroup: &CgroupV1,
    handle: &CgroupHandle,
    rules: &[DeviceRule],
) -> Result<(), Error> {
    let desired = expand_rules(rules)?;
    let id = handle.identity();
    let entries = read_list(cgroup, handle.dir())?;
    let owned_old = cgroup.ledger.owned(id);
    let baseline = without_owned(&entries, &owned_old);
    let owned_new: BTreeSet<DevBit> = desired
        .into_iter()
        .filter(|bit| !covered(&baseline, *bit))
        .collect();

    // Owned bits that the baseline now covers by itself (the runtime granted them since) are
    // forgotten, not denied: denying them would take away the runtime's grant.
    let mut forgotten = BTreeSet::new();
    for bit in &owned_old {
        if owned_new.contains(bit) {
            continue;
        }
        if covered(&baseline, *bit) {
            cgroup.ledger.remove(id, *bit);
            forgotten.insert(*bit);
            continue;
        }
        cgroup
            .files
            .write(handle.dir(), DEVICES_DENY, &bit.line())
            .map_err(|source| Error::Revoke {
                line: bit.line(),
                source,
            })?;
        cgroup.ledger.remove(id, *bit);
    }

    let mut errors = Vec::new();
    for bit in &owned_new {
        // Written whenever the bit is not effective now, not merely when the ledger lacks it: an
        // external writer may have removed an owned bit, and an identical set must restore it.
        if !covered(&entries, *bit)
            && let Err(source) = cgroup.files.write(handle.dir(), DEVICES_ALLOW, &bit.line())
        {
            errors.push(Error::Grant {
                line: bit.line(),
                source,
            });
            continue;
        }
        cgroup.ledger.add(id, *bit);
    }

    if let Err(error) = verify(cgroup, handle.dir(), &owned_old, &owned_new, &forgotten) {
        errors.push(error);
    }
    match errors.len() {
        0 => Ok(()),
        _ => Err(Error::Joined(errors)),
    }
}

/// Re-reads `devices.list` and checks that, among the bits the daemon owned before or owns now,
/// exactly the owned-now bits are effective.
fn verify(
    cgroup: &CgroupV1,
    dir: BorrowedFd<'_>,
    owned_old: &BTreeSet<DevBit>,
    owned_new: &BTreeSet<DevBit>,
    forgotten: &BTreeSet<DevBit>,
) -> Result<(), Error> {
    let after = read_list(cgroup, dir).map_err(|error| Error::Verify(Box::new(error)))?;
    let checked: BTreeSet<&DevBit> = owned_old
        .difference(forgotten)
        .chain(owned_new.iter())
        .collect();
    let mismatched: Vec<String> = checked
        .into_iter()
        .filter_map(|bit| {
            let want = owned_new.contains(bit);
            (covered(&after, *bit) != want).then(|| format!("{} (want {want})", bit.line()))
        })
        .collect();
    if mismatched.is_empty() {
        Ok(())
    } else {
        Err(Error::Drift(mismatched))
    }
}

#[cfg(test)]
mod tests;
