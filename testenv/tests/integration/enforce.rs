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

//! The real-enforcement tests (`test/integration/enforce_test.go`), under their Go names, and the
//! helpers they share with the upgrade test: the daemon image runs for real (no `--dry-run`) as a
//! privileged container with the runtime options of `.mk/docker-run.mk`, and device access is
//! probed from inside the test containers.
//!
//! They attach BPF programs, and the reload subtest runs `sudo -n systemctl daemon-reload`, so they
//! run only with `SDA_IT_ENFORCE=1`, which also turns every missing prerequisite into a failure;
//! `SDA_IT_REQUIRE_RELOAD=1` does the same for a host that cannot prove the reload scenario. The
//! image is the Rust one the identity check accepted, never the Go suite's `SDA_TEST_IMAGE`.

use std::os::unix::fs::{FileTypeExt as _, MetadataExt as _};
use std::sync::Arc;
use std::time::Duration;

use bollard::container::LogOutput;
use bollard::exec::StartExecResults;
use bollard::models::{
    ContainerCreateBody, DeviceMapping, ExecConfig, HostConfig, HostConfigCgroupnsModeEnum,
};
use bollard::query_parameters::{KillContainerOptions, StopContainerOptions};
use futures_util::StreamExt as _;
use swarm_device_access::policy::LABEL_ENABLE;
use testenv::context::Context;
use testenv::enforce::{
    Build, ENV_ENFORCE, ENV_REQUIRE_RELOAD, Verdict, flag_set, missing_prerequisite,
    reload_unsupported,
};
use testenv::host::{OP_TIMEOUT, with_timeout};
use tokio::process::Command;
use tokio::time::Instant;

use crate::daemon::{check_counts, require_processed, require_skipped};
use crate::daemon_process::{
    DaemonLog, MSG_PROCESSED, MSG_SKIPPED_POLICY, MSG_WATCHER_STARTED, follow_logs, for_container,
    with_msg,
};
use crate::docker::start_test_container;
use crate::docker::{CLEANUP_TIMEOUT, TEST_IMAGE, ensure_test_image, short_id, start_container};
use crate::testing::{Fatal, TestHandle, TestResult, docker, suite};

/// Host paths the daemon container needs, as in `.mk/docker-run.mk`.
const HOST_DOCKER_SOCKET: &str = "/var/run/docker.sock";
const HOST_DBUS_SOCKET: &str = "/run/dbus/system_bus_socket";
/// `dhi.io/static` has no `/var/run -> /run` symlink, so the socket is mounted under `/var/run`
/// inside the container; the Rust image keeps the path.
const CONTAINER_DBUS_SOCKET: &str = "/var/run/dbus/system_bus_socket";

const MSG_DRY_RUN_LEFTOVERS: &str =
    "dry-run: grants left by a previous live run cannot be detected or cleaned in dry-run";
const MSG_EPERM: &str = "Operation not permitted";

/// Bounds how long daemon-reload may take to remove the device program after systemctl returns.
const WIPE_TIMEOUT: Duration = Duration::from_secs(5);
/// Spaces the probes while waiting for the wipe, and the polls of a probe's exit code.
const PROBE_POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Bounds one `systemctl daemon-reload`.
const RELOAD_TIMEOUT: Duration = Duration::from_secs(30);
/// The grace period before Docker kills the daemon.
const DAEMON_STOP_TIMEOUT_SECS: i32 = 10;

/// Tried in order. Each sits outside Docker's default device cgroup allow list, and root with
/// Docker's default capabilities can open it once the cgroup allows it, so the daemon's rule is
/// the only thing that changes the probe's result. `/dev/kmsg` is not a candidate: without
/// `CAP_SYSLOG`, `dmesg_restrict` can still deny it after the cgroup allows it.
const ENFORCE_DEVICES: [&str; 2] = [
    "/dev/loop-control", // char 10:237
    "/dev/loop0",        // block 7:0, opened read-only
];

/// Go's `enforceRequested`: whether the run opted into the tests that change the host. Opting in
/// also makes every missing prerequisite a failure.
pub fn enforce_requested() -> bool {
    flag_set(std::env::var(ENV_ENFORCE).ok().as_deref())
}

/// Go's `requireOrSkip`: stops the test for the missing prerequisite `cause`, failing it when
/// `strict` and skipping it otherwise ([`missing_prerequisite`]).
pub fn require_or_skip(test: &TestHandle, strict: bool, cause: &str) -> Fatal {
    stop(test, missing_prerequisite(strict, cause))
}

fn stop(test: &TestHandle, verdict: Verdict) -> Fatal {
    match verdict {
        Verdict::Fail(message) => test.fatal(message),
        Verdict::Skip(message) => test.skip(message),
    }
}

/// Go: `TestEnforce_GrantsDeviceAccess`. Runs the daemon image for real and checks that an
/// opted-in container can open a bind-mounted device only after the daemon attached its BPF
/// program (and no neighbouring node), that an unlabelled sibling still cannot, and, in the reload
/// subtest, that the daemon re-applies the rule after `systemctl daemon-reload` wiped it.
pub async fn grants_device_access(test: TestHandle) -> TestResult {
    if !enforce_requested() {
        return Err(test.skip(format!(
            "set {ENV_ENFORCE}=1 to run: it attaches BPF programs and runs systemctl \
             daemon-reload on this host"
        )));
    }
    ensure_test_image(&test).await?;
    let image = suite().image.clone();
    require_enforce_host(&test)?;

    let device = pick_device(&test).await?;
    let bind = format!("{device}:{device}");
    let target = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[&bind]).await?;
    require_denied(
        &test,
        &target,
        &device,
        "labeled container before the daemon runs",
    )
    .await?;

    let daemon = start_daemon_container(&test, &image, Build::Rust, &[]).await?;
    require_processed(&test, &daemon.log, &target, 1, 0).await?;
    require_allowed(
        &test,
        &target,
        &device,
        "labeled container after the daemon granted it",
    )
    .await?;
    require_no_over_grant(&test, &target, &device).await?;

    let sibling = start_test_container(&test, &[], &[&bind]).await?;
    require_skipped(&test, &daemon.log, &sibling, MSG_SKIPPED_POLICY).await?;
    require_denied(&test, &sibling, &device, "unlabelled sibling").await?;

    test.run("reload", async move |test| {
        reload_reapply(&test, &image, &daemon, &target, &device).await
    })
    .await;
    Ok(())
}

/// Go: `TestEnforce_RestartReconcilesLeftoverGrants`. Kills a daemon that granted a device, so it
/// cannot clean up, then restarts it. A dry-run restart must leave the grant alone (dry-run never
/// touches cgroups) and say so; a live restart under a policy that denies the device must revoke
/// it.
pub async fn restart_reconciles_leftover_grants(test: TestHandle) -> TestResult {
    if !enforce_requested() {
        return Err(test.skip(format!(
            "set {ENV_ENFORCE}=1 to run: it attaches BPF programs on this host"
        )));
    }
    ensure_test_image(&test).await?;
    let image = suite().image.clone();
    require_enforce_host(&test)?;

    let device = pick_device(&test).await?;
    let bind = format!("{device}:{device}");
    let target = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[&bind]).await?;

    let first = start_daemon_container(&test, &image, Build::Rust, &[]).await?;
    require_processed(&test, &first.log, &target, 1, 0).await?;
    require_allowed(&test, &target, &device, "after the first daemon granted it").await?;
    kill_daemon_container(&test, &first).await?;

    let dry = start_daemon_container(&test, &image, Build::Rust, &["dry-run"]).await?;
    dry.log
        .wait(
            &test,
            "the dry-run leftover warning",
            &with_msg(MSG_DRY_RUN_LEFTOVERS),
        )
        .await?;
    require_processed(&test, &dry.log, &target, 1, 0).await?;
    require_allowed(
        &test,
        &target,
        &device,
        "after a dry-run restart (not a cleanup path)",
    )
    .await?;
    kill_daemon_container(&test, &dry).await?;

    let deny = format!("device-deny={device}");
    let narrow = start_daemon_container(&test, &image, Build::Rust, &[&deny]).await?;
    require_processed(&test, &narrow.log, &target, 0, 1).await?;
    require_denied(
        &test,
        &target,
        &device,
        "after a live restart with a policy that denies it",
    )
    .await
}

/// Go's `testReloadReapply`: first proves that daemon-reload wipes the device program on this
/// host, then that the daemon puts it back. Without the wipe check a host that never wipes would
/// pass the re-apply assertion without the daemon doing anything.
async fn reload_reapply(
    test: &TestHandle,
    image: &str,
    first: &DaemonContainer,
    target: &str,
    device: &str,
) -> TestResult {
    stop_daemon_container(test, test.ctx(), first).await;

    // The program is attached by the daemon, not held by a link it owns, so it outlives the
    // daemon.
    require_allowed(
        test,
        target,
        device,
        "labeled container after the daemon stopped",
    )
    .await?;

    if let Err(cause) = daemon_reload(test.ctx()).await {
        return Err(reload_not_proven(test, &cause));
    }
    if let Some(cause) = wait_denied(test, target, device).await? {
        return Err(reload_not_proven(
            test,
            &format!("daemon-reload did not wipe the device program: {cause}"),
        ));
    }

    // Denied, not open: the reload removed the daemon's wrapper, but systemd attached the unit's
    // own device filter again, so the cgroup is not left without one. A restarted daemon, whose
    // filter cache is empty, can wrap that filter; it never reaches the filter_missing path here.
    let daemon = start_daemon_container(test, image, Build::Rust, &[]).await?;
    daemon
        .log
        .wait(
            test,
            "systemd reload watcher",
            &with_msg(MSG_WATCHER_STARTED),
        )
        .await?;

    // Startup enumeration re-grants the container the reload stripped.
    require_processed(test, &daemon.log, target, 1, 0).await?;
    require_allowed(
        test,
        target,
        device,
        "labeled container after the restarted daemon granted it",
    )
    .await?;

    let processed_target = for_container(MSG_PROCESSED, target);
    let before = daemon.log.matching(&processed_target).len();
    daemon_reload(test.ctx()).await.map_err(|cause| {
        test.fatal(format!(
            "second daemon-reload failed after the first succeeded: {cause}"
        ))
    })?;

    // Wait for the re-apply pass's own record. The "re-applying device rules" line is logged
    // before the callback runs, so waiting on it would race the re-apply.
    let records = daemon
        .log
        .wait_n(
            test,
            "container processed after daemon-reload",
            &processed_target,
            before + 1,
        )
        .await?;
    if let Some(record) = records.get(before) {
        check_counts(test, record, 1, 0);
    }

    // Exactly one probe, no retry: a failure means the re-apply did not take or systemd wiped the
    // rule again after it. Both are product bugs.
    require_allowed(
        test,
        target,
        device,
        "labeled container after daemon-reload and re-apply",
    )
    .await
}

/// Go's `reloadUnsupported`: stops the reload subtest on a host where the reload scenario cannot
/// prove anything, failing it under `SDA_IT_REQUIRE_RELOAD=1` ([`reload_unsupported`]).
fn reload_not_proven(test: &TestHandle, cause: &str) -> Fatal {
    let required = flag_set(std::env::var(ENV_REQUIRE_RELOAD).ok().as_deref());
    stop(test, reload_unsupported(required, cause))
}

/// Go's `daemonReload`: `systemctl daemon-reload` through non-interactive sudo.
async fn daemon_reload(ctx: &Context) -> Result<(), String> {
    let output = ctx
        .with_timeout(RELOAD_TIMEOUT)
        .run(
            Command::new("sudo")
                .args(["-n", "systemctl", "daemon-reload"])
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|ended| format!("sudo -n systemctl daemon-reload: {ended}"))?
        .map_err(|error| format!("sudo -n systemctl daemon-reload: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let mut combined = output.stdout;
    combined.extend_from_slice(&output.stderr);
    Err(format!(
        "sudo -n systemctl daemon-reload: {}: {}",
        output.status,
        String::from_utf8_lossy(&combined).trim()
    ))
}

// ---- prerequisites ----

/// Go's `requireEnforceHost`: what the daemon container needs from the host: cgroup v2 for the
/// BPF device filter, and systemd with its D-Bus socket for the reload watcher.
pub fn require_enforce_host(test: &TestHandle) -> TestResult {
    for (path, what) in [
        ("/sys/fs/cgroup/cgroup.controllers", "cgroup v2"),
        ("/run/systemd/system", "systemd as init"),
        (HOST_DBUS_SOCKET, "systemd DBus socket"),
    ] {
        if let Err(error) = std::fs::metadata(path) {
            return Err(require_or_skip(
                test,
                enforce_requested(),
                &format!("{what}: stat {path}: {error}"),
            ));
        }
    }
    Ok(())
}

/// Go's `pickDevice`: the first candidate that exists on the host as a device node and that a
/// control container given it with `--device` can open. The control proves the probe fails only
/// because of the device cgroup.
pub async fn pick_device(test: &TestHandle) -> Result<String, Fatal> {
    let mut problems = Vec::new();
    for device in ENFORCE_DEVICES {
        let metadata = match std::fs::metadata(device) {
            Ok(metadata) => metadata,
            Err(error) => {
                problems.push(format!("stat {device}: {error}"));
                continue;
            }
        };
        let kind = metadata.file_type();
        if !kind.is_char_device() && !kind.is_block_device() {
            problems.push(format!(
                "{device} (mode {:o}): not a device node",
                metadata.mode()
            ));
            continue;
        }

        let control = start_container(
            test,
            ContainerCreateBody {
                image: Some(TEST_IMAGE.to_owned()),
                cmd: Some(vec!["sleep".to_owned(), "300".to_owned()]),
                host_config: Some(HostConfig {
                    devices: Some(vec![DeviceMapping {
                        path_on_host: Some(device.to_owned()),
                        path_in_container: Some(device.to_owned()),
                        cgroup_permissions: Some("rwm".to_owned()),
                    }]),
                    ..HostConfig::default()
                }),
                ..ContainerCreateBody::default()
            },
        )
        .await?;
        let (exit_code, stderr) = probe_open(test, &control, device).await?;
        if exit_code != 0 {
            problems.push(format!(
                "{device}: exit {exit_code}: {stderr}: control container given the device with \
                 --device cannot open it"
            ));
            continue;
        }
        test.log(format!("using device {device}"));
        return Ok(device.to_owned());
    }
    Err(require_or_skip(
        test,
        enforce_requested(),
        &format!("no usable device: {}", problems.join("\n")),
    ))
}

// ---- daemon container ----

/// A daemon running in a container, and its JSON log.
#[derive(Clone)]
pub struct DaemonContainer {
    /// The container's ID.
    pub id: String,
    /// The records of its log stream.
    pub log: Arc<DaemonLog>,
}

/// Go's `startDaemonContainer`: runs the daemon `image` (a `build` daemon) with the runtime
/// options of `.mk/docker-run.mk`, the opt-in policy, JSON debug logging and `flags`, spelled for
/// that build, and waits until it is ready. It is stopped, then removed, when the test ends.
pub async fn start_daemon_container(
    test: &TestHandle,
    image: &str,
    build: Build,
    flags: &[&str],
) -> Result<DaemonContainer, Fatal> {
    let id = start_container(
        test,
        ContainerCreateBody {
            image: Some(image.to_owned()),
            cmd: Some(build.daemon_args(flags)),
            host_config: Some(HostConfig {
                privileged: Some(true),
                cgroupns_mode: Some(HostConfigCgroupnsModeEnum::HOST),
                pid_mode: Some("host".to_owned()),
                userns_mode: Some("host".to_owned()),
                binds: Some(vec![
                    format!("{HOST_DOCKER_SOCKET}:/var/run/docker.sock"),
                    "/sys:/host/sys".to_owned(),
                    // Without /dev the daemon cannot stat the device node and never grants it.
                    "/dev:/dev".to_owned(),
                    format!("{HOST_DBUS_SOCKET}:{CONTAINER_DBUS_SOCKET}"),
                ]),
                ..HostConfig::default()
            }),
            ..ContainerCreateBody::default()
        },
    )
    .await?;
    let daemon = DaemonContainer {
        log: follow_logs(test, &id),
        id,
    };

    // Registered after start_container's removal, so it runs first: the log stream ends once the
    // container stops.
    let (cleanup_test, cleanup_daemon) = (test.clone(), daemon.clone());
    test.cleanup(move || async move {
        let ctx = cleanup_test.ctx().without_cancel();
        stop_daemon_container(&cleanup_test, &ctx, &cleanup_daemon).await;
    });

    test.log(format!(
        "started the {build} daemon {} from {image}",
        short_id(&daemon.id)
    ));
    daemon.log.wait_ready(test).await?;
    Ok(daemon)
}

/// Go's `stopDaemonContainer`: stops the daemon and waits until its log stream ends. Stopping an
/// already stopped container is a no-op.
pub async fn stop_daemon_container(test: &TestHandle, ctx: &Context, daemon: &DaemonContainer) {
    let grace = Duration::from_secs(u64::from(DAEMON_STOP_TIMEOUT_SECS.unsigned_abs()));
    let stop_ctx = ctx.with_timeout(CLEANUP_TIMEOUT + grace);
    if let Err(error) = with_timeout(
        &stop_ctx,
        OP_TIMEOUT,
        docker().stop_container(
            &daemon.id,
            Some(StopContainerOptions {
                t: Some(DAEMON_STOP_TIMEOUT_SECS),
                signal: None,
            }),
        ),
    )
    .await
    {
        test.log(format!(
            "stop daemon container {}: {error}",
            short_id(&daemon.id)
        ));
    }
    if let Err(ended) = stop_ctx.run(daemon.log.wait_exited()).await {
        test.error(format!(
            "daemon container {} log stream did not end: {ended}",
            short_id(&daemon.id)
        ));
    }
}

/// Go's `killDaemonContainer`: kills the daemon with SIGKILL, so it cannot clean up, and waits
/// until its log stream ends.
async fn kill_daemon_container(test: &TestHandle, daemon: &DaemonContainer) -> TestResult {
    with_timeout(
        test.ctx(),
        OP_TIMEOUT,
        docker().kill_container(
            &daemon.id,
            Some(KillContainerOptions {
                signal: "KILL".to_owned(),
            }),
        ),
    )
    .await
    .map_err(|error| {
        test.fatal(format!(
            "kill daemon container {}: {error}",
            short_id(&daemon.id)
        ))
    })?;
    test.ctx()
        .run(daemon.log.wait_exited())
        .await
        .map_err(|ended| {
            test.fatal(format!(
                "daemon container {} log stream did not end: {ended}",
                short_id(&daemon.id)
            ))
        })
}

// ---- device probe ----

/// Go's `probeOpen`: opens `device` read-only inside `container_id` and returns the exit code and
/// stderr of the attempt.
async fn probe_open(
    test: &TestHandle,
    container_id: &str,
    device: &str,
) -> Result<(i64, String), Fatal> {
    exec_in(
        test,
        container_id,
        &["sh", "-c", "exec 3< \"$1\"", "probe", device],
    )
    .await
}

/// Go's `requireNoOverGrant`: creates, inside the opted-in container, nodes the daemon must not
/// have granted along with `device`: the same major with the next minor, and the same numbers as
/// the other device type (a block node when `device` is a character device). Opening each must
/// still be denied.
async fn require_no_over_grant(test: &TestHandle, container_id: &str, device: &str) -> TestResult {
    let metadata =
        std::fs::metadata(device).map_err(|error| test.fatal(format!("stat {device}: {error}")))?;
    let rdev = metadata.rdev();
    let (major, minor) = (rustix::fs::major(rdev), rustix::fs::minor(rdev));
    let (kind, other) = if metadata.file_type().is_block_device() {
        ("b", "c")
    } else {
        ("c", "b")
    };
    let next_minor = minor
        .checked_add(1)
        .ok_or_else(|| test.fatal(format!("{device}: minor {minor} has no next")))?;

    for (path, node_kind, node_minor) in [
        ("/tmp/sda-next-minor", kind, next_minor),
        ("/tmp/sda-other-type", other, minor),
    ] {
        let (major_text, minor_text) = (major.to_string(), node_minor.to_string());
        let node = ["mknod", path, node_kind, &major_text, &minor_text];
        let (exit_code, stderr) = exec_in(test, container_id, &node).await?;
        if exit_code != 0 {
            return Err(test.fatal(format!(
                "mknod {node:?} in the opted-in container: exit {exit_code}: {stderr}"
            )));
        }
        require_denied(
            test,
            container_id,
            path,
            &format!("node {node_kind} {major}:{node_minor} next to the granted {device}"),
        )
        .await?;
    }
    Ok(())
}

/// Go's `execIn`: runs `command` inside `container_id` and returns its exit code and stderr.
async fn exec_in(
    test: &TestHandle,
    container_id: &str,
    command: &[&str],
) -> Result<(i64, String), Fatal> {
    let ctx = test.ctx();
    let short = short_id(container_id);
    let created = with_timeout(
        ctx,
        OP_TIMEOUT,
        docker().create_exec(
            container_id,
            ExecConfig {
                cmd: Some(command.iter().map(|&arg| arg.to_owned()).collect()),
                attach_stdout: Some(true),
                attach_stderr: Some(true),
                ..ExecConfig::default()
            },
        ),
    )
    .await
    .map_err(|error| test.fatal(format!("create probe exec in {short}: {error}")))?;
    let started = with_timeout(ctx, OP_TIMEOUT, docker().start_exec(&created.id, None))
        .await
        .map_err(|error| test.fatal(format!("attach probe exec in {short}: {error}")))?;
    let mut stderr = Vec::new();
    match started {
        StartExecResults::Attached { mut output, input } => {
            drop(input);
            loop {
                let next = ctx.run(output.next()).await.map_err(|ended| {
                    test.fatal(format!("read probe output in {short}: {ended}"))
                })?;
                match next {
                    None => break,
                    Some(Err(error)) => {
                        return Err(test.fatal(format!("read probe output in {short}: {error}")));
                    }
                    Some(Ok(LogOutput::StdErr { message })) => stderr.extend_from_slice(&message),
                    Some(Ok(
                        LogOutput::StdOut { .. }
                        | LogOutput::StdIn { .. }
                        | LogOutput::Console { .. },
                    )) => {}
                }
            }
        }
        StartExecResults::Detached => {
            return Err(test.fatal(format!("probe exec in {short} started detached")));
        }
    }

    // The stream ends when the process exits, but the exit code can lag.
    loop {
        let inspected = with_timeout(ctx, OP_TIMEOUT, docker().inspect_exec(&created.id))
            .await
            .map_err(|error| test.fatal(format!("inspect probe exec in {short}: {error}")))?;
        if !inspected.running.unwrap_or_default() {
            let exit_code = inspected.exit_code.ok_or_else(|| {
                test.fatal(format!("inspect probe exec in {short}: no exit code"))
            })?;
            return Ok((
                exit_code,
                String::from_utf8_lossy(&stderr).trim().to_owned(),
            ));
        }
        ctx.run(tokio::time::sleep(PROBE_POLL_INTERVAL))
            .await
            .map_err(|ended| test.fatal(format!("probe exec in {short} still running: {ended}")))?;
    }
}

/// Go's `requireAllowed`: opening `device` in `container_id` must succeed.
pub async fn require_allowed(
    test: &TestHandle,
    container_id: &str,
    device: &str,
    what: &str,
) -> TestResult {
    let (exit_code, stderr) = probe_open(test, container_id, device).await?;
    if exit_code != 0 {
        return Err(test.fatal(format!(
            "{what}: open {device} failed (exit {exit_code}): {stderr}"
        )));
    }
    Ok(())
}

/// Go's `requireDenied`: opening `device` in `container_id` must fail because the device cgroup
/// denies it.
pub async fn require_denied(
    test: &TestHandle,
    container_id: &str,
    device: &str,
    what: &str,
) -> TestResult {
    match check_denied(test, container_id, device).await? {
        None => Ok(()),
        Some(problem) => Err(test.fatal(format!("{what}: {problem}"))),
    }
}

/// Go's `checkDenied`: `None` when opening `device` fails because the device cgroup denies it,
/// what happened instead otherwise.
async fn check_denied(
    test: &TestHandle,
    container_id: &str,
    device: &str,
) -> Result<Option<String>, Fatal> {
    let (exit_code, stderr) = probe_open(test, container_id, device).await?;
    if exit_code == 0 {
        return Ok(Some(format!("{device}: open succeeded, want {MSG_EPERM}")));
    }
    if !stderr.contains(MSG_EPERM) {
        return Ok(Some(format!(
            "{device}: exit {exit_code}: {stderr:?}: open failed with an error other than \
             {MSG_EPERM}"
        )));
    }
    Ok(None)
}

/// Go's `waitDenied`: polls until opening `device` is denied (`None`) or [`WIPE_TIMEOUT`] passes
/// (the last problem).
async fn wait_denied(
    test: &TestHandle,
    container_id: &str,
    device: &str,
) -> Result<Option<String>, Fatal> {
    let deadline = Instant::now() + WIPE_TIMEOUT;
    loop {
        let problem = check_denied(test, container_id, device).await?;
        if problem.is_none() || Instant::now() >= deadline {
            return Ok(problem);
        }
        if let Err(ended) = test
            .ctx()
            .run(tokio::time::sleep(PROBE_POLL_INTERVAL))
            .await
        {
            return Ok(Some(format!("waiting for the wipe: {ended}")));
        }
    }
}
