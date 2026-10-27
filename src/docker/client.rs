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

// Adapted from swarm-scheduler-exporter/src/docker/client.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The client: its socket, the one request builder, the ping and moby's lazy API version
//! negotiation, and moby's error wording.
//!
//! Every request carries moby client v0.6.0's headers, `User-Agent: moby-client/v0.6.0
//! <os>/<arch>` included: the Go daemon sent exactly these, and the Engine and anything logging
//! its requests see the same client after the port (swarm-scheduler-exporter names itself
//! instead, its Q11).

use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::task::{Context, Poll};

use bytes::{Bytes, BytesMut};
use http_body_util::{BodyExt as _, Empty, Limited};
use hyper::body::{Frame, SizeHint};
use hyper::header::{ACCEPT, CONTENT_TYPE, HOST, HeaderValue, USER_AGENT};
use hyper::{Method, Request, Response, StatusCode};

use super::DUMMY_HOST;
use super::connect::Connector;
use super::context;
use super::error::{ConfigError, Error};
use super::host::{escape_host, escape_path, join_path, unix_socket};
use super::pool::{Pool, PooledBody, SendError};
use super::version::{self, MAX_API_VERSION};
use crate::go_compat::errno::io_error_text;
use crate::go_compat::http_status::status_text;
use crate::go_compat::quote::quote;

/// moby's `checkResponseErr` reads at most this much of an error body.
const ERROR_BODY_LIMIT: usize = 1024 * 1024;

/// The moby client release whose requests the client sends.
const MOBY_CLIENT_VERSION: &str = "v0.6.0";

/// Where the client connects: the daemon's unix socket.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    socket: String,
}

impl Settings {
    /// moby's `New(WithHost("unix://" + socket))`, as the Go daemon builds its client: the
    /// socket path, refused when empty or longer than the kernel takes.
    ///
    /// # Errors
    ///
    /// [`ConfigError`] with moby's message.
    pub fn unix(socket: &str) -> Result<Self, ConfigError> {
        Ok(Self {
            socket: unix_socket(socket).map_err(ConfigError::new)?,
        })
    }

    /// moby's `DaemonHost`: `unix://<socket>`.
    #[must_use]
    pub fn daemon_host(&self) -> String {
        format!("unix://{}", self.socket)
    }

    /// The socket path.
    #[must_use]
    pub fn socket(&self) -> &str {
        &self.socket
    }
}

/// The negotiated API version, shared by every request.
#[derive(Debug)]
struct Negotiation {
    state: tokio::sync::Mutex<NegotiationState>,
    // Bumped after every ping, so a request that waited for one shares its outcome.
    pings: AtomicU64,
}

#[derive(Debug)]
struct NegotiationState {
    version: String,
    negotiated: bool,
}

/// The read-only Docker client.
#[derive(Debug)]
pub struct Client {
    settings: Settings,
    pool: Arc<Pool>,
    user_agent: HeaderValue,
    negotiation: Negotiation,
}

/// A response body: read it to the end and the connection is reused.
#[derive(Debug)]
pub struct Body {
    inner: PooledBody,
}

impl hyper::body::Body for Body {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        Pin::new(&mut self.inner)
            .poll_frame(context)
            .map(|frame| frame.map(|frame| frame.map_err(|error| body_error(&error))))
    }

    fn is_end_stream(&self) -> bool {
        self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

fn body_error(error: &hyper::Error) -> Error {
    Error::body(http_error_text(error))
}

/// Go's `runtime.GOARCH` for the architecture the binary runs on, as moby's `User-Agent` names
/// it.
pub(super) fn go_arch() -> &'static str {
    match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        "x86" => "386",
        "powerpc64" if cfg!(target_endian = "little") => "ppc64le",
        "powerpc64" => "ppc64",
        "loongarch64" => "loong64",
        other => other,
    }
}

/// moby's default `User-Agent`: `moby-client/<version> <GOOS>/<GOARCH>`.
fn user_agent() -> String {
    format!(
        "moby-client/{MOBY_CLIENT_VERSION} {}/{}",
        std::env::consts::OS,
        go_arch()
    )
}

impl Client {
    /// A client for `settings`, with no connection open yet.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self::with_pool(settings, Pool::new)
    }

    pub(super) fn with_pool(settings: Settings, pool: impl FnOnce(Connector) -> Arc<Pool>) -> Self {
        let connector = Connector::new(settings.socket.clone());
        Self {
            pool: pool(connector),
            user_agent: HeaderValue::from_str(&user_agent())
                .unwrap_or_else(|_| HeaderValue::from_static("moby-client")),
            negotiation: Negotiation {
                state: tokio::sync::Mutex::new(NegotiationState {
                    version: MAX_API_VERSION.to_owned(),
                    negotiated: false,
                }),
                pings: AtomicU64::new(0),
            },
            settings,
        }
    }

    /// The settings the client was built from.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// The API version requests use now: the negotiated one, else [`MAX_API_VERSION`].
    pub async fn client_version(&self) -> String {
        self.negotiation.state.lock().await.version.clone()
    }

    /// The one request builder: `GET` with Go's headers in Go's order (`Host`, `User-Agent`,
    /// `Accept`). Only [`Client::ping`] changes the method, to `HEAD`.
    fn request(&self, target: &str, accept: &[&'static str]) -> Request<Empty<Bytes>> {
        let mut request = Request::new(Empty::new());
        *request.method_mut() = Method::GET;
        *request.uri_mut() = target
            .parse()
            .unwrap_or_else(|_| hyper::Uri::from_static("/"));
        let headers = request.headers_mut();
        headers.insert(HOST, HeaderValue::from_static(DUMMY_HOST));
        headers.insert(USER_AGENT, self.user_agent.clone());
        for value in accept {
            headers.append(ACCEPT, HeaderValue::from_static(value));
        }
        request
    }

    /// The URL moby's errors name: the socket path as the escaped host.
    fn url(&self, target: &str) -> String {
        format!("http://{}{target}", escape_host(&self.settings.socket))
    }

    /// Sends the request `build` makes (again, if a reused connection fails first), mapping a
    /// failure to moby's wording.
    async fn send(
        &self,
        target: &str,
        build: impl Fn() -> Request<Empty<Bytes>>,
    ) -> Result<Response<Body>, Error> {
        // net/http's url.Error names the method this way: "Get", "Head".
        let method = build().method().as_str().to_owned();
        let operation = format!("{}{}", &method[..1], method[1..].to_ascii_lowercase());
        let sent = async {
            self.pool
                .send(build)
                .await
                .map_err(|failure| self.connection_error(&operation, &self.url(target), &failure))
        };
        // A request under a caller's context fails as net/http fails it once the context is
        // done, before or during the exchange.
        let response = match context::current() {
            Some(call) => tokio::select! {
                biased;
                done = call.done() => {
                    return Err(Error::context_request(done, &operation, &self.url(target)));
                }
                response = sent => response?,
            },
            None => sent.await?,
        };
        let (parts, inner) = response.into_parts();
        Ok(Response::from_parts(parts, Body { inner }))
    }

    /// moby's `ping`: `HEAD /_ping`, used as is on a 200; on a failure or any other status,
    /// `GET /_ping`, whose failure fails the ping and whose status of 400 or above is moby's
    /// response error. Returns the `Api-Version` header, empty when absent. It is moby's `Ping`
    /// without `NegotiateAPIVersion`, which the Go daemon calls before every subscription, and
    /// the first step of the negotiation.
    ///
    /// # Errors
    ///
    /// [`Error`] when `GET /_ping` gets no response or a status of 400 or above.
    pub async fn ping(&self) -> Result<String, Error> {
        let target = escape_path(&join_path(&["/_ping"]));
        // The only HEAD the client sends.
        let head = || {
            let mut request = self.request(&target, &[]);
            *request.method_mut() = Method::HEAD;
            request
        };
        if let Ok(response) = self.send(&target, head).await {
            let status = response.status();
            let api_version = api_version_header(&response);
            ensure_reader_closed(response).await;
            if status == StatusCode::OK {
                return Ok(api_version);
            }
        }
        let response = self.send(&target, || self.request(&target, &[])).await?;
        let api_version = api_version_header(&response);
        if (200..400).contains(&response.status().as_u16()) {
            ensure_reader_closed(response).await;
            return Ok(api_version);
        }
        self.check_response(response, "GET", &self.url(&target))
            .await?;
        Ok(api_version)
    }

    /// moby's `checkVersion`: negotiates once, and again on the next request after a failure,
    /// which moby discards (`_ = cli.checkVersion(ctx)`): the request then goes out at the
    /// current version, [`MAX_API_VERSION`] until a negotiation succeeds. Concurrent requests
    /// share one ping and its outcome, where moby sends one each; the Go daemon's first versioned
    /// request is always the event subscription, alone, so the requests are the same.
    async fn api_version(&self) -> String {
        let pings = self.negotiation.pings.load(Ordering::Acquire);
        let mut state = self.negotiation.state.lock().await;
        if state.negotiated || self.negotiation.pings.load(Ordering::Acquire) != pings {
            return state.version.clone();
        }
        if let Ok(ping_version) = self.ping().await
            && let Ok(negotiated) = version::negotiate(&ping_version)
        {
            state.version = negotiated;
            state.negotiated = true;
        }
        self.negotiation.pings.fetch_add(1, Ordering::Release);
        state.version.clone()
    }

    /// A versioned `GET` of `path` (with `query`, already encoded, and `accept` header lines):
    /// the response once its headers are in, or moby's error for a status of 400 or above.
    ///
    /// # Errors
    ///
    /// [`Error`] for no response, as moby words it, or a response error.
    pub async fn get(
        &self,
        path: &str,
        query: &str,
        accept: &[&'static str],
    ) -> Result<Response<Body>, Error> {
        let version = self.api_version().await;
        let api_path = join_path(&[
            &format!("/v{}", version.strip_prefix('v').unwrap_or(&version)),
            path,
        ]);
        let mut target = escape_path(&api_path);
        if !query.is_empty() {
            target.push('?');
            target.push_str(query);
        }
        let response = self.send(&target, || self.request(&target, accept)).await?;
        if response.status().as_u16() < 400 {
            return Ok(response);
        }
        let url = self.url(&target);
        Err(self
            .check_response(response, "GET", &url)
            .await
            .err()
            .unwrap_or_else(|| Error::response(StatusCode::INTERNAL_SERVER_ERROR, String::new())))
    }

    /// moby's `checkResponseErr`: a status from 200 to 399 is fine; otherwise the error carries
    /// the daemon's message (the JSON `message` for `application/json`, else the body), or says
    /// it is missing or too large.
    async fn check_response(
        &self,
        response: Response<Body>,
        method: &str,
        url: &str,
    ) -> Result<(), Error> {
        let status = response.status();
        if (200..400).contains(&status.as_u16()) {
            return Ok(());
        }
        let status_message = go_status(&response);
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let mut body = BytesMut::new();
        if method != "HEAD" {
            let mut limited = Limited::new(response.into_body(), ERROR_BODY_LIMIT);
            while let Some(frame) = limited.frame().await {
                match frame {
                    Ok(frame) => {
                        if let Ok(data) = frame.into_data() {
                            body.extend_from_slice(&data);
                        }
                    }
                    Err(error) if error.is::<http_body_util::LengthLimitError>() => {
                        return Err(Error::response(
                            status,
                            format!(
                                "request returned {status_message} with a message (> \
                                 {ERROR_BODY_LIMIT} bytes) for API route and version {url}, \
                                 check if the server supports the requested API version"
                            ),
                        ));
                    }
                    Err(error) => return Err(Error::response(status, error.to_string())),
                }
            }
        }
        if body.len() >= ERROR_BODY_LIMIT {
            return Err(Error::response(
                status,
                format!(
                    "request returned {status_message} with a message (> {ERROR_BODY_LIMIT} \
                     bytes) for API route and version {url}, check if the server supports the \
                     requested API version"
                ),
            ));
        }
        if body.is_empty() {
            return Err(Error::response(
                status,
                format!(
                    "request returned {status_message} for API route and version {url}, check if \
                     the server supports the requested API version"
                ),
            ));
        }
        let daemon_message = if content_type.as_deref() == Some("application/json") {
            match serde_json::from_slice::<ErrorResponse>(&body) {
                Err(error) => {
                    return Err(Error::response(
                        status,
                        format!("error reading JSON: {error}"),
                    ));
                }
                Ok(ErrorResponse { message })
                    if message.as_deref().unwrap_or_default().is_empty() =>
                {
                    format!(
                        "API returned a {} ({}) but provided no error-message",
                        status.as_u16(),
                        status_text(status.as_u16())
                    )
                }
                Ok(ErrorResponse { message }) => message.unwrap_or_default().trim().to_owned(),
            }
        } else {
            String::from_utf8_lossy(&body).trim().to_owned()
        };
        Err(Error::response(
            status,
            format!("Error response from daemon: {daemon_message}"),
        ))
    }

    /// moby's `doRequest` wording of a request that got no response.
    fn connection_error(&self, operation: &str, url: &str, failure: &SendError) -> Error {
        let host = self.settings.daemon_host();
        let inner = match failure {
            SendError::Connect(error) => error.to_string(),
            SendError::Http(error) => http_error_text(error),
        };
        let url_error = format!("{operation} {}: {inner}", quote(url));
        if matches!(failure, SendError::Http(error) if error.is_parse()) {
            return Error::connection_failed(format!(
                "{url_error}.\n* Are you trying to connect to a TLS-enabled daemon without TLS?"
            ));
        }
        if let SendError::Connect(error) = failure
            && let Some(dial) = error.dial_error()
        {
            match dial.raw_os_error() {
                Some(1 | 13) => {
                    return Error::connection_failed(format!(
                        "permission denied while trying to connect to the docker API at {host}"
                    ));
                }
                Some(2) => {
                    return Error::connection_failed(format!(
                        "failed to connect to the docker API at {host}; check if the path is \
                         correct and if the daemon is running: {inner}"
                    ));
                }
                _ => {}
            }
        }
        let timeout = match failure {
            SendError::Connect(error) => error.is_timeout(),
            SendError::Http(_) => false,
        };
        if timeout || inner.contains("connection refused") || inner.contains("dial unix") {
            return Error::connection_failed(format!(
                "Cannot connect to the Docker daemon at {host}. Is the docker daemon running?"
            ));
        }
        Error::connection_failed(format!("error during connect: {url_error}"))
    }
}

/// The text of a failed exchange, as close to Go's as hyper allows: a connection closed before a
/// response is Go's `EOF`, a malformed response is net/http's, and an I/O error is its errno text.
/// moby's `ensureReaderClosed`: up to 512 bytes of the body read and dropped, so a short body
/// (the ping's `OK`) leaves the connection reusable, as it does in Go.
async fn ensure_reader_closed(response: Response<Body>) {
    const DRAIN_LIMIT: usize = 512;
    let _ = Limited::new(response.into_body(), DRAIN_LIMIT)
        .collect()
        .await;
}

fn http_error_text(error: &hyper::Error) -> String {
    if error.is_parse() {
        return "net/http: HTTP/1.x transport connection broken: malformed HTTP response"
            .to_owned();
    }
    if error.is_incomplete_message() || error.is_closed() || error.is_canceled() {
        return "EOF".to_owned();
    }
    let mut source = std::error::Error::source(error);
    while let Some(cause) = source {
        if let Some(io) = cause.downcast_ref::<std::io::Error>() {
            return io_error_text(io);
        }
        source = cause.source();
    }
    error.to_string()
}

/// The `Api-Version` header, as Go's `Header.Get` reads it: the first value, empty when absent.
fn api_version_header<B>(response: &Response<B>) -> String {
    response
        .headers()
        .get("Api-Version")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned()
}

/// Go's `resp.Status`: the code and the reason from the wire, which hyper keeps only when it is
/// not the canonical one.
fn go_status<B>(response: &Response<B>) -> String {
    let code = response.status().as_u16();
    let reason = response
        .extensions()
        .get::<hyper::ext::ReasonPhrase>()
        .map(|reason| String::from_utf8_lossy(reason.as_bytes()).into_owned())
        .or_else(|| response.status().canonical_reason().map(str::to_owned))
        .unwrap_or_default();
    if reason.is_empty() {
        code.to_string()
    } else {
        format!("{code} {reason}")
    }
}

/// moby's `common.ErrorResponse`.
#[derive(Debug, serde::Deserialize)]
struct ErrorResponse {
    // A missing or null message is Go's empty string.
    #[serde(default)]
    message: Option<String>,
}
