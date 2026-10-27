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

//! Go's `context.Context` as the daemon hands it to a Docker call: a cancellation and an
//! optional deadline, the earliest of every parent's.
//!
//! [`with_context`] runs a call under one. The context is also visible to the client for the
//! duration of the call, so a request that is cut short fails as Go's `http.Client` fails it, a
//! `url.Error` naming the request (`Get "<url>": context deadline exceeded`), which moby passes
//! through undecorated. A call that is not a client request (a fake, a request already answered)
//! ends with the bare `context deadline exceeded` or `context canceled`.

use std::future::Future;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use super::error::Error;

tokio::task_local! {
    /// The context of the call running on this task, for the client's requests.
    static CALL_CONTEXT: CallContext;
}

/// Why a context is done: Go's `context.Canceled` or `context.DeadlineExceeded`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContextError {
    Canceled,
    DeadlineExceeded,
}

impl ContextError {
    /// Go's text of the error.
    #[must_use]
    pub const fn text(self) -> &'static str {
        match self {
            Self::Canceled => "context canceled",
            Self::DeadlineExceeded => "context deadline exceeded",
        }
    }
}

/// A cancellation and an optional deadline. Cloning shares the context; [`CallContext::with_timeout`]
/// and [`CallContext::with_deadline`] derive a child, which its parent's cancellation reaches and
/// whose own cancellation does not reach the parent.
#[derive(Debug, Clone, Default)]
pub struct CallContext {
    token: CancellationToken,
    /// The earliest deadline of the context and its ancestors.
    deadline: Option<Instant>,
    /// When [`CallContext::cancel`] was called on this context, the first time.
    cancelled_at: Arc<OnceLock<Instant>>,
    parent: Option<Arc<CallContext>>,
}

impl CallContext {
    /// A root context, done only when cancelled: Go's `context.WithCancel(context.Background())`.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A child with a deadline `timeout` from now, or its parent's when that is earlier: Go's
    /// `context.WithTimeout`.
    #[must_use]
    pub fn with_timeout(&self, timeout: Duration) -> Self {
        self.with_deadline(Instant::now() + timeout)
    }

    /// A child with `deadline`, or its parent's when that is earlier: Go's
    /// `context.WithDeadline`.
    #[must_use]
    pub fn with_deadline(&self, deadline: Instant) -> Self {
        Self {
            token: self.token.child_token(),
            deadline: Some(self.deadline.map_or(deadline, |own| own.min(deadline))),
            cancelled_at: Arc::default(),
            parent: Some(Arc::new(self.clone())),
        }
    }

    /// Cancels the context and its children: Go's `cancel()`. A context whose deadline passed
    /// first stays done with `DeadlineExceeded`.
    pub fn cancel(&self) {
        let _ = self.cancelled_at.set(Instant::now());
        self.token.cancel();
    }

    /// A token cancelled with the context (not when its deadline passes), for a task that takes
    /// a [`CancellationToken`]. Cancelling the token does not cancel the context.
    #[must_use]
    pub fn cancellation(&self) -> CancellationToken {
        self.token.child_token()
    }

    /// Go's `ctx.Err()`: why the context is done, if it is, by what happened first.
    #[must_use]
    pub fn err(&self) -> Option<ContextError> {
        self.done_at().map(|(_, why)| why)
    }

    /// When and why the context became done: the earliest of its passed deadline, its own
    /// cancellation and its parent's.
    fn done_at(&self) -> Option<(Instant, ContextError)> {
        let deadline = self
            .deadline
            .filter(|deadline| *deadline <= Instant::now())
            .map(|deadline| (deadline, ContextError::DeadlineExceeded));
        let cancelled = self
            .cancelled_at
            .get()
            .map(|at| (*at, ContextError::Canceled));
        let inherited = self.parent.as_ref().and_then(|parent| parent.done_at());
        [deadline, cancelled, inherited]
            .into_iter()
            .flatten()
            .min_by_key(|(at, _)| *at)
    }

    /// Resolves once the context is done, with why.
    pub async fn done(&self) -> ContextError {
        if let Some(deadline) = self.deadline {
            tokio::select! {
                biased;
                () = self.token.cancelled() => {}
                () = tokio::time::sleep_until(deadline) => {}
            }
        } else {
            self.token.cancelled().await;
        }
        self.err().unwrap_or(ContextError::Canceled)
    }
}

/// Runs `call` under `context`, as a Go call takes its context: it ends with the context's error
/// once the context is done first.
///
/// # Errors
///
/// The call's [`Error`], or the context's (see the module comment) when it was done first.
pub async fn with_context<T>(
    context: &CallContext,
    call: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    CALL_CONTEXT
        .scope(context.clone(), async {
            // The call first: a request cut short by the same deadline fails inside it with
            // Go's url.Error, which wins over the bare error below.
            tokio::select! {
                biased;
                result = call => result,
                done = context.done() => Err(Error::context(done)),
            }
        })
        .await
}

/// The context of the call running on this task, if any.
pub(crate) fn current() -> Option<CallContext> {
    CALL_CONTEXT.try_with(Clone::clone).ok()
}
