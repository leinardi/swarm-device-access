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

// Adapted from swarm-scheduler-exporter/src/server.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The daemon's metrics and health server, Go's `StartMetricsServer` on rust-common's `go-http`,
//! which answers `/metrics`, `/healthz` and `/readyz` as Go's `http.ServeMux` behind an
//! `http.Server` answers them, byte for byte where hyper can (Q11): Go's routing, header case,
//! order and framing, the four server timeouts, `net.Listen`'s bind, the accept loop and the
//! drain.
//!
//! What is the daemon's stays here:
//!
//! - the routes: `/healthz` always `ok`; `/readyz` `ok` once the daemon is subscribed to the
//!   Docker events, `not ready` with a 503 before (the injected [`Readiness`]), each with Go's
//!   sniffed type; `/metrics` promhttp's answer for the [`Recorder`]'s registry, always in the
//!   text format (Q4);
//! - the server's timeouts, [`TIMEOUTS`]: 5 s for the request head, 10 s to read the request,
//!   30 s to write the answer and 60 s idle;
//! - a listen error's text, Go's `net.OpError` with no prefix; `net/http`'s own messages, Go's
//!   `log.Printf` lines on stderr in the logger's zone;
//! - [`start_metrics_server`]: the listener is bound before it returns, as Go's `net.Listen`
//!   binds it, so a busy or invalid address is a startup error; a server error is logged; the
//!   stop runs once, from the caller or the context, and drains the open connections for five
//!   seconds.
//!
//! There is no debug server: a non-empty `debug-addr` is refused ([`refuse_debug_addr`], Q2).

use std::fmt;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use clock::Clock;
use go_compat::quote::quote;
use go_http::{Drained, ListenError, Mux, Reply, Server, Timeouts};
use hyper::StatusCode;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::daemon::Readiness;
use crate::metrics::exposition::{self, Exposition};
use crate::metrics::promhttp::HandlerMetrics;
use crate::metrics::{Recorder, Registry};
use tracing::{error, info, warn};

pub use go_http::MetricsRequest;

/// The metrics endpoint.
pub const METRICS_PATH: &str = "/metrics";
/// The liveness endpoint.
pub const HEALTHZ_PATH: &str = "/healthz";
/// The readiness endpoint.
pub const READYZ_PATH: &str = "/readyz";

/// How long a stop waits for the open connections: Go's `shutdownTimeout`.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// The Go daemon's `http.Server` timeouts: `ReadHeaderTimeout` 5 s, `ReadTimeout` 10 s,
/// `WriteTimeout` 30 s and `IdleTimeout` 60 s.
pub const TIMEOUTS: Timeouts = Timeouts {
    read_header: Duration::from_secs(5),
    read: Duration::from_secs(10),
    write: Duration::from_secs(30),
    idle: Duration::from_mins(1),
};

/// What a listen error's text starts with: nothing, the Go daemon printed `net.OpError` bare.
const LISTEN_ERROR_PREFIX: &str = "";

/// The endpoints.
#[derive(Debug)]
pub struct App {
    server: Arc<Server>,
}

impl App {
    /// The endpoints answering from `readiness` and the registry of `metrics`, with `Date` read
    /// from `clock`, and the server's own messages logged as warnings.
    #[must_use]
    pub fn new(readiness: Readiness, metrics: Recorder, clock: Arc<dyn Clock>) -> Self {
        let mux = mux(readiness, move |scrape| scrape_metrics(&metrics, scrape));
        Self::with_mux(mux, clock)
    }

    /// `mux` served with `Date` from `clock`, and `net/http`'s messages logged as warnings.
    fn with_mux(mux: Mux, clock: Arc<dyn Clock>) -> Self {
        Self {
            server: Arc::new(server(mux, clock)),
        }
    }
}

/// The Go daemon's mux: `/metrics` from `metrics`, `/healthz`, and `/readyz` from `readiness`.
fn mux(
    readiness: Readiness,
    metrics: impl Fn(&MetricsRequest) -> Exposition + Send + Sync + 'static,
) -> Mux {
    Mux::new()
        .route(METRICS_PATH, move |request| {
            let exposition = metrics(&MetricsRequest::of(request));
            Reply::metrics(
                &exposition.content_type,
                exposition.content_encoding,
                exposition.body,
            )
        })
        .route(HEALTHZ_PATH, |_| healthz())
        .route(READYZ_PATH, move |_| readyz(&readiness))
}

/// `mux` behind a server whose own messages (Go's `log.Printf` lines) are logged as warnings.
fn server(mux: Mux, clock: Arc<dyn Clock>) -> Server {
    Server::new(mux, clock, |message| warn!("{message}"))
}

/// Go's `/healthz` handler: always `ok`, its type sniffed.
fn healthz() -> Reply {
    Reply::sniffed(StatusCode::OK, "ok")
}

/// Go's `/readyz` handler: `ok` once ready, `not ready` with a 503 before, its type sniffed.
fn readyz(readiness: &Readiness) -> Reply {
    if readiness.is_ready() {
        Reply::sniffed(StatusCode::OK, "ok")
    } else {
        Reply::sniffed(StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// promhttp's `Handler()`: the registry gathered and rendered for the scrape, inside its
/// handler metrics (in flight while it runs, counted under 200 after).
fn scrape_metrics(recorder: &Recorder, scrape: &MetricsRequest) -> Exposition {
    let in_flight = recorder.handler_metrics().map(HandlerMetrics::begin);
    let families = recorder
        .registry()
        .map(Registry::gather)
        .unwrap_or_default();
    let encodings: Vec<&str> = scrape.accept_encoding.iter().map(String::as_str).collect();
    let exposition = exposition::render(
        go_prom::ExpfmtVersion::V0_70_1,
        &families,
        scrape.accept.as_deref(),
        &encodings,
        scrape.names.as_ref(),
    );
    if let Some(in_flight) = in_flight {
        in_flight.finish(StatusCode::OK.as_u16());
    }
    exposition
}

/// Go's `StartMetricsServer` failing: `metrics server: listen on <address>: <cause>`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartError {
    address: String,
    cause: ListenError,
}

impl fmt::Display for StartError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "metrics server: listen on {}: {}",
            self.address, self.cause
        )
    }
}

impl std::error::Error for StartError {}

/// A running metrics server: Go's `stop` func.
#[derive(Debug)]
pub struct MetricsServer {
    local: Option<SocketAddr>,
    stop: CancellationToken,
    served: tokio::sync::Mutex<Option<tokio::task::JoinHandle<()>>>,
}

impl MetricsServer {
    /// The address the server listens on.
    #[must_use]
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local
    }

    /// Shuts the server down and waits for it: the listener is closed at once and the open
    /// connections are drained for [`SHUTDOWN_TIMEOUT`]. It runs once; a later or concurrent
    /// call waits for that one.
    pub async fn stop(&self) {
        self.stop.cancel();
        let mut served = self.served.lock().await;
        // The handle stays until the task has ended, so a stop whose caller gave up waiting
        // leaves the next one something to wait for.
        if let Some(task) = served.as_mut() {
            let _ = task.await;
            *served = None;
        }
    }
}

/// Go's `StartMetricsServer`: binds `address` now, so a busy or invalid address is an error
/// rather than a log line from a task, then serves `app` until `context` is cancelled or
/// [`MetricsServer::stop`] is called.
///
/// # Errors
///
/// [`StartError`] when the address cannot be bound.
pub async fn start_metrics_server(
    context: &CancellationToken,
    address: &str,
    app: App,
) -> Result<MetricsServer, StartError> {
    start_draining_for(context, address, app, SHUTDOWN_TIMEOUT).await
}

/// [`start_metrics_server`] with a stop that drains for `drain`.
async fn start_draining_for(
    context: &CancellationToken,
    address: &str,
    app: App,
    drain: Duration,
) -> Result<MetricsServer, StartError> {
    let listener = go_http::bind(address, LISTEN_ERROR_PREFIX)
        .await
        .map_err(|cause| StartError {
            address: address.to_owned(),
            cause,
        })?;
    let App { server } = app;
    let local = listener.local_addr().ok();
    info!(addr = %local.map_or_else(String::new, |local| local.to_string()), "metrics server listening");
    let stop = context.child_token();
    let shutdown = stop.clone();
    let task = tokio::spawn(async move {
        // An accept error Go's Serve does not retry closes the listener and is logged at once;
        // the open connections are served until the stop, as Go's are until Shutdown.
        let served = go_http::serve(
            listener,
            server,
            TIMEOUTS,
            shutdown.cancelled_owned(),
            move || Instant::now() + drain,
            |error| error!(error = %error, "metrics server error"),
        )
        .await;
        if served.drained == Drained::DeadlineExceeded {
            warn!(error = %"context deadline exceeded", "metrics server shutdown error");
        }
    });
    Ok(MetricsServer {
        local,
        stop,
        served: tokio::sync::Mutex::new(Some(task)),
    })
}

/// Why a debug server address is refused (Q2).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebugAddrRefused(String);

impl fmt::Display for DebugAddrRefused {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "debug-addr {}: the pprof debug server was removed in 2.0; leave debug-addr empty",
            quote(&self.0)
        )
    }
}

impl std::error::Error for DebugAddrRefused {}

/// Q2: 1.x served pprof on `debug-addr`; 2.0 has no debug server, so a non-empty address is
/// refused (at startup and on reload) instead of being ignored, and an empty one is accepted.
///
/// # Errors
///
/// [`DebugAddrRefused`] for a non-empty `address`.
pub fn refuse_debug_addr(address: &str) -> Result<(), DebugAddrRefused> {
    if address.is_empty() {
        Ok(())
    } else {
        Err(DebugAddrRefused(address.to_owned()))
    }
}

#[cfg(test)]
mod tests;
