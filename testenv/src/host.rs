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
// Adapted from swarm-scheduler-exporter/testenv/src/dind.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! What the harness does on the host Docker daemon itself, rather than in a test: the client, the
//! sweep and cleanup configuration, and the run of an image with the identity flag. Cut down from
//! swarm-scheduler-exporter's `DinD` bring-up: there is no cluster, no network and no image
//! transfer here, since the suite drives the host daemon the daemon under test watches. The sweep,
//! the cleanup, the removals and the bounded calls are testenv-core's `host`, configured here
//! ([`HOST_SWEEP`]).

use std::collections::HashMap;
use std::hash::BuildHasher;
use std::time::Duration;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptions, InspectContainerOptions, LogsOptions, StartContainerOptions,
    WaitContainerOptions,
};
use clock::{Clock as _, SystemClock};
use futures_util::StreamExt as _;
use testenv_core::Error;
use testenv_core::context::Context;
use testenv_core::host::{HostSweep, sweep_rules};
pub use testenv_core::host::{OP_TIMEOUT, OpError, with_timeout};
use testenv_core::identity::IDENTITY_FLAG;
use testenv_core::sweep_rules::{DEFAULT_STALE_AFTER, Owner, SweepRules, stale_after_from_env};

use crate::config::{ENV_STALE_AFTER, LABELS, RESOURCES};
use crate::identity::{check_go_identity, check_identity};

/// The suite's sweep and cleanup: its labels, containers and images, each call bounded by
/// [`OP_TIMEOUT`].
pub const HOST_SWEEP: HostSweep = HostSweep {
    labels: LABELS,
    resources: RESOURCES,
    op_timeout: OP_TIMEOUT,
};
/// Bounds the whole run of an image with the identity flag: create, start, wait, logs, remove.
const IDENTITY_RUN_TIMEOUT: Duration = Duration::from_secs(60);
/// The client's own timeout, above every deadline of the harness, so those are the ones that
/// apply.
const CLIENT_TIMEOUT_SECS: u64 = 300;

/// The host daemon's client: `DOCKER_HOST` when it names a unix socket, else the local
/// `/var/run/docker.sock`, the socket the daemon under test watches by default. Neither TCP nor
/// TLS: the harness runs beside the daemon under test, on the same host.
///
/// There is no version negotiation, and none is needed: bollard builds each URL as
/// `/v<version><path>` and then joins the absolute path onto it, which drops the version prefix
/// (bollard 0.21.1, `uri.rs`), so every request goes out unversioned and the daemon serves it at
/// its own API version.
///
/// # Errors
///
/// The socket cannot be used.
pub fn host_client() -> Result<Docker, Error> {
    Docker::connect_with_unix_defaults()
        .map(|client| client.with_timeout(Duration::from_secs(CLIENT_TIMEOUT_SECS)))
        .map_err(|error| Error::new(format!("connect to host Docker daemon: {error}")))
}

/// The sweep rules for this process: now, this host, the window [`ENV_STALE_AFTER`] sets (an hour
/// by default) and a PID check on this host.
///
/// # Errors
///
/// [`ENV_STALE_AFTER`] is set but is not a positive duration.
pub fn default_sweep_rules(self_id: &str, owner: &Owner) -> Result<SweepRules, Error> {
    let stale_after = stale_after_from_env(ENV_STALE_AFTER, DEFAULT_STALE_AFTER, |key| {
        std::env::var(key).ok()
    })?;
    Ok(sweep_rules(SystemClock.now(), stale_after, self_id, owner))
}

/// Runs `image` with [`IDENTITY_FLAG`] and checks the answer is the Rust daemon's
/// ([`check_identity`]). The container carries `labels` and is removed afterwards, whatever the
/// outcome.
///
/// # Errors
///
/// The image could not be run, or its answer is not the Rust daemon's; either way the message
/// names the image.
pub async fn check_image_identity<S: BuildHasher>(
    ctx: &Context,
    client: &Docker,
    image: &str,
    labels: &HashMap<String, String, S>,
) -> Result<(), Error> {
    check_image_answer(ctx, client, image, labels, check_identity).await
}

/// [`check_image_identity`] inverted: runs `image` with [`IDENTITY_FLAG`] and checks the answer is
/// the Go daemon's ([`check_go_identity`]).
///
/// # Errors
///
/// The image could not be run, or its answer is not the Go daemon's; either way the message names
/// the image.
pub async fn check_go_image_identity<S: BuildHasher>(
    ctx: &Context,
    client: &Docker,
    image: &str,
    labels: &HashMap<String, String, S>,
) -> Result<(), Error> {
    check_image_answer(ctx, client, image, labels, check_go_identity).await
}

/// Runs `image` with [`IDENTITY_FLAG`] and applies `check` to its answer.
async fn check_image_answer<S: BuildHasher>(
    ctx: &Context,
    client: &Docker,
    image: &str,
    labels: &HashMap<String, String, S>,
    check: fn(&str, i64, &str, &str) -> Result<(), Error>,
) -> Result<(), Error> {
    let labels = labels
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let (exit_code, stdout, stderr) = Box::pin(answer_to_identity_flag(ctx, client, image, labels))
        .await
        .map_err(|error| error.context(format!("run image {image} with {IDENTITY_FLAG}")))?;
    check(&format!("image {image}"), exit_code, &stdout, &stderr)
}

/// `docker run --rm <image> -help`: the exit code, stdout and stderr.
async fn answer_to_identity_flag(
    ctx: &Context,
    client: &Docker,
    image: &str,
    labels: HashMap<String, String>,
) -> Result<(i64, String, String), Error> {
    let ctx = ctx.with_timeout(IDENTITY_RUN_TIMEOUT);
    let created = with_timeout(
        &ctx,
        OP_TIMEOUT,
        client.create_container(
            None::<CreateContainerOptions>,
            ContainerCreateBody {
                image: Some(image.to_owned()),
                cmd: Some(vec![IDENTITY_FLAG.to_owned()]),
                labels: Some(labels),
                ..ContainerCreateBody::default()
            },
        ),
    )
    .await
    .map_err(|error| Error::new(format!("create container: {error}")))?;
    let answer = run_to_exit(&ctx, client, &created.id).await;
    // The removal runs on a context of its own, so a run cut short still cleans up.
    let removed = HOST_SWEEP
        .remove_container(
            &ctx.without_cancel().with_timeout(OP_TIMEOUT),
            client,
            &created.id,
        )
        .await;
    let answer = answer?;
    removed?;
    Ok(answer)
}

/// Starts the container `id`, waits for it to exit and reads its exit code and its output.
async fn run_to_exit(
    ctx: &Context,
    client: &Docker,
    id: &str,
) -> Result<(i64, String, String), Error> {
    with_timeout(
        ctx,
        OP_TIMEOUT,
        client.start_container(id, None::<StartContainerOptions>),
    )
    .await
    .map_err(|error| Error::new(format!("start container: {error}")))?;
    let mut waits = client.wait_container(
        id,
        Some(WaitContainerOptions {
            condition: "not-running".to_owned(),
        }),
    );
    match ctx.run(waits.next()).await {
        // bollard reports a non-zero exit as an error; either way, the container has exited.
        Ok(Some(Ok(_) | Err(DockerError::DockerContainerWaitError { .. }))) => {}
        Ok(Some(Err(error))) => return Err(Error::new(format!("wait for container: {error}"))),
        Ok(None) => return Err(Error::new("wait for container: no answer")),
        Err(ended) => return Err(Error::new(format!("wait for container: {ended}"))),
    }
    let inspected = with_timeout(
        ctx,
        OP_TIMEOUT,
        client.inspect_container(id, None::<InspectContainerOptions>),
    )
    .await
    .map_err(|error| Error::new(format!("inspect container: {error}")))?;
    let exit_code = inspected
        .state
        .and_then(|state| state.exit_code)
        .ok_or_else(|| Error::new("inspect container: no exit code"))?;
    let (stdout, stderr) = read_logs(ctx, client, id).await?;
    Ok((exit_code, stdout, stderr))
}

/// The whole output of the exited container `id`, stdout and stderr apart.
async fn read_logs(ctx: &Context, client: &Docker, id: &str) -> Result<(String, String), Error> {
    let mut stream = client.logs(
        id,
        Some(LogsOptions {
            stdout: true,
            stderr: true,
            ..LogsOptions::default()
        }),
    );
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    loop {
        let next = ctx
            .run(stream.next())
            .await
            .map_err(|ended| Error::new(format!("read container logs: {ended}")))?;
        match next {
            None => break,
            Some(Err(error)) => return Err(Error::new(format!("read container logs: {error}"))),
            Some(Ok(LogOutput::StdOut { message } | LogOutput::Console { message })) => {
                stdout.extend_from_slice(&message);
            }
            Some(Ok(LogOutput::StdErr { message })) => stderr.extend_from_slice(&message),
            Some(Ok(LogOutput::StdIn { .. })) => {}
        }
    }
    Ok((
        String::from_utf8_lossy(&stdout).into_owned(),
        String::from_utf8_lossy(&stderr).into_owned(),
    ))
}
