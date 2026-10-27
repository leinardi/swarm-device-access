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

// Adapted from swarm-scheduler-exporter/src/go_compat/errno.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The texts Go gives an `io::Error` from a system call: `syscall.Errno.Error()` for Linux, so a
//! message that quotes one reads as Go's (`no such file or directory`, not Rust's `No such file
//! or directory (os error 2)`). The table is Go's whole linux/amd64 one, so the `bpf(2)` errors
//! read as Go's too.

use std::io;

/// Go's text for `error`: `syscall.Errno.Error()` when it carries an errno, and Rust's text
/// otherwise.
#[must_use]
pub fn io_error_text(error: &io::Error) -> String {
    error
        .raw_os_error()
        .map_or_else(|| error.to_string(), errno_string)
}

/// Go's `syscall.Errno(errno).Error()`: the text of its linux/amd64 table, or `errno <n>` for a
/// number the table does not name (Linux's internal 524 among them).
#[must_use]
pub fn errno_string(errno: i32) -> String {
    errno_text(errno).map_or_else(|| format!("errno {errno}"), str::to_owned)
}

fn errno_text(errno: i32) -> Option<&'static str> {
    usize::try_from(errno)
        .ok()
        .and_then(|index| ERRNO_TEXT.get(index))
        .copied()
        .filter(|text| !text.is_empty())
}

/// Go's table, indexed by errno, printed by a Go program from `syscall.Errno(n).Error()`; an
/// empty text is a number Go does not name.
const ERRNO_TEXT: [&str; 133] = [
    "",
    "operation not permitted",
    "no such file or directory",
    "no such process",
    "interrupted system call",
    "input/output error",
    "no such device or address",
    "argument list too long",
    "exec format error",
    "bad file descriptor",
    "no child processes",
    "resource temporarily unavailable",
    "cannot allocate memory",
    "permission denied",
    "bad address",
    "block device required",
    "device or resource busy",
    "file exists",
    "invalid cross-device link",
    "no such device",
    "not a directory",
    "is a directory",
    "invalid argument",
    "too many open files in system",
    "too many open files",
    "inappropriate ioctl for device",
    "text file busy",
    "file too large",
    "no space left on device",
    "illegal seek",
    "read-only file system",
    "too many links",
    "broken pipe",
    "numerical argument out of domain",
    "numerical result out of range",
    "resource deadlock avoided",
    "file name too long",
    "no locks available",
    "function not implemented",
    "directory not empty",
    "too many levels of symbolic links",
    "",
    "no message of desired type",
    "identifier removed",
    "channel number out of range",
    "level 2 not synchronized",
    "level 3 halted",
    "level 3 reset",
    "link number out of range",
    "protocol driver not attached",
    "no CSI structure available",
    "level 2 halted",
    "invalid exchange",
    "invalid request descriptor",
    "exchange full",
    "no anode",
    "invalid request code",
    "invalid slot",
    "",
    "bad font file format",
    "device not a stream",
    "no data available",
    "timer expired",
    "out of streams resources",
    "machine is not on the network",
    "package not installed",
    "object is remote",
    "link has been severed",
    "advertise error",
    "srmount error",
    "communication error on send",
    "protocol error",
    "multihop attempted",
    "RFS specific error",
    "bad message",
    "value too large for defined data type",
    "name not unique on network",
    "file descriptor in bad state",
    "remote address changed",
    "can not access a needed shared library",
    "accessing a corrupted shared library",
    ".lib section in a.out corrupted",
    "attempting to link in too many shared libraries",
    "cannot exec a shared library directly",
    "invalid or incomplete multibyte or wide character",
    "interrupted system call should be restarted",
    "streams pipe error",
    "too many users",
    "socket operation on non-socket",
    "destination address required",
    "message too long",
    "protocol wrong type for socket",
    "protocol not available",
    "protocol not supported",
    "socket type not supported",
    "operation not supported",
    "protocol family not supported",
    "address family not supported by protocol",
    "address already in use",
    "cannot assign requested address",
    "network is down",
    "network is unreachable",
    "network dropped connection on reset",
    "software caused connection abort",
    "connection reset by peer",
    "no buffer space available",
    "transport endpoint is already connected",
    "transport endpoint is not connected",
    "cannot send after transport endpoint shutdown",
    "too many references: cannot splice",
    "connection timed out",
    "connection refused",
    "host is down",
    "no route to host",
    "operation already in progress",
    "operation now in progress",
    "stale file handle",
    "structure needs cleaning",
    "not a XENIX named type file",
    "no XENIX semaphores available",
    "is a named type file",
    "remote I/O error",
    "disk quota exceeded",
    "no medium found",
    "wrong medium type",
    "operation canceled",
    "required key not available",
    "key has expired",
    "key has been revoked",
    "key was rejected by service",
    "owner died",
    "state not recoverable",
    "operation not possible due to RF-kill",
];

#[cfg(test)]
mod tests {
    use std::io;

    use super::{errno_string, io_error_text};

    /// Go's texts for the errnos the Docker client meets, and Rust's text for an error without
    /// one.
    #[test]
    fn io_error_text_reads_as_go() {
        assert_eq!(
            io_error_text(&io::Error::from_raw_os_error(2)),
            "no such file or directory"
        );
        assert_eq!(
            io_error_text(&io::Error::from_raw_os_error(111)),
            "connection refused"
        );
        assert_eq!(
            io_error_text(&io::Error::from_raw_os_error(13)),
            "permission denied"
        );
        assert_eq!(
            io_error_text(&io::Error::other("something else")),
            "something else"
        );
    }

    /// Go's texts for the errnos `bpf(2)` returns, and `errno <n>` for one Go does not name.
    #[test]
    fn errno_string_reads_as_go() {
        assert_eq!(errno_string(28), "no space left on device");
        assert_eq!(errno_string(7), "argument list too long");
        assert_eq!(errno_string(16), "device or resource busy");
        assert_eq!(errno_string(95), "operation not supported");
        assert_eq!(errno_string(132), "operation not possible due to RF-kill");
        assert_eq!(errno_string(0), "errno 0");
        assert_eq!(errno_string(133), "errno 133");
        assert_eq!(errno_string(524), "errno 524");
        assert_eq!(errno_string(-1), "errno -1");
        assert_eq!(
            io_error_text(&io::Error::from_raw_os_error(524)),
            "errno 524"
        );
    }
}
