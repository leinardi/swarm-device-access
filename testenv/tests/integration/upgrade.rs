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

//! The cross-language upgrade and rollback gate of `docs/rust-port.md`, new to the Rust suite:
//! grants survive the daemon's exit (`test/integration/enforce_test.go`, the reload subtest), so
//! after an upgrade the Rust daemon finds the Go daemon's wrappers in place, and after a rollback
//! the Go daemon finds the Rust daemon's. On one surviving container and cgroup, the Go daemon
//! grants, the Rust daemon under a narrower policy revokes, the Rust daemon grants again, and the
//! Go daemon under the narrower policy revokes once more.
//!
//! After each step the device is probed from inside the container, and the programs attached to
//! its cgroup are read (`testenv::enforce`, through `sudo -n`) and classified with the daemon's
//! own code: after a grant exactly one owned wrapper per runtime original and no stale wrapper,
//! after a revocation no owned wrapper and each runtime original attached bare exactly once, as
//! the restore plan leaves it. The wrapper goldens alone do not prove this: they never run one
//! daemon against the other's kernel state.
//!
//! The Go daemon is `SDA_GO_TEST_IMAGE`, the last published 1.x image pinned by digest, which
//! must answer the identity flag as Go; the Rust daemon is the image under test, which the runner
//! already proved Rust.

use std::process::Stdio;
use std::time::Duration;

use bollard::query_parameters::InspectContainerOptions;
use swarm_device_access::policy::LABEL_ENABLE;
use testenv::enforce::{
    Build, DUMP_FLAG, ENV_ENFORCE, ProgramDump, cgroup_problem, granted_problems, revoked_problems,
    runtime_originals,
};
use testenv::host::{OP_TIMEOUT, with_timeout};
use tokio::process::Command;

use crate::daemon::require_processed;
use crate::docker::{ensure_test_image, short_id, start_test_container};
use crate::enforce::{
    enforce_requested, pick_device, require_allowed, require_denied, require_enforce_host,
    require_or_skip, start_daemon_container, stop_daemon_container,
};
use crate::identity::require_go_image;
use crate::testing::{TestHandle, TestResult, docker, suite};

/// Bounds one read of the attached programs through sudo.
const DUMP_TIMEOUT: Duration = Duration::from_secs(30);

/// One step of the gate: which daemon runs, with which policy, and what it must leave behind.
struct Step<'step> {
    what: &'static str,
    build: Build,
    image: &'step str,
    /// The flags beyond the opt-in policy: none, or the narrower policy.
    flags: &'step [&'step str],
    /// Whether the step grants the device (or revokes it).
    grants: bool,
}

/// The cross-language upgrade and rollback gate, named in the Go tests' style; there is no Go
/// test to port. Required before the cutover (`docs/rust-port.md`, the upgrade and rollback gate):
/// the Go daemon from `SDA_GO_TEST_IMAGE` grants the device, the Rust daemon started with a
/// narrower policy revokes the Go-attached wrapper, the Rust daemon with the original policy grants
/// it again, and the restored Go daemon with the narrower policy revokes the Rust-attached wrapper,
/// all on one container whose cgroup survives every step.
///
/// It attaches BPF programs on this host and reads them as root, so it runs only with
/// `SDA_IT_ENFORCE=1`, as the Go enforcement tests do.
pub async fn go_to_rust_and_back(test: TestHandle) -> TestResult {
    if !enforce_requested() {
        return Err(test.skip(format!(
            "set {ENV_ENFORCE}=1 to run: it attaches BPF programs on this host with the Go and \
             the Rust daemon, and reads them through sudo"
        )));
    }
    ensure_test_image(&test).await?;
    let go_image = require_go_image(&test).await?;
    let rust_image = suite().image.clone();
    require_enforce_host(&test)?;

    let device = pick_device(&test).await?;
    let bind = format!("{device}:{device}");
    let target = start_test_container(&test, &[(LABEL_ENABLE, "true")], &[&bind]).await?;
    require_denied(
        &test,
        &target,
        &device,
        "labeled container before any daemon runs",
    )
    .await?;

    let baseline = read_programs(&test, &target)
        .await
        .map_err(|cause| require_or_skip(&test, enforce_requested(), &cause))?;
    let originals = runtime_originals(&baseline).map_err(|error| {
        test.fatal(format!(
            "the runtime's device programs on {}: {error}",
            baseline.cgroup
        ))
    })?;
    test.log(format!(
        "cgroup {} (inode {}) holds {} runtime original(s)",
        baseline.cgroup,
        baseline.inode,
        originals.len()
    ));

    let deny = format!("device-deny={device}");
    let narrower: &[&str] = &[&deny];
    let steps = [
        Step {
            what: "the Go daemon grants the device",
            build: Build::Go,
            image: &go_image,
            flags: &[],
            grants: true,
        },
        Step {
            what: "the Rust daemon with a narrower policy revokes the Go-attached wrapper",
            build: Build::Rust,
            image: &rust_image,
            flags: narrower,
            grants: false,
        },
        Step {
            what: "the Rust daemon with the original policy grants the device again",
            build: Build::Rust,
            image: &rust_image,
            flags: &[],
            grants: true,
        },
        Step {
            what: "the restored Go daemon with the narrower policy revokes the Rust-attached \
                   wrapper",
            build: Build::Go,
            image: &go_image,
            flags: narrower,
            grants: false,
        },
    ];
    for step in &steps {
        run_step(&test, step, &target, &device, &baseline, &originals).await?;
    }
    Ok(())
}

/// Runs one daemon until it has processed the container, checks the device access and the
/// attached programs it left, and stops it: a stop leaves the cgroup as the daemon left it, which
/// is what the next build finds, as after an upgrade or a rollback.
async fn run_step(
    test: &TestHandle,
    step: &Step<'_>,
    target: &str,
    device: &str,
    baseline: &ProgramDump,
    originals: &[Vec<u8>],
) -> TestResult {
    test.log(format!("step: {}", step.what));
    let daemon = start_daemon_container(test, step.image, step.build, step.flags).await?;
    if step.grants {
        require_processed(test, &daemon.log, target, 1, 0).await?;
        require_allowed(test, target, device, step.what).await?;
    } else {
        require_processed(test, &daemon.log, target, 0, 1).await?;
        require_denied(test, target, device, step.what).await?;
    }

    let dump = read_programs(test, target)
        .await
        .map_err(|cause| test.fatal(format!("{}: {cause}", step.what)))?;
    if let Some(problem) = cgroup_problem(baseline, &dump) {
        return Err(test.fatal(format!("{}: {problem}", step.what)));
    }
    let problems = if step.grants {
        granted_problems(originals, &dump)
    } else {
        revoked_problems(originals, &dump)
    };
    if !problems.is_empty() {
        return Err(test.fatal(format!(
            "{}: the programs attached to {}:\n{}",
            step.what,
            dump.cgroup,
            problems.join("\n")
        )));
    }
    test.log(format!(
        "{}: {} program(s) attached, as expected",
        step.what,
        dump.programs.len()
    ));
    stop_daemon_container(test, test.ctx(), &daemon).await;
    Ok(())
}

/// The device programs attached to the cgroup of `container_id`: this binary, run as root through
/// `sudo -n` with [`DUMP_FLAG`], reads them for the container's pid.
async fn read_programs(test: &TestHandle, container_id: &str) -> Result<ProgramDump, String> {
    let inspected = with_timeout(
        test.ctx(),
        OP_TIMEOUT,
        docker().inspect_container(container_id, None::<InspectContainerOptions>),
    )
    .await
    .map_err(|error| format!("inspect {}: {error}", short_id(container_id)))?;
    let pid = inspected
        .state
        .and_then(|state| state.pid)
        .filter(|pid| *pid > 0)
        .ok_or_else(|| format!("container {} has no pid", short_id(container_id)))?;
    let binary =
        std::env::current_exe().map_err(|error| format!("locate the suite's binary: {error}"))?;
    let command = format!("sudo -n {} {DUMP_FLAG} {pid}", binary.display());
    let output = test
        .ctx()
        .with_timeout(DUMP_TIMEOUT)
        .run(
            Command::new("sudo")
                .arg("-n")
                .arg(&binary)
                .arg(DUMP_FLAG)
                .arg(pid.to_string())
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|ended| format!("{command}: {ended}"))?
        .map_err(|error| format!("{command}: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "{command}: {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    ProgramDump::parse(&String::from_utf8_lossy(&output.stdout))
        .map_err(|error| format!("{command}: {error}"))
}
