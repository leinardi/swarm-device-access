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

// Adapted from swarm-scheduler-exporter/src/docker/host.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The daemon's address, as moby's `ParseHostURL` and go-connections' `ConfigureTransport` read
//! `unix://<socket>`, and Go's URL escaping of the request paths and of the socket path in error
//! messages. The daemon dials only its unix socket (`--docker-socket`), so swarm-scheduler-
//! exporter's TCP host parsing is not taken.

use crate::go_compat::quote::quote;

/// The longest unix socket path: `len(syscall.RawSockaddrUnix{}.Path)` on Linux.
const MAX_UNIX_SOCKET_PATH: usize = 108;

/// moby's `ParseHostURL` and go-connections' `ConfigureTransport` of `unix://<socket>`: the
/// socket path, refused when empty or longer than the kernel takes.
pub(crate) fn unix_socket(socket: &str) -> Result<String, String> {
    if socket.is_empty() {
        return Err("unable to parse docker host `unix://`".to_owned());
    }
    if socket.len() > MAX_UNIX_SOCKET_PATH {
        return Err(format!("unix socket path {} is too long", quote(socket)));
    }
    Ok(socket.to_owned())
}

/// Go's `path.Join`: the elements joined with `/` and cleaned.
pub(crate) fn join_path(elements: &[&str]) -> String {
    let joined = elements
        .iter()
        .filter(|element| !element.is_empty())
        .copied()
        .collect::<Vec<_>>()
        .join("/");
    if joined.is_empty() {
        return String::new();
    }
    clean_path(&joined)
}

/// Go's `path.Clean`.
fn clean_path(path: &str) -> String {
    let rooted = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !rooted {
                    parts.push("..");
                }
            }
            _ => parts.push(part),
        }
    }
    let body = parts.join("/");
    match (rooted, body.is_empty()) {
        (true, _) => format!("/{body}"),
        (false, true) => ".".to_owned(),
        (false, false) => body,
    }
}

/// Go's `url.URL.EscapedPath` for a path without a raw form: bytes outside the unreserved set and
/// `$&+,/:;=@` are percent-encoded.
pub(crate) fn escape_path(path: &str) -> String {
    let mut out = String::with_capacity(path.len());
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"-_.~$&+,/:;=@".contains(&byte) {
            out.push(char::from(byte));
        } else {
            push_percent(&mut out, byte);
        }
    }
    out
}

/// Go's `url.URL.String` host escaping: a byte that cannot appear in a host, such as the `/` of
/// a socket path, is percent-encoded (`http://%2Fvar%2Frun%2Fdocker.sock/…`).
pub(crate) fn escape_host(host: &str) -> String {
    let mut out = String::with_capacity(host.len());
    for byte in host.bytes() {
        if byte.is_ascii() && is_host_char(char::from(byte)) && byte != b'%' {
            out.push(char::from(byte));
        } else {
            push_percent(&mut out, byte);
        }
    }
    out
}

/// `%XX`, upper-case as Go writes it.
pub(crate) fn push_percent(out: &mut String, byte: u8) {
    const HEX: &[u8; 16] = b"0123456789ABCDEF";
    out.push('%');
    out.push(char::from(HEX[usize::from(byte >> 4)]));
    out.push(char::from(HEX[usize::from(byte & 0x0f)]));
}

/// The ASCII characters Go's `url` package accepts in a host: unreserved, sub-delims, `:`, `[`,
/// `]`, `%` escapes, and `<`, `>`, `"` kept for compatibility.
fn is_host_char(character: char) -> bool {
    character.is_ascii_alphanumeric() || "-._~!$&'()*+,;=:[]%<>\"".contains(character)
}

#[cfg(test)]
mod tests;
