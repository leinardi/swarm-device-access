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

//! The raw `bpf(2)` commands the cgroup v2 device filter needs, and the only module of the
//! workspace allowed `unsafe` (`docs/rust-port.md`, eBPF): no safe crate loads a program from raw
//! instructions, attaches it with `BPF_F_REPLACE` and dumps a foreign program's translated
//! instructions. Each command fills one `#[repr(C)]` attribute struct, laid out as the kernel's
//! `union bpf_attr` member and sized as cilium's `internal/sys` structs, which the Go daemon
//! passed; the tests pin every offset and size. The query is the exception: the Go daemon
//! declared only its first 32 bytes, but the kernel's member is 64, and since cgroup programs
//! moved onto mprog (Linux 6.16) the kernel writes `revision`, at offset 56, back whatever size it
//! is given, so the whole member is declared here, as cilium does.
//!
//! Every function here is safe to call: the pointers an attribute carries are taken from buffers
//! the function borrows for the length of the call, and every descriptor the kernel returns is
//! owned at once.
#![expect(
    unsafe_code,
    reason = "the raw bpf(2) commands of the device filter, which no safe crate offers"
)]

use std::ffi::{c_long, c_uint, c_void};
use std::io;
use std::os::fd::{AsRawFd as _, BorrowedFd, FromRawFd as _, OwnedFd};

use rustix::io::Errno;

const BPF_PROG_LOAD: c_long = 5;
const BPF_PROG_ATTACH: c_long = 8;
const BPF_PROG_DETACH: c_long = 9;
const BPF_PROG_GET_FD_BY_ID: c_long = 13;
const BPF_OBJ_GET_INFO_BY_FD: c_long = 15;
const BPF_PROG_QUERY: c_long = 16;

/// `BPF_PROG_TYPE_CGROUP_DEVICE`.
pub(super) const PROG_TYPE_CGROUP_DEVICE: u32 = 15;
/// `BPF_CGROUP_DEVICE`, the attach type.
const CGROUP_DEVICE: u32 = 6;
/// `BPF_F_REPLACE`: the attach replaces the program named by `replace_bpf_fd`.
const F_REPLACE: u32 = 4;

/// The length of a kernel object name, its terminating NUL included.
const OBJ_NAME_LEN: usize = 16;

/// The licence the wrappers are loaded under, as Go loaded them.
const LICENSE: &std::ffi::CStr = c"Apache";

/// The attribute of `BPF_PROG_QUERY`, the kernel's whole `query` member (cilium's
/// `ProgQueryAttr`): the kernel may write `revision` back.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgQueryAttr {
    target_fd: u32,
    attach_type: u32,
    query_flags: u32,
    attach_flags: u32,
    prog_ids: u64,
    prog_cnt: u32,
    _pad: u32,
    prog_attach_flags: u64,
    link_ids: u64,
    link_attach_flags: u64,
    revision: u64,
}

/// The attribute of `BPF_PROG_GET_FD_BY_ID`, cilium's `ProgGetFdByIdAttr`.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgGetFdByIdAttr {
    id: u32,
}

/// The attribute of `BPF_OBJ_GET_INFO_BY_FD`, cilium's `ObjGetInfoByFdAttr`.
#[repr(C)]
#[derive(Debug, Default)]
struct ObjGetInfoByFdAttr {
    bpf_fd: u32,
    info_len: u32,
    info: u64,
}

/// `struct bpf_prog_info`, cilium's `ProgInfo`. Only the fields the daemon reads are named.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgInfo {
    prog_type: u32,
    id: u32,
    tag: [u8; 8],
    jited_prog_len: u32,
    xlated_prog_len: u32,
    jited_prog_insns: u64,
    xlated_prog_insns: u64,
    load_time: u64,
    created_by_uid: u32,
    nr_map_ids: u32,
    map_ids: u64,
    name: [u8; OBJ_NAME_LEN],
    /// From `ifindex` to `attach_btf_id` and the trailing padding.
    _rest: [u64; 19],
}

/// The attribute of `BPF_PROG_LOAD`, cilium's `ProgLoadAttr`. Only the fields the daemon sets or
/// reads are named.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgLoadAttr {
    prog_type: u32,
    insn_cnt: u32,
    insns: u64,
    license: u64,
    log_level: u32,
    log_size: u32,
    log_buf: u64,
    kern_version: u32,
    prog_flags: u32,
    prog_name: [u8; OBJ_NAME_LEN],
    /// From `prog_ifindex` to `core_relo_rec_size`.
    _middle: [u32; 19],
    log_true_size: u32,
    /// From `prog_token_fd` to `keyring_id`.
    _rest: [u32; 6],
}

/// The attribute of `BPF_PROG_ATTACH`, cilium's `ProgAttachAttr`.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgAttachAttr {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    replace_bpf_fd: u32,
    relative_fd: u32,
    expected_revision: u64,
}

/// The attribute of `BPF_PROG_DETACH`, cilium's `ProgDetachAttr`.
#[repr(C)]
#[derive(Debug, Default)]
struct ProgDetachAttr {
    target_fd: u32,
    attach_bpf_fd: u32,
    attach_type: u32,
    attach_flags: u32,
    _pad: u32,
    relative_fd: u32,
    expected_revision: u64,
}

/// The attribute structs, the only types [`bpf`] passes to the kernel.
trait Attr {}

impl Attr for ProgQueryAttr {}
impl Attr for ProgGetFdByIdAttr {}
impl Attr for ObjGetInfoByFdAttr {}
impl Attr for ProgLoadAttr {}
impl Attr for ProgAttachAttr {}
impl Attr for ProgDetachAttr {}

/// Runs `command` on `attr`, retrying a program load the kernel answers with `EAGAIN`, as
/// cilium does.
fn bpf<T: Attr>(command: c_long, attr: &mut T) -> Result<c_long, Errno> {
    let size = c_uint::try_from(size_of::<T>()).map_err(|_| Errno::INVAL)?;
    loop {
        // SAFETY: `attr` is one of the `#[repr(C)]` attribute structs (the sealed `Attr`
        // trait), initialised and exclusively borrowed for the call, and `size` is its size.
        // Each struct is the kernel's whole `union bpf_attr` member for its command, so what the
        // kernel writes back stays inside it. Every address it holds was set by the caller from
        // a buffer that the caller borrows for at least as long as this call, with the length
        // the kernel is told.
        let result = unsafe {
            libc::syscall(
                libc::SYS_bpf,
                command,
                std::ptr::from_mut(attr).cast::<c_void>(),
                size,
            )
        };
        if result >= 0 {
            return Ok(result);
        }
        let errno = Errno::from_io_error(&io::Error::last_os_error()).unwrap_or(Errno::IO);
        if errno != Errno::AGAIN || command != BPF_PROG_LOAD {
            return Err(errno);
        }
    }
}

/// The descriptor the kernel returned, owned.
fn owned_fd(raw: c_long) -> Result<OwnedFd, Errno> {
    let fd = i32::try_from(raw).map_err(|_| Errno::BADF)?;
    // SAFETY: the kernel has just returned `fd` as a new descriptor of this process, which
    // nothing else owns yet.
    Ok(unsafe { OwnedFd::from_raw_fd(fd) })
}

/// The kernel's view of a descriptor.
fn fd_u32(fd: BorrowedFd<'_>) -> u32 {
    u32::try_from(fd.as_raw_fd()).unwrap_or(u32::MAX)
}

/// The address the kernel reads from or writes to.
fn address<T>(pointer: *const T) -> u64 {
    u64::try_from(pointer.expose_provenance()).unwrap_or_default()
}

/// What `BPF_PROG_QUERY` answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct QueryAnswer {
    /// The error, if the command failed; with `ENOSPC`, `count` is how many programs there are.
    pub errno: Option<Errno>,
    /// How many program IDs the kernel wrote, or would have.
    pub count: u32,
    /// The cgroup's attach flags for `BPF_CGROUP_DEVICE`.
    pub attach_flags: u32,
}

/// `BPF_PROG_QUERY` for the device programs attached to the cgroup `target`, the IDs written to
/// `ids`.
pub(super) fn prog_query(target: BorrowedFd<'_>, ids: &mut [u32]) -> QueryAnswer {
    let mut attr = ProgQueryAttr {
        target_fd: fd_u32(target),
        attach_type: CGROUP_DEVICE,
        prog_ids: address(ids.as_mut_ptr()),
        prog_cnt: u32::try_from(ids.len()).unwrap_or(u32::MAX),
        ..ProgQueryAttr::default()
    };
    let errno = bpf(BPF_PROG_QUERY, &mut attr).err();
    QueryAnswer {
        errno,
        count: attr.prog_cnt,
        attach_flags: attr.attach_flags,
    }
}

/// `BPF_PROG_GET_FD_BY_ID`: a descriptor for the program `id`.
pub(super) fn prog_get_fd_by_id(id: u32) -> Result<OwnedFd, Errno> {
    let mut attr = ProgGetFdByIdAttr { id };
    owned_fd(bpf(BPF_PROG_GET_FD_BY_ID, &mut attr)?)
}

/// What `BPF_OBJ_GET_INFO_BY_FD` tells of a program, without its instructions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ProgSummary {
    /// The program type.
    pub prog_type: u32,
    /// The program's name.
    pub name: String,
    /// The length of its translated instructions in bytes; 0 when the caller may not read them.
    pub xlated_len: u32,
    /// How many maps it uses.
    pub nr_map_ids: u32,
    /// Whether the kernel's `bpf_prog_info` has the map count at all.
    pub map_ids_known: bool,
}

fn get_info(fd: BorrowedFd<'_>, info: &mut ProgInfo) -> Result<u32, Errno> {
    let mut attr = ObjGetInfoByFdAttr {
        bpf_fd: fd_u32(fd),
        info_len: u32::try_from(size_of::<ProgInfo>()).unwrap_or_default(),
        info: address(std::ptr::from_mut(info)),
    };
    bpf(BPF_OBJ_GET_INFO_BY_FD, &mut attr)?;
    Ok(attr.info_len)
}

/// `BPF_OBJ_GET_INFO_BY_FD` for the program `fd`.
pub(super) fn prog_info(fd: BorrowedFd<'_>) -> Result<ProgSummary, Errno> {
    let mut info = ProgInfo::default();
    let written = get_info(fd, &mut info)?;
    let name_len = info
        .name
        .iter()
        .position(|&byte| byte == 0)
        .unwrap_or(OBJ_NAME_LEN);
    let map_ids_end = std::mem::offset_of!(ProgInfo, map_ids);
    Ok(ProgSummary {
        prog_type: info.prog_type,
        name: String::from_utf8_lossy(&info.name[..name_len]).into_owned(),
        xlated_len: info.xlated_prog_len,
        nr_map_ids: info.nr_map_ids,
        map_ids_known: usize::try_from(written).is_ok_and(|written| written >= map_ids_end),
    })
}

/// The translated instructions of the program `fd`, `len` bytes of them: `None` when the kernel
/// withholds them (`kernel.kptr_restrict` or `net.core.bpf_jit_harden`).
pub(super) fn prog_xlated(fd: BorrowedFd<'_>, len: u32) -> Result<Option<Vec<u8>>, Errno> {
    let mut buffer = vec![0_u8; usize::try_from(len).map_err(|_| Errno::NOMEM)?];
    let mut info = ProgInfo {
        xlated_prog_len: len,
        xlated_prog_insns: address(buffer.as_mut_ptr()),
        ..ProgInfo::default()
    };
    get_info(fd, &mut info)?;
    Ok((info.xlated_prog_insns != 0).then_some(buffer))
}

/// Why `BPF_PROG_LOAD` failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct LoadFailure {
    /// The error.
    pub errno: Errno,
    /// The size the verifier log needed, from kernels that report it; else 0.
    pub log_true_size: u32,
}

/// `BPF_PROG_LOAD` of the encoded instructions `code` as a device program named `name` (cut to
/// 15 bytes), with the verifier log at `log_level` into `log` when the level is not 0.
pub(super) fn prog_load(
    code: &[u8],
    name: &str,
    log_level: u32,
    log: &mut [u8],
) -> Result<OwnedFd, LoadFailure> {
    let mut prog_name = [0_u8; OBJ_NAME_LEN];
    for (slot, byte) in prog_name[..OBJ_NAME_LEN - 1].iter_mut().zip(name.bytes()) {
        *slot = byte;
    }
    let mut attr = ProgLoadAttr {
        prog_type: PROG_TYPE_CGROUP_DEVICE,
        insn_cnt: u32::try_from(code.len() / 8).unwrap_or(u32::MAX),
        insns: address(code.as_ptr()),
        license: address(LICENSE.as_ptr()),
        prog_name,
        ..ProgLoadAttr::default()
    };
    if log_level != 0 {
        attr.log_level = log_level;
        attr.log_size = u32::try_from(log.len()).unwrap_or(u32::MAX);
        attr.log_buf = address(log.as_mut_ptr());
    }
    bpf(BPF_PROG_LOAD, &mut attr)
        .and_then(owned_fd)
        .map_err(|errno| LoadFailure {
            errno,
            log_true_size: attr.log_true_size,
        })
}

/// `BPF_PROG_ATTACH` of `prog` to the cgroup `target` as a device program with `flags`, or with
/// `BPF_F_REPLACE` as the atomic replacement of `replace`.
pub(super) fn prog_attach(
    target: BorrowedFd<'_>,
    prog: BorrowedFd<'_>,
    flags: u32,
    replace: Option<BorrowedFd<'_>>,
) -> Result<(), Errno> {
    let mut attr = ProgAttachAttr {
        target_fd: fd_u32(target),
        attach_bpf_fd: fd_u32(prog),
        attach_type: CGROUP_DEVICE,
        attach_flags: flags,
        ..ProgAttachAttr::default()
    };
    if let Some(replace) = replace {
        attr.attach_flags |= F_REPLACE;
        attr.replace_bpf_fd = fd_u32(replace);
    }
    bpf(BPF_PROG_ATTACH, &mut attr).map(drop)
}

/// `BPF_PROG_DETACH` of the device program `prog` from the cgroup `target`.
pub(super) fn prog_detach(target: BorrowedFd<'_>, prog: BorrowedFd<'_>) -> Result<(), Errno> {
    let mut attr = ProgDetachAttr {
        target_fd: fd_u32(target),
        attach_bpf_fd: fd_u32(prog),
        attach_type: CGROUP_DEVICE,
        ..ProgDetachAttr::default()
    };
    bpf(BPF_PROG_DETACH, &mut attr).map(drop)
}

#[cfg(test)]
mod tests;
