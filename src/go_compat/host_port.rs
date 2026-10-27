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

// Adapted from swarm-scheduler-exporter/src/go_compat/host_port.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Go's `net.SplitHostPort` and `net.JoinHostPort`, with Go's error texts, which the metrics
//! address's listen errors report verbatim.

use std::fmt;

/// Go's `net.AddrError`: `address <addr>: <why>`, or `<why>` alone for an empty address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AddrError {
    /// Why the address was refused.
    pub why: &'static str,
    /// The address, as given.
    pub address: String,
}

impl fmt::Display for AddrError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.address.is_empty() {
            return formatter.write_str(self.why);
        }
        write!(formatter, "address {}: {}", self.address, self.why)
    }
}

impl std::error::Error for AddrError {}

const MISSING_PORT: &str = "missing port in address";
const TOO_MANY_COLONS: &str = "too many colons in address";

/// Go's `net.SplitHostPort`: `host:port`, `[host]:port` or `[host%zone]:port` into the host and
/// the port, with the brackets dropped.
///
/// # Errors
///
/// An [`AddrError`] with Go's reason.
pub fn split_host_port(host_port: &str) -> Result<(&str, &str), AddrError> {
    let error = |why| AddrError {
        why,
        address: host_port.to_owned(),
    };
    // The port starts after the last colon.
    let Some(last_colon) = host_port.rfind(':') else {
        return Err(error(MISSING_PORT));
    };
    let host;
    let (open_from, close_from);
    if host_port.starts_with('[') {
        // Expect the first ']' just before the last ':'.
        let Some(end) = host_port.find(']') else {
            return Err(error("missing ']' in address"));
        };
        let after = end + 1;
        if after == host_port.len() {
            // There can't be a ':' behind the ']' now.
            return Err(error(MISSING_PORT));
        }
        if after != last_colon {
            // Either ']' isn't followed by a colon, or it is followed by a colon that is not the
            // last one.
            return Err(error(if host_port.as_bytes()[after] == b':' {
                TOO_MANY_COLONS
            } else {
                MISSING_PORT
            }));
        }
        host = &host_port[1..end];
        (open_from, close_from) = (1, after);
    } else {
        host = &host_port[..last_colon];
        if host.contains(':') {
            return Err(error(TOO_MANY_COLONS));
        }
        (open_from, close_from) = (0, 0);
    }
    if host_port[open_from..].contains('[') {
        return Err(error("unexpected '[' in address"));
    }
    if host_port[close_from..].contains(']') {
        return Err(error("unexpected ']' in address"));
    }
    Ok((host, &host_port[last_colon + 1..]))
}

/// Go's `net.JoinHostPort`: a host with a colon (IPv6) goes in brackets.
#[must_use]
pub fn join_host_port(host: &str, port: &str) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

#[cfg(test)]
mod tests;
