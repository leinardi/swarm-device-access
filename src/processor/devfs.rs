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

//! The `/dev` the daemon sees, opened by descriptor: `openat2` beneath a `/dev` directory
//! descriptor with `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS`, so no symlink step leaves it; the
//! type and device number from `fstat` on the descriptor; and the device's canonical name from
//! sysfs. The [`DevFs`] trait is the seam that lets unit tests script identities, since an
//! unprivileged test cannot `mknod`. Go's `devfs.go`.
//!
//! Names are text: a name that is not UTF-8 is read lossily, which can only make it name nothing
//! (a dangling candidate) or fail a policy glob it would otherwise have matched.

use std::fmt;
use std::fs::File;
use std::io::{self, Read};
use std::os::fd::{AsRawFd, OwnedFd};
use std::os::unix::ffi::OsStrExt;

use rustix::fs::{FileType, Mode, OFlags, ResolveFlags};
use rustix::io::Errno;

use crate::go_compat::errno::errno_string;
use crate::go_compat::filepath;

/// Where the daemon sees the host's `/dev` (bind-mounted in).
pub const DEV_ROOT: &str = "/dev";

/// A failed system call, with the operation and name Go's error carried before the errno: `openat2
/// dri/card0: permission denied`, or the errno alone when there is no context.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SysError {
    context: String,
    errno: Errno,
}

impl SysError {
    /// The failure of `context` with `errno`.
    #[must_use]
    pub fn new(context: impl Into<String>, errno: Errno) -> Self {
        Self {
            context: context.into(),
            errno,
        }
    }

    /// The failure of `context` with `error`'s errno (`EIO` for an error that has none).
    #[must_use]
    pub fn from_io(context: impl Into<String>, error: &io::Error) -> Self {
        Self::new(context, Errno::from_io_error(error).unwrap_or(Errno::IO))
    }

    /// The errno.
    #[must_use]
    pub const fn errno(&self) -> Errno {
        self.errno
    }
}

impl fmt::Display for SysError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let errno = errno_string(self.errno.raw_os_error());
        if self.context.is_empty() {
            formatter.write_str(&errno)
        } else {
            write!(formatter, "{}: {errno}", self.context)
        }
    }
}

impl std::error::Error for SysError {}

/// A device's type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DeviceType {
    Char,
    Block,
}

impl DeviceType {
    /// The letter of a device rule: `c` or `b`.
    #[must_use]
    pub const fn letter(self) -> &'static str {
        match self {
            Self::Char => "c",
            Self::Block => "b",
        }
    }

    /// The `/sys/dev` directory for the type.
    const fn sysfs_class(self) -> &'static str {
        match self {
            Self::Char => "char",
            Self::Block => "block",
        }
    }
}

/// A device number and its type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeviceId {
    pub kind: DeviceType,
    pub major: u32,
    pub minor: u32,
}

impl fmt::Display for DeviceId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{} {}:{}",
            self.kind.letter(),
            self.major,
            self.minor
        )
    }
}

/// What a resolved `/dev` entry is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    /// A character or block device.
    Device(DeviceId),
    Dir,
    Other,
}

/// A `/dev` entry opened by descriptor: its identity comes from the descriptor, never from a
/// second lookup of the path. The descriptor closes when the node is dropped.
pub trait DevNode {
    /// The node's type and, for a device, its number.
    ///
    /// # Errors
    ///
    /// [`SysError`] when `fstat` fails.
    fn stat(&self) -> Result<NodeKind, SysError>;

    /// The `/dev` path the descriptor refers to, informational and for policy only; empty when
    /// it is not under `/dev`.
    fn resolved(&self) -> String;
}

/// What a walk does after a visit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WalkControl {
    Continue,
    /// Go's `filepath.SkipAll`: the walk ends.
    SkipAll,
}

/// A walk's visitor: Go's `fs.WalkDirFunc`, called with the path, whether it is a directory, and
/// the error that stopped the walk from reading it.
pub type Visit<'visit> = dyn FnMut(&str, bool, Option<SysError>) -> WalkControl + 'visit;

/// Resolves names under `/dev` without leaving it.
pub trait DevFs {
    /// The directory that stands for `/dev`, for enumerating names.
    fn root(&self) -> &str;

    /// Opens the entry at `rel` (relative to `/dev`), following symlinks but refusing any step
    /// outside `/dev` (`EXDEV`).
    ///
    /// # Errors
    ///
    /// [`SysError`] when the lookup fails.
    fn open_beneath(&self, rel: &str) -> Result<Box<dyn DevNode>, SysError>;

    /// The target of the symlink at `rel` itself.
    ///
    /// # Errors
    ///
    /// [`SysError`] when it is not a symlink or cannot be read.
    fn read_link(&self, rel: &str) -> Result<String, SysError>;

    /// The sysfs uevent of a device.
    ///
    /// # Errors
    ///
    /// [`SysError`] when it cannot be read.
    fn read_uevent(&self, id: DeviceId) -> Result<Vec<u8>, SysError>;

    /// Enumerates the names under `base` (a path under [`DevFs::root`]), as Go's
    /// `filepath.WalkDir`. It is for names only: every name is then resolved with
    /// [`DevFs::open_beneath`].
    fn walk(&self, base: &str, visit: &mut Visit<'_>);
}

/// The production [`DevFs`]: `openat2` beneath a `/dev` descriptor opened once per pass, and
/// sysfs under `sysfs_root`.
#[derive(Debug)]
pub struct RealDevFs {
    dir: String,
    fd: OwnedFd,
    sysfs_root: String,
}

impl RealDevFs {
    /// Opens `dir` (the `/dev` directory) for one pass.
    ///
    /// # Errors
    ///
    /// [`SysError`] (`open <dir>: <errno>`) when it cannot be opened.
    pub fn open(dir: &str, sysfs_root: &str) -> Result<Self, SysError> {
        let fd = rustix::fs::open(
            dir,
            OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(|errno| SysError::new(format!("open {dir}"), errno))?;
        Ok(Self {
            dir: dir.to_owned(),
            fd,
            sysfs_root: sysfs_root.to_owned(),
        })
    }

    /// Opens `rel` beneath the `/dev` descriptor with `O_PATH`, which never opens the device
    /// driver. The kernel follows symlinks but refuses, as one atomic lookup, any step that
    /// leaves `/dev`: an absolute target, `..` past the root, or a magic link.
    fn openat2(&self, rel: &str, flags: OFlags) -> Result<OwnedFd, SysError> {
        rustix::fs::openat2(
            &self.fd,
            rel,
            OFlags::PATH | OFlags::CLOEXEC | flags,
            Mode::empty(),
            ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
        )
        .map_err(|errno| SysError::new(format!("openat2 {rel}"), errno))
    }
}

impl DevFs for RealDevFs {
    fn root(&self) -> &str {
        &self.dir
    }

    fn open_beneath(&self, rel: &str) -> Result<Box<dyn DevNode>, SysError> {
        let fd = self.openat2(rel, OFlags::empty())?;
        Ok(Box::new(RealDevNode {
            fd,
            dir: self.dir.clone(),
        }))
    }

    fn read_link(&self, rel: &str) -> Result<String, SysError> {
        let link = self.openat2(rel, OFlags::NOFOLLOW)?;
        let target = rustix::fs::readlinkat(&link, "", Vec::new())
            .map_err(|errno| SysError::new(format!("readlink {rel}"), errno))?;
        Ok(String::from_utf8_lossy(target.as_bytes()).into_owned())
    }

    fn read_uevent(&self, id: DeviceId) -> Result<Vec<u8>, SysError> {
        let number = format!("{}:{}", id.major, id.minor);
        let path = filepath::join(&[
            &self.sysfs_root,
            "dev",
            id.kind.sysfs_class(),
            &number,
            "uevent",
        ]);
        // Go's os.ReadFile error names its own operation after the path.
        let mut file = File::open(&path)
            .map_err(|error| SysError::from_io(format!("read {path}: open {path}"), &error))?;
        let mut data = Vec::new();
        file.read_to_end(&mut data)
            .map_err(|error| SysError::from_io(format!("read {path}: read {path}"), &error))?;
        Ok(data)
    }

    fn walk(&self, base: &str, visit: &mut Visit<'_>) {
        walk_dir(base, visit);
    }
}

/// A node opened by [`RealDevFs`].
#[derive(Debug)]
struct RealDevNode {
    fd: OwnedFd,
    dir: String,
}

impl DevNode for RealDevNode {
    fn stat(&self) -> Result<NodeKind, SysError> {
        let stat = rustix::fs::fstat(&self.fd).map_err(|errno| SysError::new("fstat", errno))?;
        let id = |kind| {
            NodeKind::Device(DeviceId {
                kind,
                major: rustix::fs::major(stat.st_rdev),
                minor: rustix::fs::minor(stat.st_rdev),
            })
        };
        Ok(match FileType::from_raw_mode(stat.st_mode) {
            FileType::CharacterDevice => id(DeviceType::Char),
            FileType::BlockDevice => id(DeviceType::Block),
            FileType::Directory => NodeKind::Dir,
            FileType::RegularFile
            | FileType::Symlink
            | FileType::Fifo
            | FileType::Socket
            | FileType::Unknown => NodeKind::Other,
        })
    }

    fn resolved(&self) -> String {
        let Ok(target) = std::fs::read_link(format!("/proc/self/fd/{}", self.fd.as_raw_fd()))
        else {
            return String::new();
        };
        let target = String::from_utf8_lossy(target.as_os_str().as_bytes()).into_owned();
        relative_beneath(&self.dir, &target)
            .map(|rel| filepath::join(&[DEV_ROOT, &rel]))
            .unwrap_or_default()
    }
}

/// `target` relative to `dir`, as Go's `filepath.Rel` makes it, when it is `dir` or below it;
/// anything else (a relative target, a target outside `dir`) is `None`.
fn relative_beneath(dir: &str, target: &str) -> Option<String> {
    if !target.starts_with('/') {
        return None;
    }
    let dir = filepath::clean(dir);
    let target = filepath::clean(target);
    if dir == target {
        return Some(".".to_owned());
    }
    let prefix = if dir == "/" { String::new() } else { dir };
    target
        .strip_prefix(&format!("{prefix}/"))
        .map(ToOwned::to_owned)
}

/// Go's `filepath.WalkDir`: `root`, then every name below it in lexical order, directories
/// before their contents. A root that cannot be `lstat`ed is visited with the error; a directory
/// that cannot be read is visited a second time with the error, then the entries read before it
/// are walked. Entry types come from the directory listing, as Go's `ReadDir` gives them.
pub fn walk_dir(root: &str, visit: &mut Visit<'_>) {
    match std::fs::symlink_metadata(root) {
        Err(error) => {
            visit(
                root,
                false,
                Some(SysError::from_io(format!("lstat {root}"), &error)),
            );
        }
        Ok(metadata) => {
            walk_entry(root, metadata.is_dir(), visit);
        }
    }
}

/// Walks one entry; [`WalkControl::SkipAll`] ends the whole walk.
fn walk_entry(path: &str, is_dir: bool, visit: &mut Visit<'_>) -> WalkControl {
    if visit(path, is_dir, None) == WalkControl::SkipAll {
        return WalkControl::SkipAll;
    }
    if !is_dir {
        return WalkControl::Continue;
    }
    let (entries, error) = read_dir_sorted(path);
    if let Some(error) = error
        && visit(path, true, Some(error)) == WalkControl::SkipAll
    {
        return WalkControl::SkipAll;
    }
    for (name, entry_is_dir) in entries {
        let child = filepath::join(&[path, &name]);
        if walk_entry(&child, entry_is_dir, visit) == WalkControl::SkipAll {
            return WalkControl::SkipAll;
        }
    }
    WalkControl::Continue
}

/// Go's `os.ReadDir`: the entries sorted by name, and the error that stopped the listing, with
/// the entries read before it.
fn read_dir_sorted(path: &str) -> (Vec<(String, bool)>, Option<SysError>) {
    let reader = match std::fs::read_dir(path) {
        Ok(reader) => reader,
        Err(error) => {
            return (
                Vec::new(),
                Some(SysError::from_io(format!("open {path}"), &error)),
            );
        }
    };
    let mut entries = Vec::new();
    let mut failure = None;
    for entry in reader {
        match entry {
            Ok(entry) => {
                let name = String::from_utf8_lossy(entry.file_name().as_bytes()).into_owned();
                let is_dir = entry.file_type().is_ok_and(|kind| kind.is_dir());
                entries.push((name, is_dir));
            }
            Err(error) => {
                failure = Some(SysError::from_io(format!("readdirent {path}"), &error));
                break;
            }
        }
    }
    entries.sort_by(|left, right| left.0.cmp(&right.0));
    (entries, failure)
}

/// Where sysfs is read from: `<host_root>/sys`, the documented mount, when it has device entries,
/// otherwise `/sys` (a daemon run straight on the host). `/sys/dev` lists the kernel's devices
/// whatever the mount namespace, so both give the same answer.
#[must_use]
pub fn sysfs_root_for(host_root: &str) -> String {
    let mounted = filepath::join(&[host_root, "sys"]);
    if std::fs::metadata(filepath::join(&[&mounted, "dev"])).is_ok() {
        mounted
    } else {
        "/sys".to_owned()
    }
}

/// Fails when the kernel cannot resolve paths beneath a directory (`openat2` with
/// `RESOLVE_BENEATH`, Linux 5.6). Without it no device path can be contained to `/dev`, so the
/// daemon refuses to start.
///
/// # Errors
///
/// [`ProbeError`] when `openat2` is missing or fails.
pub fn probe_openat2() -> Result<(), ProbeError> {
    match rustix::fs::openat2(
        rustix::fs::CWD,
        ".",
        OFlags::PATH | OFlags::CLOEXEC,
        Mode::empty(),
        ResolveFlags::BENEATH | ResolveFlags::NO_MAGICLINKS,
    ) {
        Ok(_) => Ok(()),
        Err(errno) if errno == Errno::NOSYS || errno == Errno::INVAL => {
            Err(ProbeError::Unsupported(errno))
        }
        Err(errno) => Err(ProbeError::Probe(errno)),
    }
}

/// Why the `openat2` probe failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProbeError {
    /// Go's `errOpenat2Unsupported`: the kernel has no `openat2` with `RESOLVE_BENEATH`.
    Unsupported(Errno),
    /// The probe failed otherwise.
    Probe(Errno),
}

impl fmt::Display for ProbeError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unsupported(errno) => write!(
                formatter,
                "openat2 with RESOLVE_BENEATH is not supported; Linux 5.6 or newer is required: {}",
                errno_string(errno.raw_os_error())
            ),
            Self::Probe(errno) => write!(
                formatter,
                "probe openat2: {}",
                errno_string(errno.raw_os_error())
            ),
        }
    }
}

impl std::error::Error for ProbeError {}

#[cfg(test)]
mod tests;
