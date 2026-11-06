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
//! writes in the project, on bollard.
//!
//! [`Engine`] shares the daemon client's connection (its socket, negotiation and error wording):
//! [`Client::transport`] hands out bollard's client, which only this module takes; only
//! `src/launcher/**` may name this module (`meta/tests/launcher_boundary.rs`). The calls:
//!
//! - `GET /containers/{id}/json`, and `GET /containers/json?all=1&filters=...` with the name
//!   filter `^/swarm-device-access$`;
//! - `DELETE /containers/{id}?force=1`;
//! - `POST /containers/create?name=...` with the daemon spec's JSON, decoded into bollard's
//!   model and sent as bollard encodes it;
//! - `POST /containers/{id}/attach?stderr=1&stdout=1&stream=1`, its output demultiplexed by
//!   docker-client into stdout and stderr pieces;
//! - `POST /containers/{id}/wait?condition=removed`, whose answer is read in the background;
//! - `POST /containers/{id}/start` and `POST /containers/{id}/stop?t=<seconds>`.
//!
//! Each unary call is bounded as a whole by the client's call timeout (the stop by that and the
//! seconds it grants the container), and nothing here retries: a write that fails is the
//! caller's to report, never sent twice. Only what the launcher reads is converted: an
//! inspect's `Image` and `State.Running`, a summary's `Id` and `Labels`, the create's `Id` and
//! `Warnings`, the wait's status code.

use std::future::Future;
use std::pin::Pin;
use std::time::Duration;

use tokio::sync::mpsc;

use super::spec::{DAEMON_NAME, DaemonSpec};
use crate::docker::attach::{self, Output, WaitCondition};
use crate::docker::bollard::models::ContainerCreateBody;
use crate::docker::bollard::query_parameters::{
    CreateContainerOptionsBuilder, InspectContainerOptions, ListContainersOptionsBuilder,
    RemoveContainerOptionsBuilder, StartContainerOptions, StopContainerOptionsBuilder,
};
use crate::docker::types::Labels;
use crate::docker::{
    CancellationToken, Client, ClientError, Error, ErrorKind, Filters, bounded, trim_id,
};

/// The daemon's output: its stdout and stderr pieces, until its output ends (the channel
/// closes) or the stream breaks (an error comes first).
pub type Attached = mpsc::Receiver<Result<Output, ClientError>>;

/// The answer of a wait, once the condition is met; the error is the engine's, worded.
pub type WaitAnswer = Pin<Box<dyn Future<Output = Result<WaitResponse, String>> + Send>>;

/// How many pieces of output wait for the copy to the console before the attach's reader does.
const OUTPUT_BUFFER: usize = 64;

/// What a wait that finds the container already removed answers: the exit code the engine no
/// longer has, read as a failure.
const REMOVED_STATUS: i64 = 1;

/// container `InspectResponse`: what the launcher reads of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Inspected {
    /// `Image`: the ID of the image the container runs.
    pub image: String,
    /// `State`, absent when the Engine reported none.
    pub state: Option<State>,
}

/// container `State`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct State {
    /// `Running`.
    pub running: bool,
}

/// container `Summary`: what the launcher reads of a listed container.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Summary {
    /// `Id`.
    pub id: String,
    /// `Labels`.
    pub labels: Labels,
}

/// container `CreateResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Created {
    /// `Id`.
    pub id: String,
    /// `Warnings`.
    pub warnings: Vec<String>,
}

/// container `WaitResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitResponse {
    /// `StatusCode`.
    pub status_code: i64,
    /// `Error`, absent unless the wait itself failed.
    pub error: Option<WaitExitError>,
}

/// container `WaitExitError`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WaitExitError {
    /// `Message`.
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

    /// moby's `ContainerAttach` with `Stream`, `Stdout` and `Stderr`: the output, demultiplexed.
    fn attach(&self, id: &str) -> impl Future<Output = Result<Attached, Error>> + Send;

    /// moby's `ContainerWait` for the `removed` condition: resolves once the wait is on its way,
    /// with its answer to come. A container the engine no longer has is answered as removed, its
    /// exit status unknown.
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
    /// The launcher's engine on `client`'s connection.
    #[must_use]
    pub fn new(client: Client) -> Self {
        Self { client }
    }

    fn call(&self) -> Duration {
        self.client.call_timeout()
    }
}

impl EngineApi for Engine {
    async fn inspect(&self, id: &str) -> Result<Inspected, Error> {
        let id = trim_id(id)?;
        let docker = self.client.transport().await;
        let inspected = bounded(
            self.call(),
            docker.inspect_container(id, None::<InspectContainerOptions>),
        )
        .await?;
        Ok(Inspected {
            image: inspected.image.unwrap_or_default(),
            state: inspected.state.map(|state| State {
                running: state.running.unwrap_or_default(),
            }),
        })
    }

    async fn list_daemons(&self) -> Result<Vec<Summary>, Error> {
        let filters = Filters::new().add("name", &[&format!("^/{DAEMON_NAME}$")]);
        let options = ListContainersOptionsBuilder::new()
            .all(true)
            .filters(&filters.to_map())
            .build();
        let docker = self.client.transport().await;
        let listed = bounded(self.call(), docker.list_containers(Some(options))).await?;
        Ok(listed
            .into_iter()
            .map(|summary| Summary {
                id: summary.id.unwrap_or_default(),
                labels: summary.labels.unwrap_or_default().into_iter().collect(),
            })
            .collect())
    }

    async fn remove(&self, id: &str) -> Result<(), Error> {
        let id = trim_id(id)?;
        let options = RemoveContainerOptionsBuilder::new().force(true).build();
        let docker = self.client.transport().await;
        bounded(self.call(), docker.remove_container(id, Some(options))).await
    }

    async fn create(&self, spec: &DaemonSpec) -> Result<Created, Error> {
        let body: ContainerCreateBody = serde_json::from_str(&spec.body())
            .map_err(|error| Error::invalid(format!("encode the daemon spec: {error}")))?;
        let options = CreateContainerOptionsBuilder::new()
            .name(DAEMON_NAME)
            .build();
        let docker = self.client.transport().await;
        let created = bounded(self.call(), docker.create_container(Some(options), body)).await?;
        Ok(Created {
            id: created.id,
            warnings: created.warnings,
        })
    }

    async fn attach(&self, id: &str) -> Result<Attached, Error> {
        let id = trim_id(id)?;
        let docker = self.client.transport().await;
        // The pump ends with the receiver the launcher drops, and the token is never cancelled.
        Ok(attach::attach(
            &docker,
            id,
            self.call(),
            OUTPUT_BUFFER,
            CancellationToken::new(),
        )
        .await?)
    }

    async fn wait_removed(&self, id: &str) -> Result<WaitAnswer, Error> {
        let id = trim_id(id)?;
        let docker = self.client.transport().await;
        let answer = attach::wait(
            &docker,
            id,
            WaitCondition::Removed,
            CancellationToken::new(),
        );
        Ok(Box::pin(async move {
            match answer.await {
                Ok(Ok(status_code)) => Ok(WaitResponse {
                    status_code,
                    error: None,
                }),
                // An auto-removed daemon the wait came too late for: removed, its status gone.
                Ok(Err(error)) if error.kind() == ErrorKind::NotFound => Ok(WaitResponse {
                    status_code: REMOVED_STATUS,
                    error: Some(WaitExitError {
                        message: format!("container already removed, exit status unknown: {error}"),
                    }),
                }),
                Ok(Err(error)) => Err(error.to_string()),
                Err(_closed) => Err("the wait ended without an answer".to_owned()),
            }
        }))
    }

    async fn start(&self, id: &str) -> Result<(), Error> {
        let id = trim_id(id)?;
        let docker = self.client.transport().await;
        bounded(
            self.call(),
            docker.start_container(id, None::<StartContainerOptions>),
        )
        .await
    }

    async fn stop(&self, id: &str, timeout_seconds: i64) -> Result<(), Error> {
        let id = trim_id(id)?;
        let seconds = i32::try_from(timeout_seconds).unwrap_or(i32::MAX);
        let options = StopContainerOptionsBuilder::new().t(seconds).build();
        let docker = self.client.transport().await;
        // The engine answers once the container stopped, up to the seconds it was given.
        let bound =
            self.call() + Duration::from_secs(u64::try_from(timeout_seconds).unwrap_or_default());
        bounded(bound, docker.stop_container(id, Some(options))).await
    }
}

#[cfg(test)]
mod tests;
