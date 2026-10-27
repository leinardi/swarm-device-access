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

//! The identity check's negative control, new to the Rust suite: the check the runner applies to
//! the image under test, applied to a throwaway `FROM scratch` image of the Go binary, must refuse
//! it. Without it, a check that accepted everything would pass every run.

use std::process::{Output, Stdio};
use std::time::Duration;

use testenv::envid::resource_name;
use testenv::host::{check_image_identity, remove_image};
use testenv::identity::is_static_elf;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

use crate::docker::CLEANUP_TIMEOUT;
use crate::testing::{Fatal, TestHandle, TestResult, docker, suite};

/// The variable naming the Go daemon binary, the one the Go suite runs (`make go-build`).
pub const ENV_GO_BINARY: &str = "SDA_TEST_BINARY";

/// Bounds the docker build of the Go image.
const BUILD_TIMEOUT: Duration = Duration::from_secs(120);

/// Where the binary sits in both throwaway images.
pub const IMAGE_BINARY_PATH: &str = "/usr/local/bin/swarm-device-access";

/// Builds a `FROM scratch` image of the static Go binary and checks that the identity check
/// refuses it, as the Go daemon answers `-help` with its usage and exit 0.
pub async fn refuses_go_image(test: TestHandle) -> TestResult {
    let binary = std::env::var(ENV_GO_BINARY).unwrap_or_default();
    if binary.is_empty() {
        return Err(test.fatal(format!(
            "{ENV_GO_BINARY} must name the Go daemon binary (make go-build; make \
             rust-test-integration sets it)"
        )));
    }
    let bytes = std::fs::read(&binary)
        .map_err(|error| test.fatal(format!("read the Go binary {binary}: {error}")))?;
    let is_static = is_static_elf(&bytes)
        .map_err(|error| test.fatal(format!("the Go binary {binary}: {error}")))?;
    if !is_static {
        return Err(test.fatal(format!(
            "the Go binary {binary} is dynamic: a FROM scratch image cannot run it"
        )));
    }

    let image = build_scratch_image(&test, &bytes).await?;
    let refused = check_image_identity(test.ctx(), docker(), &image, &suite().labels).await;
    match refused {
        Err(error)
            if error
                .to_string()
                .contains("is not the Rust daemon: -help exited 0") =>
        {
            test.log(format!("refused as expected: {error}"));
            Ok(())
        }
        Err(error) => Err(test.fatal(format!(
            "the Go image {image} was refused for another reason: {error}"
        ))),
        Ok(()) => Err(test.fatal(format!("the identity check accepted the Go image {image}"))),
    }
}

/// Builds the throwaway image of `binary` with the run's labels, removed when the test ends, and
/// returns its tag. The context is a directory under cargo's temporary directory, also removed.
async fn build_scratch_image(test: &TestHandle, binary: &[u8]) -> Result<String, Fatal> {
    let dir = crate::scratch_dir(test, "go-image")?;
    std::fs::write(dir.join("swarm-device-access"), binary)
        .map_err(|error| test.fatal(format!("write the image context: {error}")))?;
    set_executable(&dir.join("swarm-device-access"))
        .map_err(|error| test.fatal(format!("write the image context: {error}")))?;
    let tag = format!("{}:latest", resource_name(&suite().env_id, "go-identity"));
    let mut command = Command::new("docker");
    command.args(["build", "--quiet", "--tag", &tag]);
    for (key, value) in &suite().labels {
        command.args(["--label", &format!("{key}={value}")]);
    }
    // The Dockerfile goes on stdin, so no second Dockerfile ever exists in the tree, where the
    // Go meta-test `TestShippedDockerfileIsTheOnlyOne` would find it.
    command
        .args(["--file", "-"])
        .arg(&dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let cleanup_test = test.clone();
    let cleanup_tag = tag.clone();
    test.cleanup(move || async move {
        let ctx = cleanup_test
            .ctx()
            .without_cancel()
            .with_timeout(CLEANUP_TIMEOUT);
        if let Err(error) = remove_image(&ctx, docker(), &cleanup_tag).await {
            cleanup_test.log(format!("remove image {cleanup_tag}: {error}"));
        }
    });

    let dockerfile = format!(
        "FROM scratch\nCOPY swarm-device-access {IMAGE_BINARY_PATH}\nENTRYPOINT \
         [\"{IMAGE_BINARY_PATH}\"]\n"
    );
    let output = test
        .ctx()
        .with_timeout(BUILD_TIMEOUT)
        .run(build(command, dockerfile))
        .await
        .map_err(|ended| test.fatal(format!("docker build {tag}: {ended}")))?
        .map_err(|error| test.fatal(format!("docker build {tag}: {error}")))?;
    if !output.status.success() {
        return Err(test.fatal(format!(
            "docker build {tag}: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    test.log(format!("built the Go image {tag}"));
    Ok(tag)
}

/// Runs the docker build `command` with `dockerfile` on its stdin.
async fn build(mut command: Command, dockerfile: String) -> std::io::Result<Output> {
    let mut child = command.spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(dockerfile.as_bytes()).await?;
    }
    child.wait_with_output().await
}

/// Makes `path` executable by everyone, as `COPY` keeps the mode.
fn set_executable(path: &std::path::Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt as _;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
}
