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

// Adapted from swarm-rootless-docker/src/docker/mod.rs at 16913a4d87d8,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The launcher's write-capable Engine client: the eight calls of [`EngineApi`], the only Docker
//! writes in the project, sent as moby client v0.6.0 sends the Go launcher's calls.
//!
//! [`Engine`] shares the daemon client's transport (its socket, pool, negotiation and error
//! wording) through two crate-private hooks of [`Client`], and only this module calls them; only
//! `src/launcher/**` may name this module (`meta/tests/launcher_boundary.rs`). The requests, as
//! `tests/testdata/engine_wire/api-1.56/launcher.jsonl` recorded the Go launcher's:
//!
//! - `GET /containers/{id}/json`, and `GET /containers/json?all=1&filters=...` with the name
//!   filter;
//! - `DELETE /containers/{id}?force=1`;
//! - `POST /containers/create?name=...` with the JSON body of the daemon spec;
//! - `POST /containers/{id}/attach?stderr=1&stdout=1&stream=1`, upgraded to a raw stream on a
//!   connection of its own (moby's hijack);
//! - `POST /containers/{id}/wait?condition=removed`, whose answer is read in the background;
//! - `POST /containers/{id}/start` and `POST /containers/{id}/stop?t=<seconds>`.
//!
//! net/http sends `Content-Length: 0` with an empty `POST`, and hyper sends none, so the empty
//! `POST`s set it. Only what the launcher reads is decoded: an inspect's `Image` and
//! `State.Running`, a summary's `Id` and `Labels`, the create's `Id` and `Warnings`, the wait's
//! `StatusCode` and `Error.Message`.

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full, Limited};
use hyper::header::{CONNECTION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, UPGRADE};
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde::Deserialize;
use tokio::io::{AsyncRead, ReadBuf};

use super::spec::{DAEMON_NAME, DaemonSpec};
use crate::docker::types::{Labels, decode_first, decode_list, null_default};
use crate::docker::{Body, Client, Error, Filters, encode_query, trim_id};
use crate::go_compat::errno::io_error_text;

/// moby's `containerWaitErrorMsgLimit`: how much more of a wait answer that is not JSON is read
/// for the error.
const WAIT_ERROR_LIMIT: usize = 2 * 1024;

/// moby's `ensureReaderClosed`: how much of a body that is not read is drained, so the
/// connection is reused.
const DRAIN_LIMIT: usize = 512;

/// The daemon's output stream: what the attach upgraded to, read until the daemon's output ends.
pub type Attached = Box<dyn AsyncRead + Send + Unpin>;

/// The answer of a wait, once the condition is met; the error is moby's, as its error channel
/// carried it.
pub type WaitAnswer = Pin<Box<dyn Future<Output = Result<WaitResponse, String>> + Send>>;

/// container `InspectResponse`: what the launcher reads of it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Inspected {
    /// `Image`: the ID of the image the container runs.
    #[serde(rename = "Image", deserialize_with = "null_default")]
    pub image: String,
    /// `State`, absent when the Engine reported none.
    #[serde(rename = "State")]
    pub state: Option<State>,
}

/// container `State`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct State {
    /// `Running`.
    #[serde(rename = "Running", deserialize_with = "null_default")]
    pub running: bool,
}

/// container `Summary`: what the launcher reads of a listed container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Summary {
    /// `Id`.
    #[serde(rename = "Id", deserialize_with = "null_default")]
    pub id: String,
    /// `Labels`.
    #[serde(rename = "Labels", deserialize_with = "null_default")]
    pub labels: Labels,
}

/// container `CreateResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Created {
    /// `Id`.
    #[serde(rename = "Id", deserialize_with = "null_default")]
    pub id: String,
    /// `Warnings`.
    #[serde(rename = "Warnings", deserialize_with = "null_default")]
    pub warnings: Vec<String>,
}

/// container `WaitResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WaitResponse {
    /// `StatusCode`.
    #[serde(rename = "StatusCode", deserialize_with = "null_default")]
    pub status_code: i64,
    /// `Error`, absent unless the wait itself failed.
    #[serde(rename = "Error")]
    pub error: Option<WaitExitError>,
}

/// container `WaitExitError`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct WaitExitError {
    /// `Message`.
    #[serde(rename = "Message", deserialize_with = "null_default")]
    pub message: String,
}

/// The Docker calls the launcher makes, each by container ID. Every call but the two streams is
/// one request and response, which the caller bounds; [`EngineApi::attach`] and
/// [`EngineApi::wait_removed`] resolve once their stream is established, and the stream itself
/// has no deadline.
pub trait EngineApi: Send + Sync {
    /// moby's `ContainerInspect`.
    fn inspect(&self, id: &str) -> impl Future<Output = Result<Inspected, Error>> + Send;

    /// moby's `ContainerList` with `All` and the `name` filter `^/swarm-device-access$`: the
    /// daemon containers in every state.
    fn list_daemons(&self) -> impl Future<Output = Result<Vec<Summary>, Error>> + Send;

    /// moby's `ContainerRemove` with `Force`.
    fn remove(&self, id: &str) -> impl Future<Output = Result<(), Error>> + Send;

    /// moby's `ContainerCreate` of `spec`, named [`DAEMON_NAME`].
    fn create(&self, spec: &DaemonSpec) -> impl Future<Output = Result<Created, Error>> + Send;

    /// moby's `ContainerAttach` with `Stream`, `Stdout` and `Stderr`: the multiplexed output.
    fn attach(&self, id: &str) -> impl Future<Output = Result<Attached, Error>> + Send;

    /// moby's `ContainerWait` for the `removed` condition: resolves once the Engine accepted the
    /// wait, with its answer to come.
    fn wait_removed(&self, id: &str) -> impl Future<Output = Result<WaitAnswer, Error>> + Send;

    /// moby's `ContainerStart`.
    fn start(&self, id: &str) -> impl Future<Output = Result<(), Error>> + Send;

    /// moby's `ContainerStop` with `Timeout` `timeout_seconds`.
    fn stop(
        &self,
        id: &str,
        timeout_seconds: i64,
    ) -> impl Future<Output = Result<(), Error>> + Send;
}

/// The Engine on the launcher's socket.
#[derive(Debug)]
pub struct Engine {
    client: Client,
}

impl Engine {
    /// The launcher's engine on `client`'s transport.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    async fn call(
        &self,
        path: &str,
        query: &str,
        shape: impl Fn(&mut Request<Full<Bytes>>),
    ) -> Result<Response<Body>, Error> {
        self.client.exchange(path, query, shape).await
    }

    /// A `POST` with no body and the `Content-Length: 0` net/http sends with it, its answer
    /// drained.
    async fn post_empty(&self, path: &str, query: &str) -> Result<(), Error> {
        let response = self.call(path, query, empty_post).await?;
        drain(response).await;
        Ok(())
    }
}

impl EngineApi for Engine {
    async fn inspect(&self, id: &str) -> Result<Inspected, Error> {
        let id = trim_id("container", id)?;
        let response = self
            .call(&format!("/containers/{id}/json"), "", |_| {})
            .await?;
        decode_first(&read_body(response).await?).map_err(Error::decode)
    }

    async fn list_daemons(&self) -> Result<Vec<Summary>, Error> {
        let filters = Filters::new().add("name", &[&format!("^/{DAEMON_NAME}$")]);
        let query = encode_query(&mut [("all", "1".to_owned()), ("filters", filters.to_json())]);
        let response = self.call("/containers/json", &query, |_| {}).await?;
        decode_list(&read_body(response).await?).map_err(Error::decode)
    }

    async fn remove(&self, id: &str) -> Result<(), Error> {
        let id = trim_id("container", id)?;
        let response = self
            .call(&format!("/containers/{id}"), "force=1", |request| {
                *request.method_mut() = Method::DELETE;
            })
            .await?;
        drain(response).await;
        Ok(())
    }

    async fn create(&self, spec: &DaemonSpec) -> Result<Created, Error> {
        let body = Bytes::from(spec.body());
        let query = encode_query(&mut [("name", DAEMON_NAME.to_owned())]);
        let response = self
            .call("/containers/create", &query, |request| {
                *request.method_mut() = Method::POST;
                *request.body_mut() = Full::new(body.clone());
                request
                    .headers_mut()
                    .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
            })
            .await?;
        decode_first(&read_body(response).await?).map_err(Error::decode)
    }

    async fn attach(&self, id: &str) -> Result<Attached, Error> {
        let id = trim_id("container", id)?;
        let upgraded = self
            .client
            .exchange_upgrade(
                &format!("/containers/{id}/attach"),
                "stderr=1&stdout=1&stream=1",
                |request| {
                    empty_post(request);
                    let headers = request.headers_mut();
                    headers.insert(CONTENT_TYPE, HeaderValue::from_static("text/plain"));
                    headers.insert(CONNECTION, HeaderValue::from_static("Upgrade"));
                    headers.insert(UPGRADE, HeaderValue::from_static("tcp"));
                },
            )
            .await?;
        Ok(Box::new(Hijacked {
            inner: TokioIo::new(upgraded),
            socket: self.client.settings().socket().to_owned(),
        }))
    }

    async fn wait_removed(&self, id: &str) -> Result<WaitAnswer, Error> {
        let id = trim_id("container", id)?;
        let response = self
            .call(
                &format!("/containers/{id}/wait"),
                "condition=removed",
                empty_post,
            )
            .await?;
        Ok(Box::pin(wait_answer(response.into_body())))
    }

    async fn start(&self, id: &str) -> Result<(), Error> {
        let id = trim_id("container", id)?;
        self.post_empty(&format!("/containers/{id}/start"), "")
            .await
    }

    async fn stop(&self, id: &str, timeout_seconds: i64) -> Result<(), Error> {
        let id = trim_id("container", id)?;
        let query = encode_query(&mut [("t", timeout_seconds.to_string())]);
        self.post_empty(&format!("/containers/{id}/stop"), &query)
            .await
    }
}

/// Makes `request` the empty `POST` net/http sends: `Content-Length: 0`.
fn empty_post(request: &mut Request<Full<Bytes>>) {
    *request.method_mut() = Method::POST;
    request
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
}

async fn read_body(response: Response<Body>) -> Result<Vec<u8>, Error> {
    Ok(response.into_body().collect().await?.to_bytes().to_vec())
}

/// moby's `ensureReaderClosed`.
async fn drain(response: Response<Body>) {
    let _ = Limited::new(response.into_body(), DRAIN_LIMIT)
        .collect()
        .await;
}

/// moby's wait goroutine: the first JSON value of the answer, or the error its channel carries.
/// A syntax error is reported as the text of the answer (up to [`WAIT_ERROR_LIMIT`] more of it),
/// since a proxy that cuts the stream can only write plain text after the status; any other
/// failure as it reads.
async fn wait_answer(mut body: Body) -> Result<WaitResponse, String> {
    let mut received = Vec::new();
    loop {
        let read = body.frame().await;
        let ended = match read {
            None => true,
            Some(Ok(frame)) => {
                if let Ok(data) = frame.into_data() {
                    received.extend_from_slice(&data);
                }
                false
            }
            Some(Err(error)) => {
                return Err(match decode_wait(&received) {
                    // What was read so far is incomplete: the read error is what stopped it.
                    Decoded::Value(_) | Decoded::Incomplete => error.to_string(),
                    Decoded::Syntax => String::from_utf8_lossy(&received).into_owned(),
                    Decoded::Other(text) => text,
                });
            }
        };
        match decode_wait(&received) {
            Decoded::Value(response) => return Ok(response),
            Decoded::Incomplete if !ended => {}
            Decoded::Incomplete if received.iter().all(u8::is_ascii_whitespace) => {
                return Err("EOF".to_owned());
            }
            Decoded::Incomplete => return Err("unexpected EOF".to_owned()),
            Decoded::Syntax => {
                return Err(syntax_error_text(&mut body, received, ended).await);
            }
            Decoded::Other(text) => return Err(text),
        }
    }
}

/// The answer's text for a syntax error: what was read, and up to [`WAIT_ERROR_LIMIT`] more.
async fn syntax_error_text(body: &mut Body, mut received: Vec<u8>, ended: bool) -> String {
    if !ended {
        let limit = received.len() + WAIT_ERROR_LIMIT;
        while received.len() < limit {
            match body.frame().await {
                Some(Ok(frame)) => {
                    if let Ok(data) = frame.into_data() {
                        received.extend_from_slice(&data);
                    }
                }
                None | Some(Err(_)) => break,
            }
        }
        received.truncate(limit);
    }
    String::from_utf8_lossy(&received).into_owned()
}

enum Decoded {
    Value(WaitResponse),
    Incomplete,
    Syntax,
    Other(String),
}

fn decode_wait(received: &[u8]) -> Decoded {
    match serde_json::Deserializer::from_slice(received)
        .into_iter::<Option<WaitResponse>>()
        .next()
    {
        None => Decoded::Incomplete,
        Some(Ok(value)) => Decoded::Value(value.unwrap_or_default()),
        Some(Err(error)) => match error.classify() {
            serde_json::error::Category::Eof => Decoded::Incomplete,
            serde_json::error::Category::Syntax => Decoded::Syntax,
            serde_json::error::Category::Io | serde_json::error::Category::Data => {
                Decoded::Other(error.to_string())
            }
        },
    }
}

/// The hijacked attach connection, whose read errors read as Go's `net.OpError` on the unbound
/// unix socket the client dialled: `read unix @-><socket>: read: <errno>`.
struct Hijacked {
    inner: TokioIo<hyper::upgrade::Upgraded>,
    socket: String,
}

impl AsyncRead for Hijacked {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        let this = &mut *self;
        Pin::new(&mut this.inner)
            .poll_read(context, buffer)
            .map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "read unix @->{}: read: {}",
                        this.socket,
                        io_error_text(&error)
                    ),
                )
            })
    }
}

#[cfg(test)]
mod tests;
