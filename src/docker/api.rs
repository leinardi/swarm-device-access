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
//! with moby client v0.6.0's requests for the options the Go daemon passes.
//!
//! The requests: `HEAD /_ping` (then `GET`) for the ping, `GET /containers/json` without
//! options (running containers only), `GET /containers/{id}/json`, and `GET /events` with
//! `filters` (moby's JSON, keys sorted) and `since` (`<seconds>.<9 digits>`), the query keys
//! sorted and escaped as Go's `url.Values.Encode` does.

use std::future::Future;

use jiff::Timestamp;

use super::types::{ContainerInspect, ContainerSummary, Event};
use docker_engine::{
    Body, Error, Filters, JsonStream, Settings, StreamError, encode_query, since_parameter, trim_id,
};

/// The daemon's Docker client: docker-engine's, which only [`DockerApi`] and the launcher's
/// engine reach. It shows none of the crate's own calls, so `ping`, `events` and the rest are the
/// trait's, and the writes go through `Client::transport` alone.
#[derive(Debug)]
pub struct Client {
    inner: docker_engine::Client,
}

impl Client {
    /// A client for `settings`, with no connection open yet.
    #[must_use]
    pub fn new(settings: Settings) -> Self {
        Self {
            inner: docker_engine::Client::new(settings),
        }
    }

    /// The settings the client was built from.
    #[must_use]
    pub fn settings(&self) -> &Settings {
        self.inner.settings()
    }

    /// The API version requests use now: the negotiated one, else the highest the client
    /// speaks.
    pub async fn client_version(&self) -> String {
        self.inner.client_version().await
    }

    /// docker-engine's client, whose `exchange` and `exchange_upgrade` are the launcher's write
    /// calls: only `src/launcher/engine.rs` takes it (`meta/tests/launcher_boundary.rs`).
    pub(crate) fn transport(&self) -> &docker_engine::Client {
        &self.inner
    }
}

/// The Docker calls the daemon makes. Every one but [`DockerApi::events`] is a single request
/// and response, which the caller bounds with [`super::with_context`] or
/// [`super::with_context_timeout`]; the event stream is long-lived and has no deadline.
pub trait DockerApi: Send + Sync {
    /// The event stream [`DockerApi::events`] opens.
    type Events: EventStream;

    /// moby's `Ping` without `NegotiateAPIVersion`: `HEAD /_ping`, then `GET /_ping` when that
    /// fails or is not a 200.
    fn ping(&self) -> impl Future<Output = Result<(), Error>> + Send;

    /// moby's `ContainerList` with no options: `GET /containers/json`, the running containers.
    fn container_list(&self) -> impl Future<Output = Result<Vec<ContainerSummary>, Error>> + Send;

    /// moby's `ContainerInspect` without size: `GET /containers/{id}/json`.
    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, Error>> + Send;

    /// moby's `Events` from `since` with `filters`: resolves once the response headers passed
    /// moby's status check, which is when the stream counts as established, and fails without a
    /// stream otherwise.
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

/// The event stream of the Docker client.
#[derive(Debug)]
pub struct ClientEventStream {
    stream: JsonStream<Body>,
}

impl ClientEventStream {
    /// Whether the response was a JSON text sequence, read through the RS filter.
    #[must_use]
    pub fn filters_rs(&self) -> bool {
        self.stream.filters_rs()
    }
}

impl EventStream for ClientEventStream {
    async fn next(&mut self) -> Result<Event, StreamError> {
        self.stream.next().await
    }
}

impl DockerApi for Client {
    type Events = ClientEventStream;

    async fn ping(&self) -> Result<(), Error> {
        self.inner.ping().await.map(|_| ())
    }

    async fn container_list(&self) -> Result<Vec<ContainerSummary>, Error> {
        self.inner.get_list("/containers/json", "").await
    }

    async fn container_inspect(&self, id: &str) -> Result<ContainerInspect, Error> {
        let id = trim_id("container", id)?;
        self.inner
            .get_object(&format!("/containers/{id}/json"))
            .await
    }

    async fn events(
        &self,
        since: Timestamp,
        filters: &Filters,
    ) -> Result<ClientEventStream, Error> {
        let mut pairs = vec![("since", since_parameter(since))];
        if !filters.is_empty() {
            pairs.push(("filters", filters.to_json()));
        }
        Ok(ClientEventStream {
            stream: self.inner.events(&encode_query(&mut pairs)).await?,
        })
    }
}
