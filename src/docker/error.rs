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

// Adapted from swarm-scheduler-exporter/src/docker/error.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The client's errors, worded as moby client v0.6.0 words them.

use std::error::Error as StdError;
use std::fmt;

use hyper::StatusCode;

/// Why the client could not be built: moby's `New(WithHost("unix://" + socket))` error, which the
/// Go daemon logs as `docker client init failed`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
}

impl ConfigError {
    pub(crate) fn new(message: String) -> Self {
        Self { message }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for ConfigError {}

/// What kind of failure an [`Error`] is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorKind {
    /// moby's `errConnectionFailed`: no response, for a reason in the message.
    ConnectionFailed,
    /// The daemon answered with a status of 400 or above; [`Error::status`] has it.
    Response,
    /// The response body failed after the headers.
    Body,
    /// The response body is not the JSON the call expects.
    Decode,
    /// The call was refused before any request, as moby refuses an empty ID.
    InvalidParameter,
    /// The caller's deadline passed first: Go's `context.DeadlineExceeded`.
    DeadlineExceeded,
}

/// A failed request.
#[derive(Debug, Clone)]
pub struct Error {
    kind: ErrorKind,
    message: String,
    status: Option<StatusCode>,
}

impl Error {
    pub(crate) fn connection_failed(message: String) -> Self {
        Self {
            kind: ErrorKind::ConnectionFailed,
            message,
            status: None,
        }
    }

    pub(crate) fn response(status: StatusCode, message: String) -> Self {
        Self {
            kind: ErrorKind::Response,
            message,
            status: Some(status),
        }
    }

    pub(crate) fn body(message: String) -> Self {
        Self {
            kind: ErrorKind::Body,
            message,
            status: None,
        }
    }

    pub(crate) fn decode(message: String) -> Self {
        Self {
            kind: ErrorKind::Decode,
            message,
            status: None,
        }
    }

    pub(crate) fn invalid_parameter(message: String) -> Self {
        Self {
            kind: ErrorKind::InvalidParameter,
            message,
            status: None,
        }
    }

    /// Go's `context.DeadlineExceeded`, which moby passes through undecorated.
    #[must_use]
    pub fn deadline_exceeded() -> Self {
        Self {
            kind: ErrorKind::DeadlineExceeded,
            message: "context deadline exceeded".to_owned(),
            status: None,
        }
    }

    /// containerd's `errdefs.IsNotFound` of a moby error: the daemon answered 404.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.kind == ErrorKind::Response && self.status == Some(StatusCode::NOT_FOUND)
    }

    /// The kind of failure.
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        self.kind
    }

    /// The daemon's status, for a [`ErrorKind::Response`].
    #[must_use]
    pub fn status(&self) -> Option<StatusCode> {
        self.status
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for Error {}
