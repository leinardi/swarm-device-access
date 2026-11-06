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

//! The error of a Docker call: docker-client's, sorted into the kinds the program acts on, or the
//! context's when the caller's cancellation or deadline ended the call first. The error and the
//! stream's end are `Clone`, as the event loop and the launcher's tests keep them.

use std::error::Error as StdError;
use std::fmt;
use std::sync::Arc;

pub use docker_client::ErrorKind;

use super::context::ContextError;

/// A failed Docker call. Clones share the cause.
#[derive(Debug, Clone)]
pub struct Error {
    repr: Repr,
}

#[derive(Debug, Clone)]
enum Repr {
    /// docker-client's: bollard's error, or its whole-call timeout.
    Client(Arc<docker_client::Error>),
    /// The caller's context was done first: Go's `ctx.Err()`.
    Context(ContextError),
    /// An error with no bollard behind it: a refused ID, a body that does not convert, a double
    /// of a test.
    Plain { kind: ErrorKind, message: String },
}

impl Error {
    /// Go's `context.DeadlineExceeded`.
    #[must_use]
    pub fn deadline_exceeded() -> Self {
        Self::context(ContextError::DeadlineExceeded)
    }

    /// The bare error of a done context, Go's `ctx.Err()`.
    #[must_use]
    pub fn context(done: ContextError) -> Self {
        Self {
            repr: Repr::Context(done),
        }
    }

    /// An engine that could not be reached, for a reason in `message`.
    #[must_use]
    pub fn unavailable(message: impl Into<String>) -> Self {
        Self::plain(ErrorKind::Unavailable, message)
    }

    /// The engine's answer with `status` (400 or above) and `message`, sorted as docker-client
    /// sorts it: 404 is not found, 409 a conflict, the rest other.
    #[must_use]
    pub fn response(status: u16, message: impl Into<String>) -> Self {
        Self::plain(
            match status {
                404 => ErrorKind::NotFound,
                409 => ErrorKind::Conflict,
                _ => ErrorKind::Other,
            },
            message,
        )
    }

    /// A call refused before any request, as moby refuses an empty ID.
    #[must_use]
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::plain(ErrorKind::Other, message)
    }

    fn plain(kind: ErrorKind, message: impl Into<String>) -> Self {
        Self {
            repr: Repr::Plain {
                kind,
                message: message.into(),
            },
        }
    }

    /// The kind of failure; a done deadline is a [`ErrorKind::Timeout`], a cancellation
    /// [`ErrorKind::Other`] (see [`Error::is_canceled`]).
    #[must_use]
    pub fn kind(&self) -> ErrorKind {
        match &self.repr {
            Repr::Client(error) => error.kind(),
            Repr::Context(ContextError::DeadlineExceeded) => ErrorKind::Timeout,
            Repr::Context(ContextError::Canceled) => ErrorKind::Other,
            Repr::Plain { kind, .. } => *kind,
        }
    }

    /// containerd's `errdefs.IsNotFound`: the engine answered 404.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.kind() == ErrorKind::NotFound
    }

    /// Whether the caller's context was cancelled: Go's `errors.Is(err, context.Canceled)`.
    #[must_use]
    pub fn is_canceled(&self) -> bool {
        matches!(self.repr, Repr::Context(ContextError::Canceled))
    }
}

impl From<docker_client::Error> for Error {
    fn from(error: docker_client::Error) -> Self {
        Self {
            repr: Repr::Client(Arc::new(error)),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.repr {
            Repr::Client(error) => error.fmt(formatter),
            Repr::Context(done) => formatter.write_str(done.text()),
            Repr::Plain { message, .. } => formatter.write_str(message),
        }
    }
}

impl StdError for Error {
    fn source(&self) -> Option<&(dyn StdError + 'static)> {
        match &self.repr {
            Repr::Client(error) => error.source(),
            Repr::Context(_) | Repr::Plain { .. } => None,
        }
    }
}

/// A failure of a stream of output, as a test double injects one: bollard's own error behind
/// docker-client's.
#[cfg(test)]
pub(crate) fn stream_failure(message: &str) -> docker_client::Error {
    docker_client::Error::from(docker_client::bollard::errors::Error::DockerStreamError {
        error: message.to_owned(),
    })
}

/// How an event stream ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamErrorKind {
    /// Go's `io.EOF`: the engine closed the stream cleanly.
    Eof,
    /// Go's `io.ErrUnexpectedEOF`: the connection broke, or timed out, mid-stream.
    UnexpectedEof,
    /// A value that is not an event.
    Decode,
    /// The source of the events went away without saying why: Go's `ErrEventsStreamClosed`, the
    /// closed channel of a test double.
    Closed,
}

/// Why an event stream ended; every stream ends with one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamError {
    kind: StreamErrorKind,
    message: String,
}

impl StreamError {
    /// A stream that ended with `kind`, worded as `message`.
    #[must_use]
    pub fn new(kind: StreamErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }

    /// Go's `io.EOF`.
    #[must_use]
    pub fn eof() -> Self {
        Self::new(StreamErrorKind::Eof, "EOF")
    }

    /// How the stream ended.
    #[must_use]
    pub fn kind(&self) -> StreamErrorKind {
        self.kind
    }

    /// A stream item that failed: a broken or timed-out connection ends the stream unexpectedly,
    /// anything else is a value that could not be read.
    pub(super) fn failed(error: &docker_client::Error) -> Self {
        let kind = match error.kind() {
            ErrorKind::Unavailable | ErrorKind::Timeout => StreamErrorKind::UnexpectedEof,
            // The caller's own token ended the stream: the source went away, nothing failed.
            ErrorKind::Cancelled => StreamErrorKind::Closed,
            ErrorKind::NotFound | ErrorKind::Conflict | ErrorKind::Other => StreamErrorKind::Decode,
        };
        Self::new(kind, error.to_string())
    }
}

impl fmt::Display for StreamError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl StdError for StreamError {}
