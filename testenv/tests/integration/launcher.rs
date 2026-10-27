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

//! The launcher test (`test/integration/launcher_test.go`), under its Go name: the image under
//! test in launcher mode, with only the Docker socket, creating the privileged daemon container.

use std::collections::HashMap;
use std::time::Duration;

use bollard::errors::Error as DockerError;
use bollard::models::{ContainerCreateBody, ContainerSummary, HostConfig};
use bollard::query_parameters::{
    InspectContainerOptions, KillContainerOptions, ListContainersOptions, StopContainerOptions,
    WaitContainerOptions,
};
use futures_util::StreamExt as _;
use swarm_device_access::launcher::spec::{DAEMON_NAME, LABEL_LAUNCHER, LABEL_ROLE, ROLE_DAEMON};
use testenv::context::Context;
use testenv::enforce::ENV_ENFORCE;
use testenv::host::{OP_TIMEOUT, with_timeout};

use crate::daemon_process::follow_logs;
use crate::docker::{CLEANUP_TIMEOUT, remove_test_container, short_id, start_container};
use crate::enforce::enforce_requested;
use crate::testing::{Fatal, TestHandle, TestResult, docker, suite};

/// The host's Docker socket, the one mount the launcher needs.
const HOST_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// Matches the compose files' `stop_grace_period`: the launcher needs up to its shutdown budget to
/// stop the daemon.
const LAUNCHER_STOP_TIMEOUT_SECS: i32 = 30;

/// Go: `TestLauncher_RunsDaemonFromItsOwnImage`. Starts the image in launcher mode with only the
/// Docker socket, and checks the daemon container it creates, a graceful stop, and the
/// replacement of a daemon orphaned by a killed launcher.
///
/// It creates a privileged container named `swarm-device-access`, so it runs only with
/// `SDA_IT_ENFORCE=1`, as in Go. The image is the Rust one the identity check accepted.
pub async fn runs_daemon_from_its_own_image(test: TestHandle) -> TestResult {
    if !enforce_requested() {
        return Err(test.skip(format!(
            "set {ENV_ENFORCE}=1 to run: it creates a privileged {DAEMON_NAME} container on this \
             host"
        )));
    }
    let image = suite().image.clone();

    let daemons = list_daemons(&test, test.ctx()).await?;
    if let Some(daemon) = daemons.first() {
        return Err(test.fatal(format!(
            "a {DAEMON_NAME} container already exists ({}); remove it first",
            short_id(daemon.id.as_deref().unwrap_or_default())
        )));
    }

    let cleanup_test = test.clone();
    test.cleanup(move || async move { remove_test_daemons(&cleanup_test).await });

    let spec_image = image.clone();
    test.run("spec and graceful stop", async move |test| {
        spec_and_graceful_stop(&test, &spec_image).await
    })
    .await;

    test.run("orphan replaced", async move |test| {
        orphan_replaced(&test, &image).await
    })
    .await;
    Ok(())
}

async fn spec_and_graceful_stop(test: &TestHandle, image: &str) -> TestResult {
    let launcher_id = start_launcher(test, image).await?;
    require_daemon_spec(test, &launcher_id).await?;

    with_timeout(
        test.ctx(),
        Duration::from_secs(60),
        docker().stop_container(
            &launcher_id,
            Some(StopContainerOptions {
                t: Some(LAUNCHER_STOP_TIMEOUT_SECS),
                signal: None,
            }),
        ),
    )
    .await
    .map_err(|error| test.fatal(format!("stop launcher: {error}")))?;

    let status = wait_exit(test, &launcher_id).await?;
    if status != 0 {
        test.error(format!("launcher exit status {status}, want 0"));
    }

    let daemons = list_daemons(test, test.ctx()).await?;
    if let Some(daemon) = daemons.first() {
        test.error(format!(
            "daemon {} still exists after its launcher stopped",
            short_id(daemon.id.as_deref().unwrap_or_default())
        ));
    }
    Ok(())
}

async fn orphan_replaced(test: &TestHandle, image: &str) -> TestResult {
    let first = start_launcher(test, image).await?;
    with_timeout(
        test.ctx(),
        OP_TIMEOUT,
        docker().kill_container(
            &first,
            Some(KillContainerOptions {
                signal: "KILL".to_owned(),
            }),
        ),
    )
    .await
    .map_err(|error| test.fatal(format!("kill launcher: {error}")))?;
    wait_exit(test, &first).await?;

    let second = start_launcher(test, image).await?;
    let daemons = list_daemons(test, test.ctx()).await?;
    let owned_by_second = daemons.len() == 1
        && daemons[0]
            .labels
            .as_ref()
            .and_then(|labels| labels.get(LABEL_LAUNCHER))
            == Some(&second);
    if !owned_by_second {
        return Err(test.fatal(format!(
            "daemons {daemons:?}, want exactly one, owned by launcher {}",
            short_id(&second)
        )));
    }
    Ok(())
}

/// Go's `startLauncher`: runs the image in launcher mode with only the Docker socket mounted and a
/// dry-run daemon behind it, and waits until the daemon's startup, as seen in the launcher's own
/// output, is complete. The flags are the Rust binary's double-dash ones (Q1).
async fn start_launcher(test: &TestHandle, image: &str) -> Result<String, Fatal> {
    let launcher_id = start_container(
        test,
        ContainerCreateBody {
            image: Some(image.to_owned()),
            cmd: Some(
                [
                    "launch",
                    "--log-format=json",
                    "--",
                    "--dry-run",
                    "--log-format=json",
                    "--log-level=debug",
                ]
                .map(str::to_owned)
                .to_vec(),
            ),
            host_config: Some(HostConfig {
                binds: Some(vec![format!("{HOST_DOCKER_SOCKET}:/var/run/docker.sock")]),
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        },
    )
    .await?;
    follow_logs(test, &launcher_id).wait_ready(test).await?;
    Ok(launcher_id)
}

/// Go's `requireDaemonSpec`: the daemon container against the documented privileged set: the
/// launcher's image, host namespaces, no network, and exactly the three bind mounts, given as
/// `Mounts`.
async fn require_daemon_spec(test: &TestHandle, launcher_id: &str) -> TestResult {
    let launcher = with_timeout(
        test.ctx(),
        OP_TIMEOUT,
        docker().inspect_container(launcher_id, None::<InspectContainerOptions>),
    )
    .await
    .map_err(|error| test.fatal(format!("inspect launcher: {error}")))?;
    let daemon = with_timeout(
        test.ctx(),
        OP_TIMEOUT,
        docker().inspect_container(DAEMON_NAME, None::<InspectContainerOptions>),
    )
    .await
    .map_err(|error| test.fatal(format!("inspect daemon: {error}")))?;

    if daemon.image != launcher.image {
        test.error(format!(
            "daemon image {:?}, want the launcher's {:?}",
            daemon.image, launcher.image
        ));
    }
    let owner = daemon
        .config
        .as_ref()
        .and_then(|config| config.labels.as_ref())
        .and_then(|labels| labels.get(LABEL_LAUNCHER))
        .cloned()
        .unwrap_or_default();
    if owner != launcher_id {
        test.error(format!(
            "daemon launcher label {owner:?}, want {launcher_id}"
        ));
    }

    let host = daemon.host_config.unwrap_or_default();
    let got_host = (
        host.privileged.unwrap_or_default(),
        host.cgroupns_mode
            .map(|mode| mode.to_string())
            .unwrap_or_default(),
        host.pid_mode.unwrap_or_default(),
        host.userns_mode.unwrap_or_default(),
        host.network_mode.unwrap_or_default(),
        host.auto_remove.unwrap_or_default(),
    );
    let want_host = (
        true,
        "host".to_owned(),
        "host".to_owned(),
        "host".to_owned(),
        "none".to_owned(),
        true,
    );
    if got_host != want_host {
        test.error(format!("host config {got_host:?}, want {want_host:?}"));
    }

    if let Some(binds) = host.binds.filter(|binds| !binds.is_empty()) {
        test.error(format!("binds {binds:?}, want Mounts only"));
    }

    let want = [
        "/var/run/docker.sock:/var/run/docker.sock",
        "/sys:/host/sys",
        "/dev:/dev",
    ];
    let mut got = Vec::new();
    for mount in host.mounts.unwrap_or_default() {
        let kind = mount.typ.map(|kind| kind.to_string()).unwrap_or_default();
        let target = mount.target.unwrap_or_default();
        if kind != "bind" {
            test.error(format!("mount {target} is {kind}, want bind"));
        }
        got.push(format!("{}:{target}", mount.source.unwrap_or_default()));
    }
    if got != want {
        test.error(format!("mounts {got:?}, want {want:?}"));
    }
    Ok(())
}

/// Go's `listDaemons`: every container named `swarm-device-access`.
async fn list_daemons(test: &TestHandle, ctx: &Context) -> Result<Vec<ContainerSummary>, Fatal> {
    with_timeout(
        ctx,
        OP_TIMEOUT,
        docker().list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "name".to_owned(),
                vec![format!("^/{DAEMON_NAME}$")],
            )])),
            ..ListContainersOptions::default()
        })),
    )
    .await
    .map_err(|error| test.fatal(format!("list daemon containers: {error}")))
}

/// Go's `waitExit`: waits until `container_id` has stopped and returns its exit status.
async fn wait_exit(test: &TestHandle, container_id: &str) -> Result<i64, Fatal> {
    let wait_timeout =
        Duration::from_secs(u64::from(LAUNCHER_STOP_TIMEOUT_SECS.unsigned_abs())) + CLEANUP_TIMEOUT;
    let mut waits = docker().wait_container(
        container_id,
        Some(WaitContainerOptions {
            condition: "not-running".to_owned(),
        }),
    );
    match test
        .ctx()
        .with_timeout(wait_timeout)
        .run(waits.next())
        .await
    {
        Ok(Some(Ok(exit))) => Ok(exit.status_code),
        // bollard reports a non-zero exit status as an error carrying it.
        Ok(Some(Err(DockerError::DockerContainerWaitError { code, .. }))) => Ok(code),
        Ok(Some(Err(error))) => {
            Err(test.fatal(format!("wait for {}: {error}", short_id(container_id))))
        }
        Ok(None) => Err(test.fatal(format!("wait for {}: no answer", short_id(container_id)))),
        Err(ended) => Err(test.fatal(format!("wait for {}: {ended}", short_id(container_id)))),
    }
}

/// Go's `removeTestDaemons`: removes a daemon a failed test left behind. Its launchers are this
/// run's containers, removed by their own cleanups.
async fn remove_test_daemons(test: &TestHandle) {
    let ctx = test.ctx().without_cancel().with_timeout(CLEANUP_TIMEOUT);
    let Ok(daemons) = list_daemons(test, &ctx).await else {
        return;
    };
    for daemon in daemons {
        let is_daemon = daemon
            .labels
            .as_ref()
            .and_then(|labels| labels.get(LABEL_ROLE))
            .is_some_and(|role| role == ROLE_DAEMON);
        if is_daemon && let Some(id) = daemon.id {
            remove_test_container(test, &id).await;
        }
    }
}
