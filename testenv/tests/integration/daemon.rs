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

//! The dry-run daemon tests (`test/integration/daemon_test.go`), under their Go names: the daemon
//! binary runs as an unprivileged host process with `--dry-run` and the event, inspect, policy,
//! cgroup path resolution and device discovery pipeline is checked against the host daemon,
//! without attaching BPF programs.
//!
//! The daemon sees every container on the host, not only the ones a test starts, so the
//! assertions only look at JSON log records whose `id` is the test's own container.

use std::time::Duration;

use swarm_device_access::policy::{LABEL_DEVICE_ALLOW, LABEL_DEVICE_DENY, LABEL_ENABLE};
use testenv::host::{OP_TIMEOUT, with_timeout};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpStream;

use crate::daemon_process::{
    DaemonLog, MSG_INVALID_LABELS, MSG_PROCESSED, MSG_SKIPPED_POLICY, Record, for_container,
    launch_daemon, number,
};
use crate::docker::{ensure_test_image, short_id, start_test_container};
use crate::testing::{OrFatal as _, TestHandle, TestResult, docker};

const BIND_NULL: &str = "/dev/null:/dev/null";
const BIND_ZERO: &str = "/dev/zero:/dev/zero";

/// Go: `TestDaemon_DryRun_DetectsDeviceMount`. With `--dry-run` and `--policy-mode=opt-in`, a
/// container labelled `swarm-device-access.enable=true` that bind-mounts `/dev/null` is granted
/// that one device. No BPF syscall or privilege is needed: `--dry-run` skips `SetDeviceRules` and
/// logs the intent instead.
pub async fn detects_device_mount(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    let container = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[BIND_NULL]).await?;
    require_processed(&test, &log, &container, 1, 0).await
}

/// Go: `TestDaemon_DryRun_PolicyMode_SkipsUnlabelledContainer`. `--policy-mode=opt-in` applies
/// no rule for a container without the `swarm-device-access.enable=true` label.
pub async fn policy_mode_skips_unlabelled_container(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    let container = start_test_container(&test, &[], &[BIND_NULL]).await?;
    require_skipped(&test, &log, &container, MSG_SKIPPED_POLICY).await
}

/// Go: `TestDaemon_DryRun_PolicyMode_All_ProcessesUnlabelledContainer`. `--policy-mode=all`
/// processes a container that does not carry the opt-in label.
pub async fn policy_mode_all_processes_unlabelled_container(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=all"]).await?;
    let container = start_test_container(&test, &[], &[BIND_NULL]).await?;
    require_processed(&test, &log, &container, 1, 0).await
}

/// Go: `TestDaemon_DryRun_PolicyMode_All_SkipsOptedOutContainer`. `--policy-mode=all` still
/// skips a container that sets `enable=false`.
pub async fn policy_mode_all_skips_opted_out_container(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=all"]).await?;
    let container = start_test_container(&test, &[(LABEL_ENABLE, "false")], &[BIND_NULL]).await?;
    require_skipped(&test, &log, &container, MSG_SKIPPED_POLICY).await
}

/// Go: `TestDaemon_DryRun_InvalidLabel_SkipsContainer`. A malformed policy label fails closed:
/// the container is skipped, not granted.
pub async fn invalid_label_skips_container(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=all"]).await?;
    let container = start_test_container(&test, &[(LABEL_ENABLE, "maybe")], &[BIND_NULL]).await?;
    require_skipped(&test, &log, &container, MSG_INVALID_LABELS).await
}

/// Go: `TestDaemon_DryRun_GlobalDeviceDeny_BlocksDevice`. A device matching the global
/// `--device-deny` list is skipped, not granted.
pub async fn global_device_deny_blocks_device(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(
        &test,
        &[
            "--dry-run",
            "--policy-mode=opt-in",
            "--device-deny=/dev/null",
        ],
    )
    .await?;
    let container = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[BIND_NULL]).await?;
    require_processed(&test, &log, &container, 0, 1).await
}

/// Go: `TestDaemon_DryRun_GlobalDeviceAllow_BlocksNonMatchingDevice`. A device not in the global
/// `--device-allow` list is skipped while a listed one is granted.
pub async fn global_device_allow_blocks_non_matching_device(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(
        &test,
        &[
            "--dry-run",
            "--policy-mode=opt-in",
            "--device-allow=/dev/zero",
        ],
    )
    .await?;
    let container =
        start_test_container(&test, &[(LABEL_ENABLE, "true")], &[BIND_NULL, BIND_ZERO]).await?;
    require_processed(&test, &log, &container, 1, 1).await
}

/// Go: `TestDaemon_DryRun_LabelDeviceDeny_BlocksDevice`. The `swarm-device-access.device-deny`
/// container label filters out matching devices.
pub async fn label_device_deny_blocks_device(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    let container = start_test_container(
        &test,
        &[(LABEL_ENABLE, "true"), (LABEL_DEVICE_DENY, "/dev/null")],
        &[BIND_NULL],
    )
    .await?;
    require_processed(&test, &log, &container, 0, 1).await
}

/// Go: `TestDaemon_DryRun_LabelDeviceAllow_NarrowsAccess`. The
/// `swarm-device-access.device-allow` container label restricts which devices receive rules: a
/// mount outside the per-container allow list is skipped.
pub async fn label_device_allow_narrows_access(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    let container = start_test_container(
        &test,
        &[(LABEL_ENABLE, "true"), (LABEL_DEVICE_ALLOW, "/dev/zero")],
        &[BIND_NULL, BIND_ZERO],
    )
    .await?;
    require_processed(&test, &log, &container, 1, 1).await
}

/// Go: `TestDaemon_DryRun_ProcessesExistingContainers`. The startup enumeration: a container
/// already running when the daemon starts is processed without waiting for a Docker event.
pub async fn processes_existing_containers(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    // The container starts before the daemon: no Docker event will fire for it.
    let container = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[BIND_NULL]).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    require_processed(&test, &log, &container, 1, 0).await
}

/// Go: `TestDaemon_DryRun_UnpauseEvent`. An `unpause` Docker event runs the same device-rule
/// pipeline as a `start` event.
pub async fn unpause_event(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    let container = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[BIND_NULL]).await?;
    require_processed(&test, &log, &container, 1, 0).await?;

    with_timeout(test.ctx(), OP_TIMEOUT, docker().pause_container(&container))
        .await
        .map_err(|error| test.fatal(format!("pause container: {error}")))?;
    with_timeout(
        test.ctx(),
        OP_TIMEOUT,
        docker().unpause_container(&container),
    )
    .await
    .map_err(|error| test.fatal(format!("unpause container: {error}")))?;

    // The unpause event must trigger a second processing pass.
    let records = log
        .wait_n(
            &test,
            "second container processed (unpause)",
            &for_container(MSG_PROCESSED, &container),
            2,
        )
        .await?;
    check_counts(&test, &records[1], 1, 0);
    Ok(())
}

/// Go: `TestDaemon_DryRun_MultipleDeviceMounts`. Every device bind mount of a container is
/// granted.
pub async fn multiple_device_mounts(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let log = launch_daemon(&test, &["--dry-run", "--policy-mode=opt-in"]).await?;
    let container =
        start_test_container(&test, &[(LABEL_ENABLE, "true")], &[BIND_NULL, BIND_ZERO]).await?;
    require_processed(&test, &log, &container, 2, 0).await
}

/// Go: `TestDaemon_DryRun_ConfigFile_LoadsPolicyMode`. The daemon's settings load from a YAML
/// config file given with `--config`, and the flag defaults do not override values set only in
/// the file.
pub async fn config_file_loads_policy_mode(test: TestHandle) -> TestResult {
    ensure_test_image(&test).await?;
    let dir = crate::scratch_dir(&test, "config")?;
    let config = dir.join("sda-config.yaml");
    std::fs::write(&config, "policy-mode: \"all\"\ndry-run: true\n")
        .map_err(|error| test.fatal(format!("write temp config: {error}")))?;

    // No --dry-run or --policy-mode flag: both come from the config file.
    let log = launch_daemon(&test, &[&format!("--config={}", config.display())]).await?;
    let container = start_test_container(&test, &[], &[BIND_NULL]).await?;
    let record = log
        .wait(
            &test,
            &format!("container processed for {}", short_id(&container)),
            &for_container(MSG_PROCESSED, &container),
        )
        .await?;
    check_counts(&test, &record, 1, 0);
    if record.get("dry_run").and_then(serde_json::Value::as_bool) != Some(true) {
        test.error(format!(
            "config file dry-run: true not applied; record {record:?}"
        ));
    }
    Ok(())
}

/// Go: `TestDaemon_DryRun_MetricsEndpoint`. `--metrics-addr` starts a Prometheus endpoint that
/// serves the `sda_` metrics.
pub async fn metrics_endpoint(test: TestHandle) -> TestResult {
    let metrics_addr = free_local_addr().or_fatal(&test)?;
    launch_daemon(
        &test,
        &[
            "--dry-run",
            "--policy-mode=opt-in",
            &format!("--metrics-addr={metrics_addr}"),
        ],
    )
    .await?;

    let (status, body) = test
        .ctx()
        .run(get(&metrics_addr, "/metrics"))
        .await
        .or_fatal(&test)?
        .map_err(|error| test.fatal(format!("GET /metrics: {error}")))?;
    if status != 200 {
        return Err(test.fatal(format!("GET /metrics: want 200, got {status}")));
    }
    if !body.contains("# HELP sda_") {
        test.error(format!(
            "GET /metrics: expected Prometheus help text for sda_ metrics;\nbody:\n{body}"
        ));
    }
    Ok(())
}

/// Go's `requireProcessed`: waits for the daemon to process `container_id` and checks the exact
/// counts of its `container processed` record.
async fn require_processed(
    test: &TestHandle,
    log: &DaemonLog,
    container_id: &str,
    granted: i64,
    skipped: i64,
) -> TestResult {
    let record = log
        .wait(
            test,
            &format!("container processed for {}", short_id(container_id)),
            &for_container(MSG_PROCESSED, container_id),
        )
        .await?;
    check_counts(test, &record, granted, skipped);
    Ok(())
}

/// Go's `checkCounts`: the granted, skipped and error counts of a `container processed` record.
fn check_counts(test: &TestHandle, record: &Record, granted: i64, skipped: i64) {
    for (key, want) in [
        ("devices_granted", granted),
        ("skipped", skipped),
        ("errors", 0),
    ] {
        let got = number(record, key);
        if got != Some(want) {
            test.error(format!(
                "container processed: {key} = {:?}, want {want} (record {record:?})",
                record.get(key)
            ));
        }
    }
}

/// Go's `requireSkipped`: waits for a skip record about `container_id` and checks that the
/// daemon did not process the container anyway. A skip and a processed record are alternative
/// outcomes of one pass, so once the skip is logged no processed record for that pass can follow.
async fn require_skipped(
    test: &TestHandle,
    log: &DaemonLog,
    container_id: &str,
    skip_message: &str,
) -> TestResult {
    log.wait(
        test,
        &format!("{skip_message:?} for {}", short_id(container_id)),
        &for_container(skip_message, container_id),
    )
    .await?;
    let processed = log.matching(&for_container(MSG_PROCESSED, container_id));
    if !processed.is_empty() {
        test.error(format!(
            "container {} was skipped but also processed: {processed:?}",
            short_id(container_id)
        ));
    }
    Ok(())
}

/// Go's `freeLocalAddr`: a loopback address with a port that was free a moment ago. Another
/// process can still take it before the daemon binds it; the window is small enough for a test.
fn free_local_addr() -> Result<String, String> {
    let listener = std::net::TcpListener::bind("127.0.0.1:0")
        .map_err(|error| format!("probe free port: {error}"))?;
    listener
        .local_addr()
        .map(|address| address.to_string())
        .map_err(|error| format!("probe free port: {error}"))
}

/// A plain HTTP/1.0 `GET`, so the answer is neither chunked nor kept alive: its status and body.
async fn get(address: &str, path: &str) -> Result<(u16, String), String> {
    let mut stream = TcpStream::connect(address)
        .await
        .map_err(|error| error.to_string())?;
    stream
        .write_all(format!("GET {path} HTTP/1.0\r\nHost: {address}\r\n\r\n").as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    let mut answer = Vec::new();
    tokio::time::timeout(Duration::from_secs(30), stream.read_to_end(&mut answer))
        .await
        .map_err(|_| "no answer within 30s".to_owned())?
        .map_err(|error| error.to_string())?;
    let answer = String::from_utf8_lossy(&answer).into_owned();
    let (head, body) = answer
        .split_once("\r\n\r\n")
        .ok_or_else(|| format!("malformed answer: {answer:?}"))?;
    let status = head
        .split(' ')
        .nth(1)
        .and_then(|code| code.parse().ok())
        .ok_or_else(|| format!("malformed status line: {head:?}"))?;
    Ok((status, body.to_owned()))
}
