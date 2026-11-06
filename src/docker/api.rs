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

// Adapted from swarm-scheduler-exporter/src/docker/api.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The read-only Docker API the daemon uses: the four calls of [`DockerApi`] and nothing else,
//! on bollard through docker-client.
//!
//! The requests: `GET /_ping`, `GET /containers/json` without options (running containers only),
//! `GET /containers/{id}/json`, and `GET /events` with `filters` and `since` (`<seconds>.<9
//! digits>`).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, PoisonError};
use std::time::Duration;

use docker_client::bollard::Docker;
use docker_client::bollard::query_parameters::{EventsOptionsBuilder, InspectContainerOptions};
use docker_client::{
    CancellationToken, Config, ConnectError, ErrorKind, connect, negotiate, pump_stream,
};
use jiff::Timestamp;
use tokio::sync::mpsc;

use super::types::{ContainerInspect, ContainerSummary, Event};
use super::{Error, StreamError};
use crate::version::BUILD;

/// The event records buffered between bollard's stream and the loop that handles them.
const EVENT_BUFFER: usize = 16;

/// The daemon's Docker client: docker-client's connection to the unix socket. It shows none of
/// bollard's calls, so `ping`, `events` and the rest are the trait's, and the launcher's writes go
/// through `Client::transport` alone.
pub struct Client {
    connection: Mutex<Docker>,
    config: Config,
    /// Whether the API version was settled: bollard starts at its newest and [`negotiate`] lowers
    /// it to the engine's, on the first call that reaches one.
    negotiated: AtomicBool,
    call: Duration,
}

impl fmt::Debug for Client {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Client")
            .field("negotiated", &self.negotiated)
            .field("call", &self.call)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// A client for the unix socket at `socket`, with no connection open yet. `call` is the whole
    /// time one call may take, the body included (nothing a caller has not bounded runs longer),
    /// and `headers` bollard's own wait for the response headers, which a stop of a container that
    /// takes its time to stop must be allowed to use.
    ///
    /// # Errors
    ///
    /// [`ConnectError`] when bollard refuses the socket.
    pub fn unix(socket: &str, call: Duration, headers: Duration) -> Result<Self, ConnectError> {
        let config = Config::unix(socket)
            .user_agent(format!("swarm-device-access/{}", BUILD.version))
            .response_header_timeout(headers);
        Ok(Self {
            connection: Mutex::new(connect(config.clone())?),
            config,
            negotiated: AtomicBool::new(false),
            call,
        })
    }

    /// The whole-call bound of this client's unary calls.
    pub(crate) fn call_timeout(&self) -> Duration {
        self.call
    }

    fn snapshot(&self) -> Docker {
        self.connection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// bollard's client at the API version the engine speaks: negotiated on the first call that
    /// reaches an engine, as moby does lazily, and again while the engine could not be reached. An
    /// engine that answers but does not negotiate (no `/version`) is spoken to at bollard's
    /// version, as moby does after a failed negotiation. Only `src/launcher/engine.rs` takes it
    /// for the launcher's write calls (`meta/tests/launcher_boundary.rs`).
    pub(crate) async fn transport(&self) -> Docker {
        if !self.negotiated.load(Ordering::Acquire) {
            let attempt = tokio::time::timeout(
                self.call,
                Box::pin(negotiate(self.snapshot(), &self.config)),
            )
            .await;
            match attempt {
                Ok(Ok(settled)) => {
                    *self
                        .connection
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner) = settled;
                    self.negotiated.store(true, Ordering::Release);
                }
                Ok(Err(error))
                    if !matches!(error.kind(), ErrorKind::Unavailable | ErrorKind::Timeout) =>
                {
                    self.negotiated.store(true, Ordering::Release);
                }
                Ok(Err(_)) | Err(_) => {}
            }
        }
        self.snapshot()
    }
}

/// moby's `Filters`: terms with sets of values.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    terms: BTreeMap<String, BTreeSet<String>>,
}

impl Filters {
    /// No filters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// moby's `Filters.Add`: `values` join those of `term`.
    #[must_use]
    pub fn add(mut self, term: &str, values: &[&str]) -> Self {
        self.terms
            .entry(term.to_owned())
            .or_default()
            .extend(values.iter().map(|value| (*value).to_owned()));
        self
    }

    /// Whether no term was added.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The terms as bollard's options take them.
    pub(crate) fn to_map(&self) -> HashMap<String, Vec<String>> {
        self.terms
            .iter()
            .map(|(term, values)| (term.clone(), values.iter().cloned().collect()))
            .collect()
    }
}

/// moby's `since` parameter: `<seconds>.<9 digits>`.
fn since_parameter(since: Timestamp) -> String {
    format!("{}.{:09}", since.as_second(), since.subsec_nanosecond())
}

/// The Docker calls the daemon makes. Every one but [`DockerApi::events`] is a single request
/// and response, which the caller bounds with [`super::with_context`]; the event stream is
/// long-lived and has no deadline.
pub trait DockerApi: Send + Sync {
    /// The event stream [`DockerApi::events`] opens.
    type Events: EventStream;

    /// `GET /_ping`.
    fn ping(&self) -> impl Future<Output = Result<(), Error>> + Send;

    /// moby's `ContainerList` with no options: `GET /containers/json`, the running containers.
    fn container_list(&self) -> impl Future<Output = Result<Vec<ContainerSummary>, Error>> + Send;

    /// moby's `ContainerInspect` without size: `GET /containers/{id}/json`.
    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, Error>> + Send;

    /// moby's `Events` from `since` with `filters`: resolves with the stream, which bollard opens
    /// lazily, so a refused connect is its first item.
    fn events(
        &self,
        since: Timestamp,
        filters: &Filters,
    ) -> impl Future<Output = Result<Self::Events, Error>> + Send;
}

/// A shared client is a client: the daemon hands one to the coordinator and the event loop.
impl<Api: DockerApi + ?Sized> DockerApi for std::sync::Arc<Api> {
    type Events = Api::Events;

    fn ping(&self) -> impl Future<Output = Result<(), Error>> + Send {
        Api::ping(self)
    }

    fn container_list(&self) -> impl Future<Output = Result<Vec<ContainerSummary>, Error>> + Send {
        Api::container_list(self)
    }

    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, Error>> + Send {
        Api::container_inspect(self, id)
    }

    fn events(
        &self,
        since: Timestamp,
        filters: &Filters,
    ) -> impl Future<Output = Result<Self::Events, Error>> + Send {
        Api::events(self, since, filters)
    }
}

/// An open event stream.
pub trait EventStream: Send {
    /// The next event, or how the stream ended; after an error the stream is over.
    fn next(&mut self) -> impl Future<Output = Result<Event, StreamError>> + Send;
}

/// The event stream of the Docker client: bollard's stream pumped into a channel by a task that
/// ends with the stream, which closes its connection.
#[derive(Debug)]
pub struct ClientEventStream {
    events:
        mpsc::Receiver<Result<docker_client::bollard::models::EventMessage, docker_client::Error>>,
    over: bool,
    cancel: CancellationToken,
}

impl EventStream for ClientEventStream {
    async fn next(&mut self) -> Result<Event, StreamError> {
        if self.over {
            return Err(StreamError::eof());
        }
        match self.events.recv().await {
            Some(Ok(event)) => Ok(event.into()),
            Some(Err(error)) => {
                self.over = true;
                Err(StreamError::failed(&error))
            }
            None => {
                self.over = true;
                Err(StreamError::eof())
            }
        }
    }
}

impl Drop for ClientEventStream {
    fn drop(&mut self) {
        // The pump ends with its receiver; the token ends it at once, also while it is idle.
        self.cancel.cancel();
    }
}

impl DockerApi for Client {
    type Events = ClientEventStream;

    async fn ping(&self) -> Result<(), Error> {
        let docker = self.transport().await;
        bounded(self.call, docker.ping()).await?;
        Ok(())
    }

    async fn container_list(&self) -> Result<Vec<ContainerSummary>, Error> {
        let docker = self.transport().await;
        let listed = bounded(self.call, docker.list_containers(None)).await?;
        Ok(listed.into_iter().map(Into::into).collect())
    }

    async fn container_inspect(&self, id: &str) -> Result<ContainerInspect, Error> {
        let id = trim_id(id)?;
        let docker = self.transport().await;
        let inspected = bounded(
            self.call,
            docker.inspect_container(id, None::<InspectContainerOptions>),
        )
        .await?;
        Ok(inspected.into())
    }

    async fn events(
        &self,
        since: Timestamp,
        filters: &Filters,
    ) -> Result<ClientEventStream, Error> {
        let docker = self.transport().await;
        let mut options = EventsOptionsBuilder::new().since(&since_parameter(since));
        if !filters.is_empty() {
            options = options.filters(&filters.to_map());
        }
        let options = options.build();
        let cancel = CancellationToken::new();
        let events = pump_stream(docker.events(Some(options)), EVENT_BUFFER, cancel.clone());
        Ok(ClientEventStream {
            events,
            over: false,
            cancel,
        })
    }
}

/// Runs a bollard `call` for at most `timeout`, from the request to the end of the response body
/// (bollard's own timeout stops at the headers), its failure sorted into docker-client's kinds.
/// The call is boxed: bollard's futures are large, and the launcher's supervision holds several.
///
/// # Errors
///
/// [`ErrorKind::Timeout`] when `timeout` passes first, else the call's error.
pub(crate) fn bounded<'call, T: 'call>(
    timeout: Duration,
    call: impl Future<Output = Result<T, docker_client::bollard::errors::Error>> + Send + 'call,
) -> Pin<Box<dyn Future<Output = Result<T, Error>> + Send + 'call>> {
    Box::pin(async move { Ok(docker_client::bounded(timeout, call).await?) })
}

/// moby's refusal of an empty ID: `invalid container name or ID: value is empty`.
pub(crate) fn trim_id(id: &str) -> Result<&str, Error> {
    let id = id.trim();
    if id.is_empty() {
        return Err(Error::invalid(
            "invalid container name or ID: value is empty",
        ));
    }
    Ok(id)
}
