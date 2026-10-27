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

//! An open cgroup directory. The directory is resolved by path exactly once, in
//! [`CgroupHandle::open`]; every later read and write goes through the descriptor, so a container
//! that exits and a cgroup recreated at the same path between resolution and mutation cannot
//! receive the old container's rules (the descriptor still refers to the removed directory).

use std::fmt;
use std::fs::File;
use std::io::{self, Read as _};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};

use rustix::fs::{Mode, OFlags};

use crate::go_compat::errno::io_error_text;
use crate::go_compat::quote::quote;
use crate::go_compat::strings::fields;

/// One cgroup directory: its path and the inode of the directory. A cgroup removed and recreated
/// at the same path gets a new inode, so state keyed by `Identity` never carries over to the new
/// cgroup.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct Identity {
    pub path: String,
    pub inode: u64,
}

/// An open cgroup directory; the descriptor closes when the handle is dropped.
#[derive(Debug)]
pub struct CgroupHandle {
    fd: OwnedFd,
    pub(super) identity: Identity,
}

/// Why a cgroup directory could not be opened or read, worded as Go's `CgroupHandle`.
#[derive(Debug)]
pub enum HandleError {
    /// `open cgroup "<path>": <errno>`.
    Open { path: String, source: io::Error },
    /// `stat cgroup "<path>": <errno>`.
    Stat { path: String, source: io::Error },
    /// `open cgroup.procs of "<path>": <errno>`.
    OpenProcs { path: String, source: io::Error },
    /// `read cgroup.procs of "<path>": read cgroup.procs: <errno>`.
    ReadProcs { path: String, source: io::Error },
}

impl fmt::Display for HandleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Open { path, source } => {
                write!(
                    formatter,
                    "open cgroup {}: {}",
                    quote(path),
                    io_error_text(source)
                )
            }
            Self::Stat { path, source } => {
                write!(
                    formatter,
                    "stat cgroup {}: {}",
                    quote(path),
                    io_error_text(source)
                )
            }
            Self::OpenProcs { path, source } => write!(
                formatter,
                "open cgroup.procs of {}: {}",
                quote(path),
                io_error_text(source)
            ),
            // Go reads an os.File named cgroup.procs, whose PathError names it again.
            Self::ReadProcs { path, source } => write!(
                formatter,
                "read cgroup.procs of {}: read cgroup.procs: {}",
                quote(path),
                io_error_text(source)
            ),
        }
    }
}

impl std::error::Error for HandleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Open { source, .. }
            | Self::Stat { source, .. }
            | Self::OpenProcs { source, .. }
            | Self::ReadProcs { source, .. } => Some(source),
        }
    }
}

impl CgroupHandle {
    /// Opens the cgroup directory at `path` and records its identity from `fstat` on the same
    /// descriptor.
    ///
    /// # Errors
    ///
    /// [`HandleError`] when the directory cannot be opened or its inode read.
    pub fn open(path: &str) -> Result<Self, HandleError> {
        let fd = rustix::fs::open(
            path,
            OFlags::DIRECTORY | OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| HandleError::Open {
            path: path.to_owned(),
            source: errno.into(),
        })?;
        let stat = rustix::fs::fstat(&fd).map_err(|errno| HandleError::Stat {
            path: path.to_owned(),
            source: errno.into(),
        })?;
        Ok(Self {
            fd,
            identity: Identity {
                path: path.to_owned(),
                inode: stat.st_ino,
            },
        })
    }

    /// The path and inode the handle was opened with.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.identity
    }

    /// Whether `pid` is listed in the cgroup's `cgroup.procs`, read through the descriptor.
    ///
    /// # Errors
    ///
    /// [`HandleError`] when `cgroup.procs` cannot be opened or read.
    pub fn has_process(&self, pid: i64) -> Result<bool, HandleError> {
        let path = || self.identity.path.clone();
        let fd = rustix::fs::openat(
            &self.fd,
            "cgroup.procs",
            OFlags::RDONLY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| HandleError::OpenProcs {
            path: path(),
            source: errno.into(),
        })?;
        let mut data = Vec::new();
        File::from(fd)
            .read_to_end(&mut data)
            .map_err(|source| HandleError::ReadProcs {
                path: path(),
                source,
            })?;
        let pid = pid.to_string();
        Ok(fields(&String::from_utf8_lossy(&data))
            .iter()
            .any(|listed| *listed == pid))
    }

    /// The directory descriptor, for reads and writes relative to it.
    pub(super) fn dir(&self) -> BorrowedFd<'_> {
        self.fd.as_fd()
    }
}

#[cfg(test)]
mod tests;
