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

//! The daemon's metrics and health server, Go's `StartMetricsServer` on hyper: `/metrics`,
//! `/healthz` and `/readyz`, answered as Go's `http.ServeMux` behind an `http.Server` answers
//! them, byte for byte where hyper can (Q11).
//!
//! - Exact paths, any method; `OPTIONS *` as Go's global options handler; Go's 404; and a 307 to
//!   the cleaned path for an unclean one, keeping the query ([`gopath`]).
//! - `/healthz` always `ok`; `/readyz` `ok` once the daemon is subscribed to the Docker events,
//!   `not ready` with a 503 before (the injected [`Readiness`]); `/metrics` promhttp's answer for
//!   the [`Recorder`]'s registry, always in the text format (Q4).
//! - Go's header case and order: the handler's headers sorted by name, then `Date`, then
//!   `Content-Length`, then `Connection`. hyper keeps a header set by hand where it is and only
//!   appends what is missing, so all three are set here; hyper's own `Date` is off, so its
//!   parse-error answers carry none, as Go's do.
//! - Go's framing: a body of at most 2 KiB (Go's `bufferBeforeChunkingSize`) is sent with a
//!   `Content-Length`; a larger one is chunked, in chunks of 2 KiB (Go's chunk boundaries follow
//!   the handler's writes, which no client sees), and a HEAD answer to it has neither header.
//! - Go's four server timeouts, 5 s for the request head, 10 s to read the request, 30 s to
//!   write the answer and 60 s idle, kept by [`timeouts`] instead of hyper's header timer.
//! - The listener is bound before [`start_metrics_server`] returns, as Go's `net.Listen` binds
//!   it, so a busy or invalid address is a startup error; the stop runs once, from the caller or
//!   the context, and drains the open connections for five seconds.
//!
//! There is no debug server: a non-empty `debug-addr` is refused ([`refuse_debug_addr`], Q2).

mod framing;
pub mod gopath;
pub mod timeouts;

use std::collections::BTreeSet;
use std::convert::Infallible;
use std::fmt;
use std::future::Future;
use std::io::{self, Write as _};
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use bytes::Bytes;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::header::{
    ACCEPT, ACCEPT_ENCODING, CONNECTION, CONTENT_ENCODING, CONTENT_LENGTH, CONTENT_TYPE, DATE,
    HeaderName, HeaderValue, LOCATION,
};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode, Version};
use hyper_util::rt::TokioIo;
use hyper_util::server::graceful::{GracefulConnection, GracefulShutdown};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::TcpListener;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::clock::Clock;
use crate::daemon::Readiness;
use crate::go_compat::duration;
use crate::go_compat::errno::io_error_text;
use crate::go_compat::host_port::split_host_port;
use crate::go_compat::quote::quote;
use crate::logger::{Attr, LogHandle};
use crate::metrics::exposition::{self, Exposition};
use crate::metrics::promhttp::HandlerMetrics;
use crate::metrics::{Recorder, Registry};

use self::gopath::Route;
pub use self::timeouts::{ConnState, TimedIo, Timeouts};

/// The metrics endpoint.
pub const METRICS_PATH: &str = "/metrics";
/// The liveness endpoint.
pub const HEALTHZ_PATH: &str = "/healthz";
/// The readiness endpoint.
pub const READYZ_PATH: &str = "/readyz";

/// How long a stop waits for the open connections: Go's `shutdownTimeout`.
pub const SHUTDOWN_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

const TEXT_PLAIN: &str = "text/plain; charset=utf-8";
const TEXT_HTML: &str = "text/html; charset=utf-8";

/// Go's `bufferBeforeChunkingSize`: a handler that writes at most this much gets a
/// `Content-Length`; anything longer is chunked.
const BUFFER_BEFORE_CHUNKING: usize = 2048;

/// What promhttp reads from a scrape: the first `Accept` header, every `Accept-Encoding` one,
/// and the `name[]` query filter, none when the query names no family.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MetricsRequest {
    /// The first `Accept` header.
    pub accept: Option<String>,
    /// Every `Accept-Encoding` header.
    pub accept_encoding: Vec<String>,
    /// The `name[]` values; a value that is not UTF-8 names no family.
    pub names: Option<BTreeSet<String>>,
}

/// A route only the tests serve, which waits for the future it makes.
#[cfg(test)]
pub(crate) type SlowRoute = Arc<dyn Fn() -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// What the endpoints answer from, and the clock and log the server writes times with.
pub struct App {
    readiness: Readiness,
    metrics: Recorder,
    clock: Arc<dyn Clock>,
    log: LogHandle,
    #[cfg(test)]
    pub(crate) slow: Option<SlowRoute>,
    /// Replaces the registry's answer, so the framing tests choose the body.
    #[cfg(test)]
    pub(crate) exposition: Option<ExpositionFn>,
}

/// A test's `/metrics` answer for a scrape.
#[cfg(test)]
pub(crate) type ExpositionFn = Box<dyn Fn(&MetricsRequest) -> Exposition + Send + Sync>;

impl fmt::Debug for App {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.debug_struct("App").finish_non_exhaustive()
    }
}

impl App {
    /// The endpoints answering from `readiness` and the registry of `metrics`, with `Date` read
    /// from `clock`, and the server's own messages written through `log` and in its zone.
    #[must_use]
    pub fn new(
        readiness: Readiness,
        metrics: Recorder,
        clock: Arc<dyn Clock>,
        log: LogHandle,
    ) -> Self {
        Self {
            readiness,
            metrics,
            clock,
            log,
            #[cfg(test)]
            slow: None,
            #[cfg(test)]
            exposition: None,
        }
    }
}

/// An answer before Go's header order and framing are applied.
struct Reply {
    status: StatusCode,
    headers: Vec<(HeaderName, HeaderValue)>,
    body: Bytes,
    // OPTIONS * sets its Content-Length itself.
    explicit_length: bool,
    // A type the handler did not set, which Go sniffs from the body and writes after
    // Content-Length.
    sniffed_type: Option<HeaderValue>,
}

impl Reply {
    fn new(
        status: StatusCode,
        headers: Vec<(HeaderName, HeaderValue)>,
        body: impl Into<Bytes>,
    ) -> Self {
        Self {
            status,
            headers,
            body: body.into(),
            explicit_length: false,
            sniffed_type: None,
        }
    }

    /// An answer whose handler wrote `body` without setting a type: Go sniffs `text/plain`.
    fn sniffed(status: StatusCode, body: &'static str) -> Self {
        Self {
            sniffed_type: Some(HeaderValue::from_static(TEXT_PLAIN)),
            ..Self::new(status, Vec::new(), body)
        }
    }
}

/// `http.Error(w, "404 page not found", 404)`.
fn not_found() -> Reply {
    Reply::new(
        StatusCode::NOT_FOUND,
        vec![
            (CONTENT_TYPE, HeaderValue::from_static(TEXT_PLAIN)),
            (
                HeaderName::from_static("x-content-type-options"),
                HeaderValue::from_static("nosniff"),
            ),
        ],
        "404 page not found\n",
    )
}

/// `http.Redirect(w, r, location, 307)`. Go compares the method case-sensitively: only `GET`
/// gets the body, and only `GET` and `HEAD` the `Content-Type`.
fn redirect(method: &Method, location: &str) -> Reply {
    let Ok(location_value) = HeaderValue::from_str(location) else {
        // The location is escaped, so always a valid header value.
        return not_found();
    };
    let mut headers = Vec::with_capacity(2);
    let is_get = method.as_str() == "GET";
    if is_get || method.as_str() == "HEAD" {
        headers.push((CONTENT_TYPE, HeaderValue::from_static(TEXT_HTML)));
    }
    headers.push((LOCATION, location_value));
    let body = if is_get {
        format!(
            "<a href=\"{}\">Temporary Redirect</a>.\n\n",
            gopath::html_escape(location)
        )
    } else {
        String::new()
    };
    Reply::new(StatusCode::TEMPORARY_REDIRECT, headers, body)
}

/// Go's `/healthz` handler: always `ok`, its type sniffed.
fn healthz() -> Reply {
    Reply::sniffed(StatusCode::OK, "ok")
}

/// Go's `/readyz` handler: `ok` once ready, `not ready` with a 503 before, its type sniffed.
fn readyz(app: &App) -> Reply {
    if app.readiness.is_ready() {
        Reply::sniffed(StatusCode::OK, "ok")
    } else {
        Reply::sniffed(StatusCode::SERVICE_UNAVAILABLE, "not ready")
    }
}

/// promhttp's handler: the exposition, with its `Content-Type` and, when compressed, its
/// `Content-Encoding`.
fn metrics<B>(app: &App, request: &Request<B>) -> Reply {
    let header_text = |value: &HeaderValue| String::from_utf8_lossy(value.as_bytes()).into_owned();
    let names = request.uri().query().and_then(|query| {
        gopath::parse_query(query)
            .remove(b"name[]".as_slice())
            .map(|names| {
                names
                    .into_iter()
                    .filter_map(|name| String::from_utf8(name).ok())
                    .collect()
            })
    });
    let scrape = MetricsRequest {
        accept: request.headers().get(ACCEPT).map(header_text),
        accept_encoding: request
            .headers()
            .get_all(ACCEPT_ENCODING)
            .iter()
            .map(header_text)
            .collect(),
        names,
    };
    #[cfg(test)]
    if let Some(answer) = &app.exposition {
        return metrics_reply(answer(&scrape));
    }
    metrics_reply(scrape_metrics(&app.metrics, &scrape))
}

/// The `/metrics` answer for `exposition`.
fn metrics_reply(exposition: Exposition) -> Reply {
    let mut headers = Vec::with_capacity(2);
    if let Some(encoding) = exposition.content_encoding {
        headers.push((CONTENT_ENCODING, HeaderValue::from_static(encoding)));
    }
    if let Ok(content_type) = HeaderValue::from_str(&exposition.content_type) {
        headers.push((CONTENT_TYPE, content_type));
    }
    Reply::new(StatusCode::OK, headers, exposition.body)
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

/// The patterns the mux serves.
fn patterns(app: &App) -> &'static [&'static str] {
    #[cfg(test)]
    if app.slow.is_some() {
        return &[METRICS_PATH, HEALTHZ_PATH, READYZ_PATH, SLOW_PATH];
    }
    let _ = app;
    &[METRICS_PATH, HEALTHZ_PATH, READYZ_PATH]
}

/// The test-only route that waits.
#[cfg(test)]
const SLOW_PATH: &str = "/slow";

/// Routes one request as the Go daemon's mux does.
fn handle<B>(app: &App, request: &Request<B>) -> Reply {
    let method = request.method();
    // serverHandler.ServeHTTP hands `OPTIONS *` to the global options handler.
    if method == Method::OPTIONS && request.uri().path() == "*" {
        let mut reply = Reply::new(
            StatusCode::OK,
            vec![(CONTENT_LENGTH, HeaderValue::from_static("0"))],
            Bytes::new(),
        );
        reply.explicit_length = true;
        return reply;
    }
    // CONNECT's target is an authority, which matches no path pattern.
    if method == Method::CONNECT {
        return not_found();
    }
    match gopath::route(request.uri().path(), request.uri().query(), patterns(app)) {
        Route::Matched(METRICS_PATH) => metrics(app, request),
        Route::Matched(HEALTHZ_PATH) => healthz(),
        Route::Matched(READYZ_PATH) => readyz(app),
        // Only the tests' slow route, which waited before the request got here.
        Route::Matched(_) => Reply::new(
            StatusCode::OK,
            vec![(CONTENT_TYPE, HeaderValue::from_static(TEXT_PLAIN))],
            "slow done\n",
        ),
        Route::Redirect(location) => redirect(method, &location),
        Route::NotFound => not_found(),
        Route::BadRequest => Reply::new(
            StatusCode::BAD_REQUEST,
            vec![
                (CONTENT_TYPE, HeaderValue::from_static(TEXT_PLAIN)),
                (CONNECTION, HeaderValue::from_static("close")),
            ],
            "400 Bad Request",
        ),
    }
}

/// Whether an HTTP/1.1 request asked for the connection to close, as Go's `wantsClose` reads its
/// `Connection` tokens.
fn wants_close<B>(request: &Request<B>) -> bool {
    request.version() == Version::HTTP_11
        && request.headers().get_all(CONNECTION).iter().any(|value| {
            String::from_utf8_lossy(value.as_bytes())
                .split(',')
                .any(|token| token.trim().eq_ignore_ascii_case("close"))
        })
}

/// Go's `http.TimeFormat` of `now`.
fn http_date(clock: &dyn Clock) -> HeaderValue {
    let date = clock
        .now()
        .strftime("%a, %d %b %Y %H:%M:%S GMT")
        .to_string();
    HeaderValue::from_str(&date).unwrap_or_else(|_| HeaderValue::from_static(""))
}

/// The answer in Go's header order and framing, with a body that reports to `state` when hyper
/// is done with it.
fn finish<B>(
    reply: Reply,
    request: &Request<B>,
    date: HeaderValue,
    state: ConnState,
) -> Response<ResponseBody> {
    let Reply {
        status,
        mut headers,
        body,
        explicit_length,
        sniffed_type,
    } = reply;
    // The handler's headers sorted by name, as Go writes them; a sort on the lowercase names
    // gives the same order as one on Go's canonical ones for header-name characters.
    headers.retain(|(name, _)| explicit_length || *name != CONTENT_LENGTH);
    headers.sort_by(|left, right| left.0.as_str().cmp(right.0.as_str()));
    let connection = headers
        .iter()
        .position(|(name, _)| *name == CONNECTION)
        .map(|index| headers.remove(index));
    let chunked = body.len() > BUFFER_BEFORE_CHUNKING;
    let is_head = request.method() == Method::HEAD;
    let mut response = Response::new(ResponseBody {
        data: body.clone(),
        chunked,
        state: Some(state),
    });
    *response.status_mut() = status;
    let map = response.headers_mut();
    for (name, value) in headers {
        map.append(name, value);
    }
    map.insert(DATE, date);
    // Go sets the length of a body it buffered whole, except for a HEAD answer that wrote
    // nothing.
    let head_wrote_nothing = is_head && body.is_empty();
    if !(explicit_length || chunked || head_wrote_nothing) {
        map.insert(CONTENT_LENGTH, HeaderValue::from(body.len()));
    }
    if let Some(sniffed) = sniffed_type {
        map.insert(CONTENT_TYPE, sniffed);
    }
    if let Some((name, value)) = connection {
        map.insert(name, value);
    } else if wants_close(request) {
        map.insert(CONNECTION, HeaderValue::from_static("close"));
    }
    response
}

/// A response body that tells the connection's deadlines when hyper is done with it, and that
/// never reports having ended: hyper 1.12 otherwise treats it as absent, drops a `Content-Length:
/// 0` set by hand and appends its own after every other header.
#[derive(Debug)]
pub struct ResponseBody {
    data: Bytes,
    chunked: bool,
    state: Option<ConnState>,
}

impl Drop for ResponseBody {
    fn drop(&mut self) {
        if let Some(state) = self.state.take() {
            state.on_response_done();
        }
    }
}

impl Body for ResponseBody {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        if self.data.is_empty() {
            return Poll::Ready(None);
        }
        let size = if self.chunked {
            self.data.len().min(BUFFER_BEFORE_CHUNKING)
        } else {
            self.data.len()
        };
        let frame = self.data.split_to(size);
        Poll::Ready(Some(Ok(Frame::data(frame))))
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        if self.chunked {
            SizeHint::default()
        } else {
            SizeHint::with_exact(u64::try_from(self.data.len()).unwrap_or(u64::MAX))
        }
    }
}

/// One HTTP/1 connection over `io`, with Go's deadlines, header case and order.
pub fn connection<I>(
    io: I,
    app: Arc<App>,
    timeouts: Timeouts,
) -> impl GracefulConnection<Error = hyper::Error> + Send + 'static
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let state = ConnState::new(timeouts);
    let io = TokioIo::new(TimedIo::new(io, state.clone()));
    let service = service_fn(move |request: Request<Incoming>| {
        let app = Arc::clone(&app);
        let state = state.clone();
        state.on_request_head();
        async move {
            #[cfg(test)]
            if let Some(slow) = app.slow.as_ref().filter(|_| {
                gopath::route(request.uri().path(), None, patterns(&app))
                    == Route::Matched(SLOW_PATH)
            }) {
                slow().await;
            }
            let reply = handle(&app, &request);
            let date = http_date(&*app.clock);
            Ok::<_, Infallible>(finish(reply, &request, date, state))
        }
    });
    http1::Builder::new()
        .title_case_headers(true)
        .auto_date_header(false)
        // No hyper timer: the deadlines are the I/O wrapper's.
        .header_read_timeout(None)
        .serve_connection(io, service)
}

/// How the drain ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Drained {
    /// Every connection finished before the deadline.
    Clean,
    /// Connections were still open at the deadline, Go's `Shutdown` returning the context's
    /// error.
    DeadlineExceeded,
}

/// How [`serve`] ended: the drain and the deadline it ran to.
#[derive(Debug)]
pub struct Served {
    /// How the drain ended.
    pub drained: Drained,
    /// The drain's deadline.
    pub deadline: Instant,
}

/// Whether Go's `Serve` retries an accept error: `syscall.Errno.Temporary` (`EINTR`, `EMFILE`,
/// `ENFILE`, and the timeouts `EAGAIN` and `ETIMEDOUT`), and `ECONNABORTED`, which Go's poller
/// retries by itself.
fn temporary(error: &io::Error) -> bool {
    matches!(error.raw_os_error(), Some(4 | 11 | 23 | 24 | 103 | 110))
}

/// Go's `log.Printf` line: `2006/01/02 15:04:05 <message>` in the local zone, on stderr.
fn go_log(app: &App, message: &str) {
    let now = app.clock.now();
    let offset = app.log.logger().zone().to_offset(now);
    let civil = offset.to_datetime(now);
    let _ = writeln!(
        io::stderr(),
        "{:04}/{:02}/{:02} {:02}:{:02}:{:02} {message}",
        civil.year(),
        civil.month(),
        civil.day(),
        civil.hour(),
        civil.minute(),
        civil.second()
    );
}

/// Serves `app` on `listener` until `shutdown` resolves, then closes the listener at once, so
/// later connects are refused, and waits for the open connections until `drain_deadline()`,
/// computed when the drain starts. An accept error Go's `Serve` does not retry is handed to
/// `on_error` when it happens and closes the listener, as `Serve` returning does; the open
/// connections are still served until `shutdown`, as Go's are until `Shutdown`.
pub async fn serve(
    listener: TcpListener,
    app: Arc<App>,
    timeouts: Timeouts,
    shutdown: impl Future<Output = ()>,
    drain_deadline: impl FnOnce() -> Instant,
    on_error: impl FnOnce(io::Error),
) -> Served {
    let graceful = GracefulShutdown::new();
    let local = listener
        .local_addr()
        .map_or_else(|_| String::new(), |address| address.to_string());
    let mut shutdown = std::pin::pin!(shutdown);
    let mut retry = std::time::Duration::ZERO;
    loop {
        tokio::select! {
            () = &mut shutdown => break,
            accepted = listener.accept() => match accepted {
                Ok((stream, _)) => {
                    retry = std::time::Duration::ZERO;
                    let watched = graceful.watch(connection(stream, Arc::clone(&app), timeouts));
                    tokio::spawn(async move {
                        let _ = watched.await;
                    });
                }
                Err(accept_error) if temporary(&accept_error) => {
                    // Go's Serve: 5ms, doubling, at most 1s.
                    retry = if retry.is_zero() {
                        std::time::Duration::from_millis(5)
                    } else {
                        (retry * 2).min(std::time::Duration::from_secs(1))
                    };
                    go_log(
                        &app,
                        &format!(
                            "http: Accept error: accept tcp {local}: accept4: {}; retrying in {}",
                            io_error_text(&accept_error),
                            duration::format(i64::try_from(retry.as_nanos()).unwrap_or(i64::MAX))
                        ),
                    );
                    tokio::time::sleep(retry).await;
                }
                Err(accept_error) => {
                    drop(listener);
                    on_error(io::Error::new(
                        accept_error.kind(),
                        format!("accept tcp {local}: accept4: {}", io_error_text(&accept_error)),
                    ));
                    shutdown.await;
                    return drain(graceful, drain_deadline()).await;
                }
            },
        }
    }
    drop(listener);
    drain(graceful, drain_deadline()).await
}

/// Waits for the open connections until `deadline`.
async fn drain(graceful: GracefulShutdown, deadline: Instant) -> Served {
    let drained = match tokio::time::timeout_at(deadline, graceful.shutdown()).await {
        Ok(()) => Drained::Clean,
        Err(_) => Drained::DeadlineExceeded,
    };
    Served { drained, deadline }
}

/// A listen failure: Go's `net.OpError` text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListenError(String);

impl fmt::Display for ListenError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for ListenError {}

/// Go's `LookupPort` for TCP: decimal digits with an optional sign, from 0 to 65535, or one of
/// the service names Go knows without `/etc/services` (its `services` table, matched without
/// regard to case): the daemon's image (`dhi.io/static`) has no `/etc/services`.
/// Go's built-in `services` table for TCP.
const GO_TCP_SERVICES: [(&str, u16); 14] = [
    ("ftp", 21),
    ("ftps", 990),
    ("gopher", 70),
    ("http", 80),
    ("https", 443),
    ("imap2", 143),
    ("imap3", 220),
    ("imaps", 993),
    ("pop3", 110),
    ("pop3s", 995),
    ("smtp", 25),
    ("submissions", 465),
    ("ssh", 22),
    ("telnet", 23),
];

fn parse_port(port: &str) -> Result<u16, String> {
    let (negative, digits) = match port.as_bytes().first() {
        Some(b'+') => (false, &port[1..]),
        Some(b'-') => (true, &port[1..]),
        _ => (false, port),
    };
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return GO_TCP_SERVICES
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case(port))
            .map(|(_, number)| *number)
            .ok_or_else(|| format!("lookup tcp/{port}: unknown port"));
    }
    let value = digits.bytes().fold(0_u32, |value, digit| {
        value
            .saturating_mul(10)
            .saturating_add(u32::from(digit - b'0'))
    });
    match u16::try_from(value) {
        Ok(0) => Ok(0),
        Ok(value) if !negative => Ok(value),
        Ok(_) | Err(_) => Err(format!("address {port}: invalid port")),
    }
}

/// `net.Listen("tcp", address)`, which `TcpListener::bind` is not: an empty or unspecified host
/// (`0.0.0.0`, `[::]`, `[::ffff:0.0.0.0]`) is the dual-stack wildcard `[::]` (`0.0.0.0` only
/// where IPv6 is missing), an IPv4-mapped address listens as IPv4, an IPv6 zone is an interface
/// name or number, a host name listens on its first IPv4 address, `SO_REUSEADDR` is on, and the
/// backlog is the kernel's limit, as Go reads `somaxconn`.
///
/// # Errors
///
/// A [`ListenError`] with Go's wording for the address errors and the failing system call; a
/// host name that does not resolve has the resolver's wording (Q12).
pub async fn bind(address: &str) -> Result<TcpListener, ListenError> {
    let (host, port) =
        split_host_port(address).map_err(|error| ListenError(format!("listen tcp: {error}")))?;
    let port = parse_port(port).map_err(|error| ListenError(format!("listen tcp: {error}")))?;
    let target = match literal(host, port) {
        Some(target) => target,
        None => resolve(host, port).await?,
    };
    let (candidates, shown, dual_stack) = match target {
        Target::Wildcard { shown } => (
            vec![
                SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port),
                SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port),
            ],
            shown,
            true,
        ),
        Target::One { address, shown } => (vec![address], shown, false),
    };
    let mut last = ListenError(format!("listen tcp {shown}: no suitable address"));
    for candidate in candidates {
        match listen_on(candidate, dual_stack) {
            Ok(listener) => return Ok(listener),
            Err((call, error)) => {
                last = ListenError(format!(
                    "listen tcp {shown}: {call}: {}",
                    io_error_text(&error)
                ));
                // Only a host without IPv6 (EAFNOSUPPORT) falls back to 0.0.0.0: an address in
                // use on [::] must not quietly bind IPv4 only.
                if error.raw_os_error() != Some(97) {
                    return Err(last);
                }
            }
        }
    }
    Err(last)
}

/// Where [`bind`] listens, and the address Go's errors show for it.
enum Target {
    /// Go's wildcard: `[::]` with IPv4 mapped, else `0.0.0.0`.
    Wildcard { shown: String },
    /// One address.
    One { address: SocketAddr, shown: String },
}

/// Go's target for an IP: the wildcard for an unspecified one (shown `0.0.0.0`, as Go shows it
/// for `[::]` too), IPv4 for a mapped one, and the zone for IPv6, shown as given.
fn target_of(ip: IpAddr, zone: Option<&str>, port: u16) -> Target {
    let ip = ip.to_canonical();
    if ip.is_unspecified() {
        return Target::Wildcard {
            shown: format!("0.0.0.0:{port}"),
        };
    }
    match ip {
        IpAddr::V4(_) => Target::One {
            address: SocketAddr::new(ip, port),
            shown: format!("{ip}:{port}"),
        },
        IpAddr::V6(ipv6) => {
            let scope = zone.map_or(0, zone_index);
            let shown = match zone {
                Some(zone) => format!("[{ipv6}%{zone}]:{port}"),
                None => format!("[{ipv6}]:{port}"),
            };
            Target::One {
                address: SocketAddr::V6(SocketAddrV6::new(ipv6, port, 0, scope)),
                shown,
            }
        }
    }
}

/// The target of an empty host or an IP literal, which Go never resolves; `None` for a name.
/// A zone is IPv6's only, so `127.0.0.1%lo` is a name, as Go's `ParseIP` makes it.
fn literal(host: &str, port: u16) -> Option<Target> {
    if host.is_empty() {
        return Some(Target::Wildcard {
            shown: format!(":{port}"),
        });
    }
    let (text, zone) = match host.split_once('%') {
        Some((text, zone)) => (text, Some(zone)),
        None => (host, None),
    };
    match text.parse::<IpAddr>() {
        Ok(ip @ IpAddr::V6(_)) => Some(target_of(ip, zone, port)),
        Ok(ip @ IpAddr::V4(_)) if zone.is_none() => Some(target_of(ip, None, port)),
        Ok(IpAddr::V4(_)) | Err(_) => None,
    }
}

/// Go's resolution of a host name for `Listen`: its first IPv4 address, else its first.
async fn resolve(host: &str, port: u16) -> Result<Target, ListenError> {
    // getaddrinfo reads `inet_aton`'s shorthands (`0`, `127.1`, `0x7f.1`) as addresses; Go's own
    // resolver looks them up as names, which no DNS server answers.
    if inet_aton_form(host) {
        return Err(ListenError(format!(
            "listen tcp: lookup {host}: no such host"
        )));
    }
    let resolved: Vec<SocketAddr> = tokio::net::lookup_host((host, port))
        .await
        .map_err(|error| ListenError(format!("listen tcp: lookup {host}: {error}")))?
        .collect();
    resolved
        .iter()
        .find(|address| address.is_ipv4())
        .or_else(|| resolved.first())
        .map(|address| target_of(address.ip(), None, port))
        .ok_or_else(|| ListenError(format!("listen tcp: lookup {host}: no such host")))
}

/// Whether `host` has `inet_aton`'s numeric shape: one to four dot-separated parts, each decimal,
/// `0`-prefixed octal or `0x`-prefixed hexadecimal.
fn inet_aton_form(host: &str) -> bool {
    let parts: Vec<&str> = host.split('.').collect();
    parts.len() <= 4
        && parts.iter().all(|part| {
            match part.strip_prefix("0x").or_else(|| part.strip_prefix("0X")) {
                Some(hex) => hex.bytes().all(|byte| byte.is_ascii_hexdigit()),
                None => !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()),
            }
        })
}

/// Go's cap on a zone's number (`big` in its `dtoi`).
const ZONE_INDEX_CAP: u32 = 0x00FF_FFFF;

/// Go's `zoneCache.index`: the interface of that name, else the zone's leading decimal digits
/// (capped as Go's `dtoi` caps them), else 0.
fn zone_index(zone: &str) -> u32 {
    let named = rustix::net::socket(
        rustix::net::AddressFamily::INET,
        rustix::net::SocketType::DGRAM,
        None,
    )
    .and_then(|socket| rustix::net::netdevice::name_to_index(&socket, zone));
    if let Ok(index) = named {
        return index;
    }
    let mut index = 0_u32;
    for byte in zone.bytes().take_while(u8::is_ascii_digit) {
        index = index * 10 + u32::from(byte - b'0');
        if index >= ZONE_INDEX_CAP {
            return ZONE_INDEX_CAP;
        }
    }
    index
}

/// One listening socket on `address`, with the system call that failed.
fn listen_on(
    address: SocketAddr,
    dual_stack: bool,
) -> Result<TcpListener, (&'static str, io::Error)> {
    let socket = Socket::new(
        Domain::for_address(address),
        Type::STREAM,
        Some(Protocol::TCP),
    )
    .map_err(|error| ("socket", error))?;
    socket
        .set_reuse_address(true)
        .map_err(|error| ("setsockopt", error))?;
    if dual_stack && address.is_ipv6() {
        socket
            .set_only_v6(false)
            .map_err(|error| ("setsockopt", error))?;
    }
    socket
        .set_nonblocking(true)
        .map_err(|error| ("setsockopt", error))?;
    socket
        .bind(&address.into())
        .map_err(|error| ("bind", error))?;
    // The kernel caps it at somaxconn, which is what Go asks for.
    socket
        .listen(i32::from(u16::MAX))
        .map_err(|error| ("listen", error))?;
    TcpListener::from_std(socket.into()).map_err(|error| ("listen", error))
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
    drain: std::time::Duration,
) -> Result<MetricsServer, StartError> {
    let listener = bind(address).await.map_err(|cause| StartError {
        address: address.to_owned(),
        cause,
    })?;
    let log = app.log.clone();
    let local = listener.local_addr().ok();
    log.info(
        "metrics server listening",
        &[Attr::string(
            "addr",
            local.map_or_else(String::new, |local| local.to_string()),
        )],
    );
    let stop = context.child_token();
    let shutdown = stop.clone();
    let task = tokio::spawn(async move {
        let served = serve(
            listener,
            Arc::new(app),
            Timeouts::DAEMON,
            shutdown.cancelled_owned(),
            move || Instant::now() + drain,
            |error| log.error("metrics server error", &[Attr::error("err", &error)]),
        )
        .await;
        if served.drained == Drained::DeadlineExceeded {
            log.warn(
                "metrics server shutdown error",
                &[Attr::string("err", "context deadline exceeded")],
            );
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
