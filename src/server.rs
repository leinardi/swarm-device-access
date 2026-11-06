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

//! The daemon's metrics and health server: service-kit's `http` server (hyper on tokio, with a
//! header-read timeout, a write-stall timeout and a bounded drain) in front of an axum router.
//!
//! - the routes: `/healthz` always `ok`; `/readyz` `ok` once the daemon is subscribed to the
//!   Docker events, `not ready` with a 503 before (the injected [`Readiness`]); `/metrics` the
//!   [`Recorder`]'s registry as OpenMetrics text (Q4), uncompressed; anything else is a 404. Any
//!   method is answered;
//! - the server's deadlines: 5 s for a request's head ([`HEADER_READ_TIMEOUT`]), 30 s without a
//!   byte leaving a write ([`WRITE_STALL_TIMEOUT`]), and [`SHUTDOWN_TIMEOUT`] for the drain;
//! - [`start_metrics_server`]: the listener is bound before it returns, so a busy or invalid
//!   address is a startup error (`metrics server: listen on <address>: <cause>`); a server error
//!   is logged; the stop runs once, from the caller or the context, and drains the open
//!   connections for five seconds.
//!
//! There is no debug server: a non-empty `debug-addr` is refused ([`refuse_debug_addr`], Q2).

use std::fmt;
use std::io;
use std::net::SocketAddr;
use std::time::Duration;

use axum::Router;
use axum::http::StatusCode;
use axum::http::header::CONTENT_TYPE;
use axum::routing::any;
use go_compat::quote::quote;
use service_kit::http::{Lifecycle, Outcome, ServerConfig};
use tokio::net::TcpListener;
use tokio_util::sync::CancellationToken;
use tracing::{error, info, warn};

use crate::daemon::Readiness;
use crate::metrics::Recorder;

/// The metrics endpoint.
pub const METRICS_PATH: &str = "/metrics";
/// The liveness endpoint.
pub const HEALTHZ_PATH: &str = "/healthz";
/// The readiness endpoint.
pub const READYZ_PATH: &str = "/readyz";

/// How long a stop waits for the open connections: Go's `shutdownTimeout`.
pub const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a client may take to send a request's head, an idle keep-alive wait included.
pub const HEADER_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// How long a write may wait without a byte leaving before the connection is closed.
pub const WRITE_STALL_TIMEOUT: Duration = Duration::from_secs(30);

/// The endpoints.
#[derive(Debug)]
pub struct App {
    router: Router,
}

impl App {
    /// The endpoints answering from `readiness` and the registry of `metrics`.
    #[must_use]
    pub fn new(readiness: Readiness, metrics: Recorder) -> Self {
        Self::with_router(router(readiness, metrics))
    }

    /// `router` served as it is.
    fn with_router(router: Router) -> Self {
        Self { router }
    }
}

/// The daemon's routes: `/metrics` from `metrics`, `/healthz`, and `/readyz` from `readiness`.
fn router(readiness: Readiness, metrics: Recorder) -> Router {
    Router::new()
        .route(
            METRICS_PATH,
            any(move || {
                let exposition = metrics.exposition();
                async move { ([(CONTENT_TYPE, exposition.content_type)], exposition.body) }
            }),
        )
        .route(HEALTHZ_PATH, any(|| async { "ok" }))
        .route(
            READYZ_PATH,
            any(move || {
                let ready = readiness.is_ready();
                async move {
                    if ready {
                        (StatusCode::OK, "ok")
                    } else {
                        (StatusCode::SERVICE_UNAVAILABLE, "not ready")
                    }
                }
            }),
        )
}

/// Why the metrics server did not start: `metrics server: listen on <address>: <cause>`.
#[derive(Debug)]
pub struct StartError {
    address: String,
    cause: io::Error,
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

impl std::error::Error for StartError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.cause)
    }
}

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
    let config = ServerConfig::new(HEADER_READ_TIMEOUT, WRITE_STALL_TIMEOUT, SHUTDOWN_TIMEOUT);
    start_with(context, address, app, config).await
}

/// [`start_metrics_server`] with `config`'s deadlines.
async fn start_with(
    context: &CancellationToken,
    address: &str,
    app: App,
    config: ServerConfig,
) -> Result<MetricsServer, StartError> {
    let listener = bind(address).await.map_err(|cause| StartError {
        address: address.to_owned(),
        cause,
    })?;
    let local = listener.local_addr().ok();
    info!(addr = %local.map_or_else(String::new, |local| local.to_string()), "metrics server listening");
    let stop = context.child_token();
    let lifecycle = Lifecycle::new();
    let stopper = {
        let (stop, lifecycle) = (stop.clone(), lifecycle.clone());
        tokio::spawn(async move {
            stop.cancelled().await;
            lifecycle.stop();
        })
    };
    let task = tokio::spawn(async move {
        let outcome = service_kit::http::serve(listener, app.router, config, &lifecycle).await;
        stopper.abort();
        match outcome {
            Outcome::Drained => {}
            Outcome::DeadlineExceeded => {
                warn!(error = %"context deadline exceeded", "metrics server shutdown error");
            }
            Outcome::ListenerFailed(failure) => {
                error!(error = %failure, "metrics server error");
            }
        }
    });
    Ok(MetricsServer {
        local,
        stop,
        served: tokio::sync::Mutex::new(Some(task)),
    })
}

/// The listen backlog; the kernel caps it at `net.core.somaxconn`.
const BACKLOG: i32 = 1024;

/// Binds `address`, a `host:port` or Go's `:port` for every interface: dual-stack on `[::]`
/// whatever `net.ipv6.bindv6only` says, as Go sets it, and on `0.0.0.0` where the host has no
/// IPv6.
async fn bind(address: &str) -> io::Result<TcpListener> {
    match address.strip_prefix(':') {
        Some(port) => {
            let port: u16 = port.parse().map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    format!("invalid port {port:?}"),
                )
            })?;
            wildcard(port)
        }
        None => TcpListener::bind(address).await,
    }
}

/// Every interface on `port`: `[::]` with `IPV6_V6ONLY` off, else `0.0.0.0` when the host has no
/// IPv6 (and only then: any other failure, a busy port included, is the first one).
fn wildcard(port: u16) -> io::Result<TcpListener> {
    match dual_stack(port) {
        Ok(listener) => Ok(listener),
        Err(first) if ipv6_unavailable(&first) => {
            let socket = socket2::Socket::new(socket2::Domain::IPV4, socket2::Type::STREAM, None)
                .map_err(|_| io::Error::new(first.kind(), first.to_string()))?;
            listen_on(&socket, SocketAddr::from(([0, 0, 0, 0], port))).map_err(|_| first)
        }
        Err(first) => Err(first),
    }
}

/// A listener on `[::]:port` that also takes IPv4 connections.
fn dual_stack(port: u16) -> io::Result<TcpListener> {
    let socket = socket2::Socket::new(socket2::Domain::IPV6, socket2::Type::STREAM, None)?;
    socket.set_only_v6(false)?;
    listen_on(
        &socket,
        SocketAddr::from((std::net::Ipv6Addr::UNSPECIFIED, port)),
    )
}

/// Binds and listens `socket` on `address`, address reuse on as Go's listeners have it, and
/// hands it to tokio.
fn listen_on(socket: &socket2::Socket, address: SocketAddr) -> io::Result<TcpListener> {
    socket.set_reuse_address(true)?;
    socket.set_nonblocking(true)?;
    socket.bind(&address.into())?;
    socket.listen(BACKLOG)?;
    TcpListener::from_std(socket.try_clone()?.into())
}

/// Whether `error` says this host has no IPv6 to listen on.
fn ipv6_unavailable(error: &io::Error) -> bool {
    matches!(
        error.kind(),
        io::ErrorKind::AddrNotAvailable | io::ErrorKind::Unsupported
    ) || error.raw_os_error() == Some(rustix::io::Errno::AFNOSUPPORT.raw_os_error())
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
