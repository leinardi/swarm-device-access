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

//! The kernel side of the cgroup v2 controller: the [`V2Ops`] the controller drives, so that the
//! ordering of its attaches, detaches and closes can be tested without privileges, and
//! [`KernelOps`], the production implementation on `bpf_sys.rs`. The errors read as the Go
//! daemon's, whose kernel side was cilium/ebpf v0.22.0: its wording, its verifier-log retry on a
//! failed load and the hints it adds are kept.
//!
//! cilium probes kernel features before some calls (object names, map IDs in the program info,
//! `BPF_PROG_ATTACH` itself) and reads `/proc/self/fdinfo` when `BPF_OBJ_GET_INFO_BY_FD` answers
//! `EINVAL`. Every one of those features is older than the `openat2` the daemon refuses to start
//! without (Linux 5.6), so none is probed here and the fallback is not ported.

use std::fmt;
use std::os::fd::{AsFd as _, BorrowedFd, OwnedFd};
use std::sync::atomic::{AtomicU8, Ordering};

use rustix::io::Errno;

use super::asm::{DecodeError, EncodeCause, Instruction};
use super::bpf_sys;
use super::owned::ProgMeta;
use crate::go_compat::errno::errno_string;
use crate::go_compat::quote::is_space;
use crate::logger::{Attr, LogHandle};

/// `BPF_F_ALLOW_OVERRIDE`.
pub const F_ALLOW_OVERRIDE: u32 = 1;
/// `BPF_F_ALLOW_MULTI`.
pub const F_ALLOW_MULTI: u32 = 2;

/// How many program IDs the first query has room for: a cgroup holds at most 64 programs.
const QUERY_IDS: usize = 64;
/// How many times a query is repeated when more programs appeared than it had room for.
const QUERY_RETRIES: usize = 10;

/// cilium's verifier log sizes and attempts.
const MIN_VERIFIER_LOG_SIZE: u32 = 64 * 1024;
const MAX_VERIFIER_LOG_SIZE: u32 = u32::MAX >> 2;
const MAX_VERIFIER_ATTEMPTS: u32 = 30;
/// `BPF_LOG_LEVEL1`, cilium's `LogLevelBranch`.
const LOG_LEVEL_BRANCH: u32 = 1;

/// The programs attached to a cgroup as a device filter.
#[derive(Debug)]
pub struct Attached<P> {
    /// The programs that could be opened.
    pub progs: Vec<P>,
    /// How many programs the kernel reports attached: the only reliable "nothing attached"
    /// signal, since every attached program may be inaccessible.
    pub total: usize,
    /// How many could not be opened (`EACCES` or `EPERM`, typically an LSM label).
    pub inaccessible: usize,
    /// The cgroup's attach flags for `BPF_CGROUP_DEVICE`.
    pub attach_flags: u32,
}

/// The kernel surface the cgroup v2 controller uses. A program handle is closed when dropped.
pub trait V2Ops: Send + Sync {
    /// An open program.
    type Prog: Send;

    /// The programs attached to the cgroup `dir` for `BPF_CGROUP_DEVICE`. On error no handle is
    /// returned, and every one opened so far is closed.
    ///
    /// # Errors
    ///
    /// [`OpsError`] when the query fails or an attached program cannot be opened for a reason
    /// other than permission.
    fn query(&self, dir: BorrowedFd<'_>) -> Result<Attached<Self::Prog>, OpsError>;

    /// Loads `instructions` as a device program named `name`.
    ///
    /// # Errors
    ///
    /// [`OpsError`] when an instruction does not encode or the kernel refuses the program.
    fn load(&self, name: &str, instructions: &[Instruction]) -> Result<Self::Prog, OpsError>;

    /// Attaches `prog` to the cgroup `dir` with `flags`, or as the atomic replacement of
    /// `replace`.
    ///
    /// # Errors
    ///
    /// [`OpsError`] when the kernel refuses the attach.
    fn attach(
        &self,
        prog: &Self::Prog,
        dir: BorrowedFd<'_>,
        flags: u32,
        replace: Option<&Self::Prog>,
    ) -> Result<(), OpsError>;

    /// Detaches `prog` from the cgroup `dir`.
    ///
    /// # Errors
    ///
    /// [`OpsError`] when the kernel refuses the detach.
    fn detach(&self, prog: &Self::Prog, dir: BorrowedFd<'_>) -> Result<(), OpsError>;

    /// The program's translated instructions, and what the reloadability gate needs to know
    /// about its maps.
    ///
    /// # Errors
    ///
    /// [`OpsError`] on a kernel older than 4.16, or when the instructions cannot be read.
    fn instructions(&self, prog: &Self::Prog) -> Result<(Vec<Instruction>, ProgMeta), OpsError>;
}

/// A program the kernel opened for [`KernelOps`], closed on drop.
#[derive(Debug)]
pub struct KernelProg {
    fd: OwnedFd,
}

/// The production [`V2Ops`], on the raw `bpf(2)` calls.
#[derive(Debug, Clone)]
pub struct KernelOps {
    log: LogHandle,
}

impl KernelOps {
    /// The operations, logging through `log`.
    #[must_use]
    pub const fn new(log: LogHandle) -> Self {
        Self { log }
    }

    fn open_by_id(id: u32) -> Result<KernelProg, FetchError> {
        let fd = bpf_sys::prog_get_fd_by_id(id).map_err(FetchError::GetById)?;
        bpf_sys::prog_info(fd.as_fd()).map_err(FetchError::Discover)?;
        Ok(KernelProg { fd })
    }

    fn load_code(code: &[u8], name: &str) -> Result<OwnedFd, LoadError> {
        let mut log_level = 0;
        let mut log_size = 0;
        let mut attempts = 1;
        loop {
            let mut log = vec![
                0_u8;
                if log_level == 0 {
                    0
                } else {
                    to_usize(log_size)
                }
            ];
            let failure = match bpf_sys::prog_load(code, name, log_level, &mut log) {
                Ok(fd) => return Ok(fd),
                Err(failure) => failure,
            };
            if !retry_log(&mut log_level, &mut log_size, failure) {
                return Err(LoadError::from_failure(failure.errno, &log));
            }
            if attempts >= MAX_VERIFIER_ATTEMPTS {
                return Err(LoadError::Attempts(failure.errno));
            }
            attempts += 1;
        }
    }
}

fn to_usize(value: u32) -> usize {
    usize::try_from(value).unwrap_or(usize::MAX)
}

/// cilium's `retryLogAttrs`: whether to load again, and with which log.
fn retry_log(log_level: &mut u32, log_size: &mut u32, failure: bpf_sys::LoadFailure) -> bool {
    if *log_size == MAX_VERIFIER_LOG_SIZE {
        return false;
    }
    if failure.errno == Errno::NOSPC {
        *log_size = if failure.log_true_size != 0 {
            failure.log_true_size
        } else if *log_size >= MAX_VERIFIER_LOG_SIZE / 2 {
            MAX_VERIFIER_LOG_SIZE
        } else {
            (*log_size).clamp(MIN_VERIFIER_LOG_SIZE, MAX_VERIFIER_LOG_SIZE / 2) * 2
        };
        return true;
    }
    if *log_level == 0 {
        *log_level = LOG_LEVEL_BRANCH;
        *log_size = MIN_VERIFIER_LOG_SIZE;
        return true;
    }
    false
}

impl V2Ops for KernelOps {
    type Prog = KernelProg;

    fn query(&self, dir: BorrowedFd<'_>) -> Result<Attached<KernelProg>, OpsError> {
        let mut size = QUERY_IDS;
        for _ in 0..QUERY_RETRIES {
            let mut ids = vec![0_u32; size];
            let answer = bpf_sys::prog_query(dir, &mut ids);
            size = to_usize(answer.count);
            match answer.errno {
                // The kernel reported how many there are: query again with room for them.
                Some(Errno::NOSPC) => continue,
                Some(errno) => return Err(OpsError::Query(errno)),
                None => {}
            }
            ids.truncate(size);
            let mut progs = Vec::with_capacity(ids.len());
            let mut inaccessible = 0;
            for id in ids {
                match Self::open_by_id(id) {
                    Ok(prog) => progs.push(prog),
                    // Counted, not skipped: runc ignores such programs because extra filters only
                    // restrict, but one could be a wrapper of this daemon holding a grant, and
                    // without opening it neither its ownership nor the result of a replacement
                    // can be established.
                    Err(error) if error.is_permission() => {
                        self.log.debug(
                            "existing CGROUP_DEVICE program cannot be accessed (likely LSM policy)",
                            &[
                                Attr::uint("prog_id", u64::from(id)),
                                Attr::error("err", &error),
                            ],
                        );
                        inaccessible += 1;
                    }
                    Err(error) => return Err(OpsError::Fetch(error)),
                }
            }
            return Ok(Attached {
                progs,
                total: size,
                inaccessible,
                attach_flags: answer.attach_flags,
            });
        }
        Err(OpsError::IncompleteProgList)
    }

    fn load(&self, name: &str, instructions: &[Instruction]) -> Result<KernelProg, OpsError> {
        if instructions.is_empty() {
            return Err(OpsError::Load(LoadError::Empty));
        }
        let mut code = Vec::new();
        for (index, instruction) in instructions.iter().enumerate() {
            instruction
                .encode(&mut code)
                .map_err(|cause| OpsError::Load(LoadError::Encode { index, cause }))?;
        }
        let fd = Self::load_code(&code, name).map_err(OpsError::Load)?;
        Ok(KernelProg { fd })
    }

    fn attach(
        &self,
        prog: &KernelProg,
        dir: BorrowedFd<'_>,
        flags: u32,
        replace: Option<&KernelProg>,
    ) -> Result<(), OpsError> {
        bpf_sys::prog_attach(
            dir,
            prog.fd.as_fd(),
            flags,
            replace.map(|old| old.fd.as_fd()),
        )
        .map_err(|errno| OpsError::Attach { flags, errno })
    }

    fn detach(&self, prog: &KernelProg, dir: BorrowedFd<'_>) -> Result<(), OpsError> {
        bpf_sys::prog_detach(dir, prog.fd.as_fd()).map_err(OpsError::Detach)
    }

    fn instructions(&self, prog: &KernelProg) -> Result<(Vec<Instruction>, ProgMeta), OpsError> {
        if !kernel_dumps_sanitized() {
            return Err(OpsError::OldKernel);
        }
        let fd = prog.fd.as_fd();
        let summary =
            bpf_sys::prog_info(fd).map_err(|errno| OpsError::Info(InfoError::ObjInfo(errno)))?;
        let xlated = if summary.xlated_len > 0 {
            bpf_sys::prog_xlated(fd, summary.xlated_len)
                .map_err(|errno| OpsError::Info(InfoError::Xlated(errno)))?
                .ok_or(OpsError::Instructions(InstructionsError::Restricted))?
        } else {
            Vec::new()
        };
        // Zero when the caller may not read them, or the kernel cannot dump them.
        if xlated.is_empty() {
            return Err(OpsError::Instructions(InstructionsError::NotSupported));
        }
        let instructions = super::asm::decode(&xlated)
            .map_err(|error| OpsError::Instructions(InstructionsError::Decode(error)))?;
        let meta = ProgMeta {
            map_ids_known: summary.map_ids_known || summary.nr_map_ids > 0,
            map_count: to_usize(summary.nr_map_ids),
        };
        Ok((instructions, meta))
    }
}

/// Whether the running kernel is 4.16 or newer, the first release whose translated dumps of
/// device filters are sanitized consistently enough to be loaded back.
fn kernel_dumps_sanitized() -> bool {
    let uname = rustix::system::uname();
    kernel_at_least(&uname.release().to_string_lossy(), 4, 16)
}

const REPLACE_UNKNOWN: u8 = 0;
const REPLACE_SUPPORTED: u8 = 1;
const REPLACE_UNSUPPORTED: u8 = 2;

/// Whether the kernel supports an atomic `BPF_F_REPLACE` of a cgroup program (Linux 5.5+). It is
/// learnt from the first replace rather than from a kernel version: the attempt either succeeds
/// or fails with `EINVAL` (an unknown flag on older kernels), and the answer is kept for the
/// life of the process. Falling back to attach-then-detach on a kernel that could have replaced
/// is safe, only less tidy, so a stray `EINVAL` cannot make anything wider.
#[derive(Debug, Default)]
pub struct ReplaceProbe {
    state: AtomicU8,
}

/// The probe every cgroup v2 controller shares: the kernel's answer does not change.
pub static PROCESS_REPLACE_PROBE: ReplaceProbe = ReplaceProbe::new();

impl ReplaceProbe {
    /// A probe that has not seen a replace yet.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            state: AtomicU8::new(REPLACE_UNKNOWN),
        }
    }

    /// A probe that has already seen the answer, `supported` or not.
    #[must_use]
    pub const fn known(supported: bool) -> Self {
        Self {
            state: AtomicU8::new(if supported {
                REPLACE_SUPPORTED
            } else {
                REPLACE_UNSUPPORTED
            }),
        }
    }

    /// Whether a replace may be tried: until one has failed with `EINVAL`.
    #[must_use]
    pub fn usable(&self) -> bool {
        self.state.load(Ordering::Relaxed) != REPLACE_UNSUPPORTED
    }

    /// Records the outcome of a replace: the first success or `EINVAL` decides.
    pub fn record(&self, outcome: Result<(), &OpsError>) {
        let decided = match outcome {
            Ok(()) => REPLACE_SUPPORTED,
            Err(error) if error.errno() == Some(Errno::INVAL) => REPLACE_UNSUPPORTED,
            Err(_) => return,
        };
        // Losing the race means another replace decided first, with the same answer.
        let _ = self.state.compare_exchange(
            REPLACE_UNKNOWN,
            decided,
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
    }
}

/// Names cgroup attach flags for logs and errors.
#[must_use]
pub fn describe_attach_flags(flags: u32) -> String {
    let names: Vec<&str> = [
        (F_ALLOW_OVERRIDE, "BPF_F_ALLOW_OVERRIDE"),
        (F_ALLOW_MULTI, "BPF_F_ALLOW_MULTI"),
    ]
    .into_iter()
    .filter(|&(flag, _)| flags & flag != 0)
    .map(|(_, name)| name)
    .collect();
    if names.is_empty() {
        "none (exclusive)".to_owned()
    } else {
        names.join("|")
    }
}

/// cilium's errno text: Go's, except that Linux's internal `ENOTSUPP` reads as not supported.
fn cilium_errno(errno: Errno) -> String {
    if errno.raw_os_error() == ENOTSUPP {
        "operation not supported".to_owned()
    } else {
        errno_string(errno.raw_os_error())
    }
}

/// Linux's internal `ENOTSUPP`, which leaks out of `bpf(2)`.
const ENOTSUPP: i32 = 524;

/// Why a kernel operation of the cgroup v2 controller failed, worded as the Go daemon's errors.
#[derive(Debug)]
pub enum OpsError {
    /// `BPF_PROG_QUERY` failed.
    Query(Errno),
    /// The programs kept outnumbering the query's room.
    IncompleteProgList,
    /// An attached program could not be opened.
    Fetch(FetchError),
    /// A program could not be loaded.
    Load(LoadError),
    /// `BPF_PROG_ATTACH` failed.
    Attach {
        /// The attach flags asked for.
        flags: u32,
        /// The error.
        errno: Errno,
    },
    /// `BPF_PROG_DETACH` failed.
    Detach(Errno),
    /// The kernel is older than 4.16; Go's `ErrProgramNotWrappable`.
    OldKernel,
    /// `BPF_OBJ_GET_INFO_BY_FD` failed.
    Info(InfoError),
    /// The translated instructions are not available or do not decode.
    Instructions(InstructionsError),
}

impl OpsError {
    /// The errno a system call failed with, if that is the error.
    #[must_use]
    pub const fn errno(&self) -> Option<Errno> {
        match self {
            Self::Query(errno)
            | Self::Attach { errno, .. }
            | Self::Detach(errno)
            | Self::Info(InfoError::ObjInfo(errno) | InfoError::Xlated(errno))
            | Self::Fetch(FetchError::GetById(errno) | FetchError::Discover(errno)) => Some(*errno),
            Self::Load(error) => error.errno(),
            Self::IncompleteProgList | Self::OldKernel | Self::Instructions(_) => None,
        }
    }

    /// Whether it is Go's `ErrProgramNotWrappable`.
    #[must_use]
    pub const fn is_not_wrappable(&self) -> bool {
        matches!(self, Self::OldKernel)
    }
}

impl fmt::Display for OpsError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Query(errno) => write!(
                out,
                "bpf_prog_query(BPF_CGROUP_DEVICE) failed: {}",
                errno_string(errno.raw_os_error())
            ),
            Self::IncompleteProgList => {
                out.write_str("could not get complete list of CGROUP_DEVICE programs")
            }
            Self::Fetch(error) => write!(out, "cannot fetch program from id: {error}"),
            Self::Load(error) => {
                write!(out, "unable to create new device filters program: {error}")
            }
            Self::Attach { flags, errno } => write!(
                out,
                "failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, flags {}): attach program: {}",
                describe_attach_flags(*flags),
                cilium_errno(*errno)
            ),
            Self::Detach(errno) => write!(
                out,
                "failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): can't detach program: {}",
                cilium_errno(*errno)
            ),
            Self::OldKernel => out.write_str(
                "device filter program cannot be wrapped safely: kernel older than 4.16",
            ),
            Self::Info(error) => write!(
                out,
                "unable to get Info() of the original device filters program: {error}"
            ),
            Self::Instructions(error) => write!(
                out,
                "unable to get the instructions of the original device filters program: {error}"
            ),
        }
    }
}

impl std::error::Error for OpsError {}

/// Why an attached program could not be opened.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchError {
    /// `BPF_PROG_GET_FD_BY_ID` failed.
    GetById(Errno),
    /// `BPF_OBJ_GET_INFO_BY_FD` on the new descriptor failed.
    Discover(Errno),
}

impl FetchError {
    /// Whether it is Go's `os.ErrPermission`: `EACCES` or `EPERM`.
    #[must_use]
    pub fn is_permission(self) -> bool {
        let (Self::GetById(errno) | Self::Discover(errno)) = self;
        errno == Errno::ACCESS || errno == Errno::PERM
    }
}

impl fmt::Display for FetchError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::GetById(errno) => write!(out, "get program by id: {}", cilium_errno(*errno)),
            Self::Discover(errno) => write!(
                out,
                "discover program type: getting object info: {}",
                cilium_errno(*errno)
            ),
        }
    }
}

impl std::error::Error for FetchError {}

/// Why a program's information could not be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InfoError {
    /// The first `BPF_OBJ_GET_INFO_BY_FD` failed.
    ObjInfo(Errno),
    /// The one that reads the translated instructions failed.
    Xlated(Errno),
}

impl fmt::Display for InfoError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::ObjInfo(errno) => {
                write!(out, "getting object info: {}", cilium_errno(*errno))
            }
            Self::Xlated(errno) => out.write_str(&cilium_errno(*errno)),
        }
    }
}

/// Why a program's translated instructions are not available.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstructionsError {
    /// The kernel withholds them.
    Restricted,
    /// The caller may not read them, or the kernel cannot dump them.
    NotSupported,
    /// They do not decode.
    Decode(DecodeError),
}

impl fmt::Display for InstructionsError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Restricted => out.write_str(
                "instructions: restricted by kernel.kptr_restrict and/or \
                 net.core.bpf_jit_harden sysctls",
            ),
            Self::NotSupported => {
                out.write_str("insufficient permissions or unsupported kernel: not supported")
            }
            Self::Decode(error) => write!(
                out,
                "unmarshaling instructions: offset {}: {}",
                error.offset, error.cause
            ),
        }
    }
}

/// Why `BPF_PROG_LOAD` refused a program, worded as cilium's errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoadError {
    /// No instruction to load.
    Empty,
    /// An instruction does not encode.
    Encode {
        /// Its index.
        index: usize,
        /// Why.
        cause: EncodeCause,
    },
    /// The verifier log kept growing past cilium's attempts.
    Attempts(Errno),
    /// `EPERM` before the verifier ran: the memlock limit.
    Memlock(Errno),
    /// `EFAULT`: a verifier bug.
    VerifierBug(Errno),
    /// The kernel refused the program, with the verifier log's lines.
    Verifier {
        /// The error.
        errno: Errno,
        /// cilium's reading of the log tail, when it names one: a bad CO-RE relocation or an
        /// unknown kfunc.
        cause: Option<&'static str>,
        /// The log, trimmed, one entry per line.
        log: Vec<String>,
    },
}

/// cilium's markers of a bad CO-RE relocation and of an unknown kfunc in the verifier log.
const CORE_BAD_CALL: &str = "invalid func unknown#195896080\n";
const CORE_BAD_LOAD: &str = "(18) r10 = 0xbad2310\n";
const KFUNC_BAD_CALL: &str = "invalid func unknown#233685214\n";
const BAD_RELOCATION: &str = "bad CO-RE relocation";
const UNKNOWN_KFUNC: &str = "unknown kfunc";

impl LoadError {
    /// cilium's classification of a failed load, from the error and the log buffer.
    fn from_failure(errno: Errno, log: &[u8]) -> Self {
        let end = log.iter().position(|&byte| byte == 0).unwrap_or(log.len());
        let tail = String::from_utf8_lossy(&log[end.saturating_sub(256)..end]);
        let mut cause = None;
        if errno == Errno::PERM && log.first() == Some(&0) {
            // EPERM from RLIMIT_MEMLOCK happens before the verifier, so the log is empty.
            return Self::Memlock(errno);
        } else if errno == Errno::FAULT {
            return Self::VerifierBug(errno);
        } else if errno == Errno::INVAL {
            if tail.contains(CORE_BAD_CALL) {
                cause = Some(BAD_RELOCATION);
            } else if tail.contains(KFUNC_BAD_CALL) {
                cause = Some(UNKNOWN_KFUNC);
            }
        } else if errno == Errno::ACCESS && tail.contains(CORE_BAD_LOAD) {
            cause = Some(BAD_RELOCATION);
        }
        Self::Verifier {
            errno,
            cause,
            log: log_lines(&log[..end]),
        }
    }

    const fn errno(&self) -> Option<Errno> {
        match self {
            Self::Attempts(errno)
            | Self::Memlock(errno)
            | Self::VerifierBug(errno)
            | Self::Verifier { errno, .. } => Some(*errno),
            Self::Empty | Self::Encode { .. } => None,
        }
    }
}

const LOG_WHITESPACE: &[char] = &['\t', '\r', '\u{b}', '\n', ' '];

/// cilium's `ErrorWithLog` split: the log trimmed, one line per entry, each trimmed at its end.
fn log_lines(log: &[u8]) -> Vec<String> {
    let text = String::from_utf8_lossy(log);
    let trimmed = text.trim_matches(LOG_WHITESPACE);
    if trimmed.is_empty() {
        return Vec::new();
    }
    trimmed
        .split('\n')
        .map(|line| line.trim_end_matches(LOG_WHITESPACE).to_owned())
        .collect()
}

/// Whether cilium shows the line before `line` too.
fn include_previous_line(line: &str) -> bool {
    line.starts_with('\t')
        || matches!(line.as_bytes(), [b'R', digit, ..] if digit.is_ascii_digit())
        || line.starts_with("invalid bpf_context access")
}

impl fmt::Display for LoadError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => out.write_str("instructions cannot be empty"),
            Self::Encode { index, cause } => write!(out, "instruction {index}: {cause}"),
            Self::Attempts(errno) => write!(
                out,
                "load program: {} (bug: hit {MAX_VERIFIER_ATTEMPTS} verifier attempts)",
                cilium_errno(*errno)
            ),
            Self::Memlock(errno) => write!(
                out,
                "load program: {} (MEMLOCK may be too low, consider rlimit.RemoveMemlock)",
                cilium_errno(*errno)
            ),
            Self::VerifierBug(errno) => write!(
                out,
                "load program: {} (hit verifier bug, increase LogSizeStart to fit the log and \
                 check dmesg)",
                cilium_errno(*errno)
            ),
            Self::Verifier { errno, cause, log } => {
                let cause = cause.map_or_else(|| cilium_errno(*errno), str::to_owned);
                write!(out, "load program: {cause}")?;
                // cilium's VerifierError: the last line (and the one before it when that line
                // continues it), without the final statistics, and how many were left out.
                let shown_log = match log.split_last() {
                    Some((last, rest)) if last.starts_with("processed ") => rest,
                    Some(_) | None => log.as_slice(),
                };
                let Some(last) = shown_log.last() else {
                    return Ok(());
                };
                let lines = if shown_log.len() >= 2 && include_previous_line(last) {
                    &shown_log[shown_log.len() - 2..]
                } else {
                    &shown_log[shown_log.len() - 1..]
                };
                for line in lines {
                    write!(out, ": {}", line.trim())?;
                }
                let omitted = log.len() - lines.len();
                if omitted > 0 {
                    write!(out, " ({omitted} line(s) omitted)")?;
                }
                Ok(())
            }
        }
    }
}

/// Whether `release` (a `uname -r` string) is at least `want_major.want_minor`, parsed as Go's
/// `fmt.Sscanf(release, "%d.%d", ...)` parses its leading numbers: spaces before each number are
/// skipped but a newline is not, a sign is allowed, and anything after the second number is
/// ignored. A release that does not parse is not at least anything.
#[must_use]
pub fn kernel_at_least(release: &str, want_major: i64, want_minor: i64) -> bool {
    let Some((major, rest)) = scan_decimal(release) else {
        return false;
    };
    let Some((minor, _)) = rest.strip_prefix('.').and_then(scan_decimal) else {
        return false;
    };
    major > want_major || (major == want_major && minor >= want_minor)
}

/// Go's `%d` verb: leading spaces, an optional sign, then at least one ASCII digit, as a 64-bit
/// integer.
fn scan_decimal(input: &str) -> Option<(i64, &str)> {
    let start = input.trim_start_matches(|character| character != '\n' && is_space(character));
    let unsigned = start.strip_prefix(['+', '-']).unwrap_or(start);
    let digits = unsigned
        .find(|character: char| !character.is_ascii_digit())
        .unwrap_or(unsigned.len());
    if digits == 0 {
        return None;
    }
    let (token, rest) = start.split_at(start.len() - unsigned.len() + digits);
    Some((token.parse().ok()?, rest))
}

#[cfg(test)]
mod tests;
