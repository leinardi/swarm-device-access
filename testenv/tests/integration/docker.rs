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

//! The Docker side of Go's `helpers_test.go`: the one pull of the test image, and the containers
//! a test starts, each carrying the run's labels and removed when the test ends.

use std::collections::HashMap;
use std::time::Duration;

use bollard::models::{ContainerCreateBody, HostConfig};
use bollard::query_parameters::{
    CreateContainerOptions, CreateImageOptions, StartContainerOptions,
};
use futures_util::StreamExt as _;
use testenv::host::{OP_TIMEOUT, remove_container, with_timeout};

use crate::testing::{Fatal, TestHandle, docker, suite};

/// The image every test container runs.
pub const TEST_IMAGE: &str = "docker.io/library/busybox:1.36";
/// Bounds the one pull of [`TEST_IMAGE`].
const IMAGE_PULL_TIMEOUT: Duration = Duration::from_secs(45);
/// Bounds each teardown call. Teardown runs after the test context is cancelled, so it uses a
/// context of its own.
pub const CLEANUP_TIMEOUT: Duration = Duration::from_secs(10);

/// The first 12 characters of a container ID, as Docker prints it.
pub fn short_id(id: &str) -> &str {
    id.get(..12).unwrap_or(id)
}

/// Go's `ensureTestImage`: pulls [`TEST_IMAGE`] once per run; every test that needs it fails if
/// that pull did.
pub async fn ensure_test_image(test: &TestHandle) -> Result<(), Fatal> {
    let ctx = test.ctx().clone();
    let pulled = suite()
        .test_image_pull
        .get_or_init(async || {
            let pull = async {
                let mut progress = docker().create_image(
                    Some(CreateImageOptions {
                        from_image: Some(TEST_IMAGE.to_owned()),
                        ..CreateImageOptions::default()
                    }),
                    None,
                    None,
                );
                while let Some(step) = progress.next().await {
                    step.map_err(|error| error.to_string())?;
                }
                Ok::<(), String>(())
            };
            ctx.with_timeout(IMAGE_PULL_TIMEOUT)
                .run(pull)
                .await
                .map_err(|ended| ended.to_string())
                .flatten()
        })
        .await;
    pulled
        .clone()
        .map_err(|error| test.fatal(format!("pull test image {TEST_IMAGE}: {error}")))
}

/// Go's `startTestContainer`: creates and starts a busybox container sleeping for 300 s, with
/// `labels`, the run's labels and `binds`. It is removed when the test ends.
pub async fn start_test_container(
    test: &TestHandle,
    labels: &[(&str, &str)],
    binds: &[&str],
) -> Result<String, Fatal> {
    start_container(
        test,
        ContainerCreateBody {
            image: Some(TEST_IMAGE.to_owned()),
            cmd: Some(vec!["sleep".to_owned(), "300".to_owned()]),
            labels: Some(
                labels
                    .iter()
                    .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
                    .collect(),
            ),
            host_config: Some(HostConfig {
                binds: Some(binds.iter().map(|&bind| bind.to_owned()).collect()),
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        },
    )
    .await
}

/// Go's `startContainer`: creates and starts a container from `body`, adding the run's labels to
/// its own. It is removed when the test ends.
pub async fn start_container(
    test: &TestHandle,
    mut body: ContainerCreateBody,
) -> Result<String, Fatal> {
    let image = body.image.clone().unwrap_or_default();
    let mut labels: HashMap<String, String> = suite().labels.clone();
    labels.extend(body.labels.take().unwrap_or_default());
    body.labels = Some(labels);

    let ctx = test.ctx().clone();
    let created = with_timeout(
        &ctx,
        OP_TIMEOUT,
        docker().create_container(None::<CreateContainerOptions>, body),
    )
    .await
    .map_err(|error| test.fatal(format!("create container from {image}: {error}")))?;
    let id = created.id;

    let (cleanup_test, cleanup_id) = (test.clone(), id.clone());
    test.cleanup(move || async move {
        remove_test_container(&cleanup_test, &cleanup_id).await;
    });

    with_timeout(
        &ctx,
        OP_TIMEOUT,
        docker().start_container(&id, None::<StartContainerOptions>),
    )
    .await
    .map_err(|error| test.fatal(format!("start container from {image}: {error}")))?;
    test.log(format!("started container {} from {image}", short_id(&id)));
    Ok(id)
}

/// Go's `removeContainer`: force-removes `container_id`. It runs as a cleanup, after the test
/// context is cancelled, so it drops that cancellation and relies on [`CLEANUP_TIMEOUT`].
pub async fn remove_test_container(test: &TestHandle, container_id: &str) {
    let ctx = test.ctx().without_cancel().with_timeout(CLEANUP_TIMEOUT);
    if let Err(error) = remove_container(&ctx, docker(), container_id).await {
        test.log(format!(
            "remove container {}: {error}",
            short_id(container_id)
        ));
    }
}
