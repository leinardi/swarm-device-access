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

// Adapted from swarm-scheduler-exporter/src/docker/connect.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Dialing the daemon's unix socket within go-connections' dial timeout, worded as Go's
//! `net.OpError` words a failure. swarm-scheduler-exporter's TCP, TLS and proxy dialing is not
//! taken: the daemon reaches Docker only through the socket it is configured with.

use std::fmt;
use std::io;

use tokio::net::UnixStream;

use super::DIAL_TIMEOUT;
use crate::go_compat::errno::io_error_text;

/// Why a connection could not be made, worded as Go's `net.OpError` and net/http word it.
#[derive(Debug)]
pub(crate) enum ConnectError {
    /// The connect system call failed.
    Dial { address: String, error: io::Error },
    /// The dial did not finish within [`DIAL_TIMEOUT`].
    DialTimeout { address: String },
}

impl ConnectError {
    /// The `io::Error` of a failed connect.
    pub(crate) fn dial_error(&self) -> Option<&io::Error> {
        match self {
            Self::Dial { error, .. } => Some(error),
            Self::DialTimeout { .. } => None,
        }
    }

    /// Whether Go's `net.Error.Timeout()` would be true.
    pub(crate) fn is_timeout(&self) -> bool {
        match self {
            Self::DialTimeout { .. } => true,
            Self::Dial { error, .. } => error.raw_os_error() == Some(110),
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial { address, error } => write!(
                formatter,
                "dial unix {address}: connect: {}",
                io_error_text(error)
            ),
            Self::DialTimeout { address } => {
                write!(formatter, "dial unix {address}: i/o timeout")
            }
        }
    }
}

/// How to reach the daemon: its socket path.
#[derive(Debug, Clone)]
pub(crate) struct Connector {
    socket: String,
}

impl Connector {
    pub(crate) fn new(socket: String) -> Self {
        Self { socket }
    }

    /// A new connection, dialed within [`DIAL_TIMEOUT`].
    pub(crate) async fn connect(&self) -> Result<UnixStream, ConnectError> {
        match tokio::time::timeout(DIAL_TIMEOUT, UnixStream::connect(&self.socket)).await {
            Ok(Ok(stream)) => Ok(stream),
            Ok(Err(error)) => Err(ConnectError::Dial {
                address: self.socket.clone(),
                error,
            }),
            Err(_) => Err(ConnectError::DialTimeout {
                address: self.socket.clone(),
            }),
        }
    }
}

#[cfg(test)]
mod tests;
