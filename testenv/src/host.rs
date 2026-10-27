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

//! Everything the harness does on the host Docker daemon itself, rather than in a test: the
//! client, the run's labels, the sweep of abandoned runs and the cleanup of one run, and the run
//! of an image with the identity flag. Cut down from swarm-scheduler-exporter's `DinD` bring-up to
//! its sweep, cleanup and call helpers: there is no cluster, no network and no image transfer
//! here, since the suite drives the host daemon the daemon under test watches.

use std::collections::{BTreeMap, HashMap};
use std::fmt;
use std::future::Future;
use std::hash::BuildHasher;
use std::time::Duration;

use bollard::Docker;
use bollard::container::LogOutput;
use bollard::errors::Error as DockerError;
use bollard::models::ContainerCreateBody;
use bollard::query_parameters::{
    CreateContainerOptions, InspectContainerOptions, ListContainersOptions, ListImagesOptions,
    LogsOptions, RemoveContainerOptions, RemoveImageOptions, StartContainerOptions,
    WaitContainerOptions,
};
use futures_util::StreamExt as _;
use jiff::Timestamp;
use swarm_device_access::clock::{Clock as _, SystemClock};
use swarm_device_access::logger::Attr;

use crate::Error;
use crate::context::{Context, Ended};
use crate::envid::{LABEL_ENV_ID, label_filter};
use crate::identity::{IDENTITY_FLAG, check_identity};
use crate::redact::RedactingLogger;
use crate::sweep_rules::{
    Found, LABEL_OWNER, LABEL_PID_NAMESPACE, LABEL_STARTED, Owner, SweepRules, stale_after_from_env,
};

/// Bounds one ordinary Docker call, so a hung daemon fails that call instead of stalling the run
/// until the suite deadline.
pub const OP_TIMEOUT: Duration = Duration::from_secs(30);
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

/// The labels every container and image of the run carries: the envid, which the cleanup and
/// `make sweep-test-leaks` select by, and the owner, start time and PID namespace the sweep of a
/// later run decides by.
#[must_use]
pub fn run_labels(env_id: &str, owner: &Owner, started: Timestamp) -> HashMap<String, String> {
    HashMap::from([
        (LABEL_ENV_ID.to_owned(), env_id.to_owned()),
        (LABEL_OWNER.to_owned(), owner.to_string()),
        (
            LABEL_STARTED.to_owned(),
            started.strftime("%Y-%m-%dT%H:%M:%SZ").to_string(),
        ),
        (LABEL_PID_NAMESPACE.to_owned(), owner.pid_namespace.clone()),
    ])
}

/// A shell command that removes every container and image of the run `env_id` from the host.
#[must_use]
pub fn cleanup_command(env_id: &str) -> String {
    let filter = format!("label={}", label_filter(env_id));
    format!(
        "docker ps -aq --filter {filter} | xargs -r docker rm -f; docker image ls -q --filter \
         {filter} | xargs -r docker rmi -f"
    )
}

/// The sweep rules for this process: now, this host, the configured window and a PID check on
/// this host.
///
/// # Errors
///
/// `SDA_IT_STALE_AFTER` is set but is not a positive duration.
pub fn default_sweep_rules(self_id: &str, owner: &Owner) -> Result<SweepRules, Error> {
    Ok(SweepRules {
        now: SystemClock.now(),
        stale_after: stale_after_from_env(|key| std::env::var(key).ok())?,
        host: owner.host.clone(),
        pid_namespace: owner.pid_namespace.clone(),
        self_id: self_id.to_owned(),
        alive: Box::new(process_alive),
    })
}

/// Whether `pid` exists on this host: signal 0 checks without delivering, and a permission error
/// still means the process is there. A PID outside `pid_t` names no process: Linux keeps PIDs
/// under 2^22.
fn process_alive(pid: i64) -> bool {
    let Some(pid) = i32::try_from(pid)
        .ok()
        .and_then(rustix::process::Pid::from_raw)
    else {
        return false;
    };
    match rustix::process::test_kill_process(pid) {
        Ok(()) => true,
        Err(errno) => errno == rustix::io::Errno::PERM,
    }
}

/// Removes the runs that `rules` finds abandoned: leaks from runs that were killed before their
/// cleanups ran. A run that may still belong to a running suite, on this host or another, is left
/// alone, and the reason is logged.
pub async fn sweep(ctx: &Context, client: &Docker, rules: &SweepRules, log: &RedactingLogger) {
    for env in list_environments(ctx, client, log).await {
        let verdict = rules.decide(env);
        if !verdict.remove {
            if verdict.env.id != rules.self_id {
                log.debug(
                    "sweep: keeping environment",
                    &[
                        Attr::string("other_envid", verdict.env.id.clone()),
                        Attr::string("reason", verdict.reason.clone()),
                    ],
                );
            }
            continue;
        }

        log.info(
            "sweep: removing leaked environment",
            &[
                Attr::string("leaked_envid", verdict.env.id.clone()),
                Attr::string("reason", verdict.reason.clone()),
            ],
        );

        if let Err(error) = cleanup(ctx, client, &verdict.env.id).await {
            log.warn(
                "sweep: cleanup incomplete",
                &[
                    Attr::string("leaked_envid", verdict.env.id.clone()),
                    Attr::string("err", error.to_string()),
                ],
            );
        }
    }
}

/// Adds one resource's labels to the environment they name.
fn record(by_id: &mut BTreeMap<String, Found>, labels: Option<&HashMap<String, String>>) {
    let Some(labels) = labels else {
        return;
    };
    let Some(env_id) = labels.get(LABEL_ENV_ID).filter(|id| !id.is_empty()) else {
        return;
    };
    let found = by_id.entry(env_id.clone()).or_insert_with(|| Found {
        id: env_id.clone(),
        ..Found::default()
    });
    // A resource may carry the envid alone (the Go suite's containers do); the owner and start
    // time come from whichever resource has them.
    for (field, key) in [
        (&mut found.owner, LABEL_OWNER),
        (&mut found.started, LABEL_STARTED),
        (&mut found.pid_namespace, LABEL_PID_NAMESPACE),
    ] {
        if field.is_empty()
            && let Some(value) = labels.get(key)
        {
            value.clone_into(field);
        }
    }
}

/// The label filter selecting resources that carry `value` (a key, or `key=value`).
fn label_filters(value: String) -> HashMap<String, Vec<String>> {
    HashMap::from([("label".to_owned(), vec![value])])
}

/// Every environment with a container or image on the host daemon, from their labels. A listing
/// that fails is logged and leaves that kind of resource out: the sweep is best-effort, and each
/// test's cleanups still remove the run's own resources.
async fn list_environments(ctx: &Context, client: &Docker, log: &RedactingLogger) -> Vec<Found> {
    let mut by_id = BTreeMap::new();

    match with_timeout(
        ctx,
        OP_TIMEOUT,
        client.list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(label_filters(LABEL_ENV_ID.to_owned())),
            ..ListContainersOptions::default()
        })),
    )
    .await
    {
        Ok(containers) => {
            for container in &containers {
                record(&mut by_id, container.labels.as_ref());
            }
        }
        Err(error) => log.warn(
            "sweep: list containers",
            &[Attr::string("err", error.to_string())],
        ),
    }

    match with_timeout(
        ctx,
        OP_TIMEOUT,
        client.list_images(Some(ListImagesOptions {
            filters: Some(label_filters(LABEL_ENV_ID.to_owned())),
            ..ListImagesOptions::default()
        })),
    )
    .await
    {
        Ok(images) => {
            for image in &images {
                record(&mut by_id, Some(&image.labels));
            }
        }
        Err(error) => log.warn(
            "sweep: list images",
            &[Attr::string("err", error.to_string())],
        ),
    }

    by_id.into_values().collect()
}

/// Removes every container, then every image, labeled with `env_id`. It keeps going past
/// individual failures and returns them joined.
///
/// # Errors
///
/// Every listing or removal that failed, one per line; a resource already gone is not a failure.
pub async fn cleanup(ctx: &Context, client: &Docker, env_id: &str) -> Result<(), Error> {
    let mut errors = Vec::new();

    let containers = with_timeout(
        ctx,
        OP_TIMEOUT,
        client.list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(label_filters(label_filter(env_id))),
            ..ListContainersOptions::default()
        })),
    )
    .await
    .unwrap_or_else(|error| {
        errors.push(Error::new(format!("list containers: {error}")));
        Vec::new()
    });
    for container in containers {
        let id = container.id.unwrap_or_default();
        if let Err(error) = remove_container(ctx, client, &id).await {
            errors.push(error);
        }
    }

    let images = with_timeout(
        ctx,
        OP_TIMEOUT,
        client.list_images(Some(ListImagesOptions {
            filters: Some(label_filters(label_filter(env_id))),
            ..ListImagesOptions::default()
        })),
    )
    .await
    .unwrap_or_else(|error| {
        errors.push(Error::new(format!("list images: {error}")));
        Vec::new()
    });
    for image in images {
        if let Err(error) = remove_image(ctx, client, &image.id).await {
            errors.push(error);
        }
    }

    Error::join(errors).map_or(Ok(()), Err)
}

/// Force-removes the container `id`; one already gone is not an error.
///
/// # Errors
///
/// The removal failed.
pub async fn remove_container(ctx: &Context, client: &Docker, id: &str) -> Result<(), Error> {
    let removed = with_timeout(
        ctx,
        OP_TIMEOUT,
        client.remove_container(
            id,
            Some(RemoveContainerOptions {
                force: true,
                ..RemoveContainerOptions::default()
            }),
        ),
    )
    .await;
    match removed {
        Err(error) if !error.is_not_found() => {
            Err(Error::new(format!("remove container {id}: {error}")))
        }
        Ok(()) | Err(_) => Ok(()),
    }
}

/// Force-removes the image `id`; one already gone is not an error.
///
/// # Errors
///
/// The removal failed.
pub async fn remove_image(ctx: &Context, client: &Docker, id: &str) -> Result<(), Error> {
    let removed = with_timeout(
        ctx,
        OP_TIMEOUT,
        client.remove_image(
            id,
            Some(RemoveImageOptions {
                force: true,
                ..RemoveImageOptions::default()
            }),
            None,
        ),
    )
    .await;
    match removed {
        Err(error) if !error.is_not_found() => {
            Err(Error::new(format!("remove image {id}: {error}")))
        }
        Ok(_) | Err(_) => Ok(()),
    }
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
    let labels = labels
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect();
    let (exit_code, stdout, stderr) = Box::pin(answer_to_identity_flag(ctx, client, image, labels))
        .await
        .map_err(|error| error.context(format!("run image {image} with {IDENTITY_FLAG}")))?;
    check_identity(&format!("image {image}"), exit_code, &stdout, &stderr)
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
    let removed = remove_container(
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

/// A Docker call that failed, or the context it ran under ended first.
#[derive(Debug)]
pub enum OpError {
    /// The daemon or the transport failed the call.
    Docker(DockerError),
    /// The context ended first.
    Ended(Ended),
}

impl OpError {
    /// containerd's `errdefs.IsNotFound`: the daemon answered 404.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        matches!(
            self,
            Self::Docker(DockerError::DockerResponseServerError {
                status_code: 404,
                ..
            })
        )
    }
}

impl fmt::Display for OpError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Docker(error) => error.fmt(formatter),
            Self::Ended(ended) => ended.fmt(formatter),
        }
    }
}

impl std::error::Error for OpError {}

impl From<DockerError> for OpError {
    fn from(error: DockerError) -> Self {
        Self::Docker(error)
    }
}

/// Runs one Docker call under its own deadline derived from `ctx`. The call is boxed: bollard's
/// request futures are large, and every caller's future would otherwise carry one inline.
///
/// # Errors
///
/// The call failed, or `ctx` or the deadline ended first.
pub async fn with_timeout<T, E: Into<OpError>>(
    ctx: &Context,
    timeout: Duration,
    call: impl Future<Output = Result<T, E>>,
) -> Result<T, OpError> {
    ctx.with_timeout(timeout)
        .run(Box::pin(call))
        .await
        .map_err(OpError::Ended)?
        .map_err(Into::into)
}
