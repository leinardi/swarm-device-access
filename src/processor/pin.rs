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

//! A container's process held by a pidfd, so the pid read from an inspect stays meaningful until
//! the mutation, and the process's cgroup read through one descriptor on its `/proc` directory.
//! Go's `pin.go`.
//!
//! Go asks whether the pinned process is alive with `pidfd_send_signal(fd, 0)`; rustix has no
//! signal 0, so the port polls the pidfd, which turns readable when the process exits. A zombie
//! counts as gone here where Go saw it as alive: stricter, and the reconcile retries.

use std::fmt;
use std::fs::File;
use std::io::Read;
use std::os::fd::OwnedFd;

use rustix::event::{PollFd, PollFlags, Timespec};
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use rustix::process::{Pid, PidfdFlags};

use super::devfs::SysError;
use crate::cgroup::{ParseError, ProcCgroup, parse_proc_cgroup};
use crate::go_compat::filepath;
use crate::go_compat::quote::quote;

/// A process held by a pidfd: while it is alive its pid cannot be reused, so a pid read once stays
/// meaningful. The pidfd closes when the value is dropped.
pub trait PinnedProcess: Send {
    /// Whether the pinned process has not exited.
    ///
    /// # Errors
    ///
    /// [`PinError::Gone`] when it has exited.
    fn alive(&self) -> Result<(), PinError>;
}

/// Opens pidfds. It is a seam: tests cannot pin arbitrary pids.
pub trait ProcessPinner: Send + Sync {
    /// Pins `pid`.
    ///
    /// # Errors
    ///
    /// [`PinError::Open`] when the pidfd cannot be opened.
    fn pin(&self, pid: i64) -> Result<Box<dyn PinnedProcess>, PinError>;
}

/// The production [`ProcessPinner`] (Linux 5.3+).
#[derive(Debug, Clone, Copy, Default)]
pub struct PidfdPinner;

impl ProcessPinner for PidfdPinner {
    fn pin(&self, pid: i64) -> Result<Box<dyn PinnedProcess>, PinError> {
        // A pid that is not a positive pid_t names no process; the kernel answers EINVAL to Go's
        // call for zero and negative pids (which rustix asserts against), and the inspect never
        // reports a larger one.
        let raw = i32::try_from(pid)
            .ok()
            .filter(|raw| *raw > 0)
            .and_then(Pid::from_raw);
        let fd = raw
            .ok_or(Errno::INVAL)
            .and_then(|raw| rustix::process::pidfd_open(raw, PidfdFlags::empty()))
            .map_err(|errno| PinError::Open { pid, errno })?;
        Ok(Box::new(Pidfd(fd)))
    }
}

/// A pidfd from [`PidfdPinner`].
#[derive(Debug)]
struct Pidfd(OwnedFd);

impl PinnedProcess for Pidfd {
    fn alive(&self) -> Result<(), PinError> {
        let mut fds = [PollFd::new(&self.0, PollFlags::IN)];
        let zero = Timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        match rustix::event::poll(&mut fds, Some(&zero)) {
            Ok(0) => Ok(()),
            // Readable: the process exited. Go's pidfd_send_signal then fails with ESRCH.
            Ok(_) => Err(PinError::Gone(Errno::SRCH)),
            Err(errno) => Err(PinError::Gone(errno)),
        }
    }
}

/// Why a process could not be pinned or is no longer alive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinError {
    /// `pidfd_open(<pid>): <errno>`.
    Open { pid: i64, errno: Errno },
    /// Go's `errPinnedProcessGone`: `pinned container process exited: <errno>`.
    Gone(Errno),
}

impl fmt::Display for PinError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { pid, errno } => {
                write!(
                    formatter,
                    "pidfd_open({pid}): {}",
                    SysError::new("", *errno)
                )
            }
            Self::Gone(errno) => write!(
                formatter,
                "pinned container process exited: {}",
                SysError::new("", *errno)
            ),
        }
    }
}

impl std::error::Error for PinError {}

/// Reads `cgroup` and `mountinfo` of `pid` through one descriptor on `/proc/<pid>`. The directory
/// descriptor refers to the process it was opened for, so a later pid reuse cannot redirect the
/// second read.
///
/// # Errors
///
/// [`ProcCgroupError`] when a file cannot be read or does not name the device cgroup.
pub fn read_proc_cgroup(proc_root: &str, pid: i64) -> Result<ProcCgroup, ProcCgroupError> {
    let dir_path = filepath::join(&[proc_root, "proc", &pid.to_string()]);
    let dir = rustix::fs::open(
        dir_path.as_str(),
        OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
        Mode::empty(),
    )
    .map_err(|errno| ProcCgroupError::OpenDir {
        path: dir_path,
        errno,
    })?;
    let cgroup = read_at(&dir, "cgroup")?;
    let mountinfo = read_at(&dir, "mountinfo")?;
    parse_proc_cgroup(&cgroup, &mountinfo).map_err(|source| ProcCgroupError::Parse { pid, source })
}

fn read_at(dir: &OwnedFd, name: &'static str) -> Result<Vec<u8>, ProcCgroupError> {
    let fd = rustix::fs::openat(dir, name, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty())
        .map_err(|errno| ProcCgroupError::Open { name, errno })?;
    let mut data = Vec::new();
    File::from(fd)
        .read_to_end(&mut data)
        .map_err(|error| ProcCgroupError::Read {
            name,
            errno: Errno::from_io_error(&error).unwrap_or(Errno::IO),
        })?;
    Ok(data)
}

/// Why a process's cgroup could not be read, worded as Go's errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcCgroupError {
    /// `open "<dir>": <errno>`.
    OpenDir { path: String, errno: Errno },
    /// `open <name>: <errno>`.
    Open { name: &'static str, errno: Errno },
    /// `read <name>: read <name>: <errno>`: Go wraps the `os.File` read error, which names the
    /// file itself.
    Read { name: &'static str, errno: Errno },
    /// `resolve cgroup of pid <pid>: <cause>`.
    Parse { pid: i64, source: ParseError },
}

impl fmt::Display for ProcCgroupError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OpenDir { path, errno } => write!(
                formatter,
                "open {}: {}",
                quote(path),
                SysError::new("", *errno)
            ),
            Self::Open { name, errno } => {
                write!(formatter, "open {name}: {}", SysError::new("", *errno))
            }
            Self::Read { name, errno } => write!(
                formatter,
                "read {name}: read {name}: {}",
                SysError::new("", *errno)
            ),
            Self::Parse { pid, source } => {
                write!(formatter, "resolve cgroup of pid {pid}: {source}")
            }
        }
    }
}

impl std::error::Error for ProcCgroupError {}

#[cfg(test)]
mod tests;
