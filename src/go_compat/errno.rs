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
//! or directory (os error 2)`).

use std::io;

/// Go's text for `error`: the Linux errno string Go prints when the error carries an errno it
/// names here, and Rust's text otherwise.
#[must_use]
pub fn io_error_text(error: &io::Error) -> String {
    error
        .raw_os_error()
        .and_then(errno_text)
        .map_or_else(|| error.to_string(), str::to_owned)
}

/// Go's `syscall.Errno` texts (its linux/amd64 `zerrors` table) for the errors a file read or a
/// socket can return.
fn errno_text(errno: i32) -> Option<&'static str> {
    Some(match errno {
        1 => "operation not permitted",
        2 => "no such file or directory",
        4 => "interrupted system call",
        5 => "input/output error",
        9 => "bad file descriptor",
        11 => "resource temporarily unavailable",
        12 => "cannot allocate memory",
        13 => "permission denied",
        17 => "file exists",
        20 => "not a directory",
        21 => "is a directory",
        22 => "invalid argument",
        23 => "too many open files in system",
        24 => "too many open files",
        32 => "broken pipe",
        36 => "file name too long",
        40 => "too many levels of symbolic links",
        88 => "socket operation on non-socket",
        97 => "address family not supported by protocol",
        98 => "address already in use",
        99 => "cannot assign requested address",
        100 => "network is down",
        101 => "network is unreachable",
        103 => "software caused connection abort",
        104 => "connection reset by peer",
        105 => "no buffer space available",
        107 => "transport endpoint is not connected",
        110 => "connection timed out",
        111 => "connection refused",
        112 => "host is down",
        113 => "no route to host",
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use std::io;

    use super::io_error_text;

    /// Go's texts for the errnos the Docker client meets, and Rust's text for anything else.
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
}
