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

// Adapted from swarm-scheduler-exporter/src/docker/pool.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Connections kept for reuse, and sending one request over them, as Go's `http.Transport`
//! does for moby's default client: at most two idle connections (`MaxIdleConnsPerHost`), each
//! closed after 30s idle (moby's `IdleConnTimeout`), the most recently used taken first, and a
//! request that fails on a reused connection before any response retried on another.
//!
//! The Go daemon builds its client with moby's defaults, which set no response-header timeout
//! (swarm-scheduler-exporter's Go client set 30s): every call is bounded by its caller instead, and
//! the event stream's establishment by the subscription's own timeout.

use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, Weak};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::client::conn::http1::{self, SendRequest};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

use super::connect::{ConnectError, Connector};

/// The body of a request: empty for every call but the launcher's create.
pub(crate) type RequestBody = Full<Bytes>;

/// net/http's `DefaultMaxIdleConnsPerHost`, the bound that applies with one daemon.
const MAX_IDLE: usize = 2;

/// moby's `IdleConnTimeout`.
const IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// How many times a request is sent before its failure is reported: Go retries on another
/// connection for as long as the failed one was reused, and only two can be idle.
const MAX_ATTEMPTS: usize = MAX_IDLE + 1;

/// Why a request got no response.
#[derive(Debug)]
pub(crate) enum SendError {
    /// No connection could be made.
    Connect(ConnectError),
    /// The exchange failed.
    Http(hyper::Error),
}

struct IdleConnection {
    id: u64,
    sender: SendRequest<RequestBody>,
}

/// The idle connections of one daemon.
pub(crate) struct Pool {
    connector: Connector,
    idle: Mutex<Vec<IdleConnection>>,
    next_id: AtomicU64,
}

impl std::fmt::Debug for Pool {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Pool")
            .field("connector", &self.connector)
            .field("idle", &self.lock().len())
            .finish_non_exhaustive()
    }
}

impl Pool {
    pub(crate) fn new(connector: Connector) -> Arc<Self> {
        Arc::new(Self {
            connector,
            idle: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(0),
        })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Vec<IdleConnection>> {
        self.idle.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// The most recently returned idle connection that is still open.
    async fn checkout(&self) -> Option<SendRequest<RequestBody>> {
        loop {
            let mut sender = self.lock().pop()?.sender;
            if !sender.is_closed() && sender.ready().await.is_ok() {
                return Some(sender);
            }
        }
    }

    /// A new connection, its HTTP/1.1 state driven by a task of its own; header names are
    /// written in Go's canonical case.
    async fn connect(&self) -> Result<SendRequest<RequestBody>, SendError> {
        let stream = self.connector.connect().await.map_err(SendError::Connect)?;
        let (sender, connection) = http1::Builder::new()
            .title_case_headers(true)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(SendError::Http)?;
        tokio::spawn(async move {
            // The connection ends with an error the request already reported, or cleanly.
            let _ = connection.await;
        });
        Ok(sender)
    }

    /// Sends `request` on a connection of its own that is never pooled and can be taken over by
    /// an HTTP upgrade, as moby's hijacked attach dials one.
    pub(crate) async fn send_upgradable(
        &self,
        request: Request<RequestBody>,
    ) -> Result<Response<Incoming>, SendError> {
        let stream = self.connector.connect().await.map_err(SendError::Connect)?;
        let (mut sender, connection) = http1::Builder::new()
            .title_case_headers(true)
            .handshake(TokioIo::new(stream))
            .await
            .map_err(SendError::Http)?;
        tokio::spawn(async move {
            // The connection ends with an error the request already reported, cleanly, or by
            // being handed to the upgrade.
            let _ = connection.with_upgrades().await;
        });
        sender.send_request(request).await.map_err(SendError::Http)
    }

    /// Keeps `sender` for reuse if there is room, and closes it after [`IDLE_TIMEOUT`] unused.
    fn give_back(self: &Arc<Self>, sender: SendRequest<RequestBody>) {
        if sender.is_closed() {
            return;
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        {
            let mut idle = self.lock();
            if idle.len() >= MAX_IDLE {
                return;
            }
            idle.push(IdleConnection { id, sender });
        }
        let pool = Arc::downgrade(self);
        // Outside a runtime there is no timer; the connection then closes when next checked out
        // or dropped with the pool.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                tokio::time::sleep(IDLE_TIMEOUT).await;
                if let Some(pool) = pool.upgrade() {
                    pool.lock().retain(|connection| connection.id != id);
                }
            });
        }
    }

    /// Sends the request `build` makes, on an idle connection or a new one. A failure on a
    /// reused connection before any response, which is how a connection the daemon closed while
    /// idle shows, sends it again on another, as Go does for a `GET` or `HEAD`. Any other method
    /// (the launcher's writes) is sent again only when it was never written, as net/http's
    /// `shouldRetryRequest` retries a request that is not replayable only on a
    /// `nothingWrittenError`: a write the daemon may already have carried out is never repeated.
    pub(crate) async fn send(
        self: &Arc<Self>,
        build: impl Fn() -> Request<RequestBody>,
    ) -> Result<Response<PooledBody>, SendError> {
        let mut attempts = 0;
        loop {
            attempts += 1;
            let (mut sender, reused) = match self.checkout().await {
                Some(sender) => (sender, true),
                None => (self.connect().await?, false),
            };
            let request = build();
            // net/http's isReplayable: the safe methods, of which the client sends GET and HEAD.
            let replayable = request.method().is_safe();
            match sender.send_request(request).await {
                Err(error) => {
                    // hyper reports a request it never wrote as canceled.
                    let unsent = error.is_canceled();
                    let retry = reused
                        && attempts < MAX_ATTEMPTS
                        && (unsent
                            || (replayable
                                && (error.is_closed() || error.is_incomplete_message())));
                    if !retry {
                        return Err(SendError::Http(error));
                    }
                }
                Ok(response) => {
                    let (parts, incoming) = response.into_parts();
                    let body = PooledBody::new(incoming, sender, Arc::downgrade(self));
                    return Ok(Response::from_parts(parts, body));
                }
            }
        }
    }
}

/// A response body that returns its connection to the pool once read to the end, as Go's
/// transport does; a body dropped before its end closes the connection.
#[derive(Debug)]
pub(crate) struct PooledBody {
    inner: Incoming,
    connection: Option<(SendRequest<RequestBody>, Weak<Pool>)>,
}

impl PooledBody {
    fn new(inner: Incoming, sender: SendRequest<RequestBody>, pool: Weak<Pool>) -> Self {
        let mut body = Self {
            inner,
            connection: Some((sender, pool)),
        };
        if body.inner.is_end_stream() {
            body.release();
        }
        body
    }

    fn release(&mut self) {
        if let Some((sender, pool)) = self.connection.take()
            && let Some(pool) = pool.upgrade()
        {
            pool.give_back(sender);
        }
    }
}

impl Body for PooledBody {
    type Data = Bytes;
    type Error = hyper::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, hyper::Error>>> {
        let poll = Pin::new(&mut self.inner).poll_frame(context);
        match &poll {
            Poll::Ready(None) => self.release(),
            Poll::Ready(Some(Err(_))) => self.connection = None,
            Poll::Ready(Some(Ok(_))) => {
                if self.inner.is_end_stream() {
                    self.release();
                }
            }
            Poll::Pending => {}
        }
        poll
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
