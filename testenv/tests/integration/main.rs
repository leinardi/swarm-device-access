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
// Adapted from swarm-scheduler-exporter/testenv/tests/integration/main.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The integration suite (`harness = false`): the Rust port of `test/integration`, against the
//! host Docker daemon. It runs the Rust daemon binary (`SDA_RUST_TEST_BINARY`) as a host process
//! in dry-run, and the Rust daemon image (`SDA_RUST_TEST_IMAGE`) in launcher mode; both are
//! required. `make test-integration` builds the image from the shipped Dockerfile and copies the
//! binary out of it, so the host process runs the image's own static bytes.
//!
//! Before any test, the runner proves the binary and the image are Rust: each must answer `-help`
//! as the Rust daemon does (exit 2 and the single-dash refusal, Q1), where Go prints its usage
//! and exits 0, and the binary must be static. A mismatch, a missing variable or an unreachable
//! Docker daemon fails the run; none of them skips it. Then it sweeps the abandoned runs of earlier crashed suites.
//!
//! The real-enforcement tests, and the cross-language upgrade gate, which also runs the Go daemon
//! image (`SDA_GO_TEST_IMAGE`, proven Go by the same identity flag), run only with
//! `SDA_IT_ENFORCE=1`; the upgrade gate reads the attached programs by running this binary as
//! root with `--dump-device-programs <pid>`, which prints them and runs no test.
//!
//! As Go's `TestMain` did, the run has one deadline (`SDA_IT_SUITE_TIMEOUT`, default 4m), which
//! SIGINT and SIGTERM also end, so blocked waits return and cleanups still run. Tests run one
//! after another and keep their Go names. `--run <pattern>` selects them with Go's `-run`
//! semantics, subtests included, and a pattern that selects no test fails the run. Each test and
//! subtest prints one `--- PASS`, `--- FAIL` or `--- SKIP` line with its seconds, then the run
//! prints the selected, passed, skipped and failed counts; any failure exits 1.

mod daemon;
mod daemon_process;
mod docker;
mod enforce;
mod identity;
mod launcher;
mod testing;
mod upgrade;

use std::collections::HashMap;
use std::io::Write as _;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use bollard::query_parameters::ListContainersOptions;
use go_clock::{Clock as _, SystemClock};
use go_compat::duration;
use go_compat::local_zone::local_zone;
use go_slog::Attr;
use testenv::cleanup_command;
use testenv::config::{ENV_IDS, LABELS};
use testenv::enforce::{DUMP_FLAG, dump_device_programs};
use testenv::host::{
    HOST_SWEEP, check_image_identity, default_sweep_rules, host_client, with_timeout,
};
use testenv::identity::check_identity;
use testenv_core::context::Context;
use testenv_core::identity::{IDENTITY_FLAG, is_static_elf};
use testenv_core::redact::RedactingLogger;
use testenv_core::run_filter::RunFilter;
use testenv_core::sweep_rules::current_owner;
use testenv_core::testing::{Runner, RunnerConfig, TallyStyle, Test, TimeoutRule, say};
use tokio::process::Command;
use tokio::sync::OnceCell;

use crate::testing::{Fatal, Suite, TestHandle, install, suite};

/// The Rust daemon binary under test, run as a host process.
const ENV_BINARY: &str = "SDA_RUST_TEST_BINARY";
/// The Rust daemon image under test.
const ENV_IMAGE: &str = "SDA_RUST_TEST_IMAGE";
/// Bounds the setup's own Docker calls and the binary's identity run.
const SETUP_TIMEOUT: Duration = Duration::from_secs(60);
/// Bounds the final check and cleanup of whatever a test left with the run's labels.
const TEARDOWN_TIMEOUT: Duration = Duration::from_mins(1);

/// The runner's lines and exit codes, as Go's `TestMain` and `runSuite` printed them: the deadline
/// (`SDA_IT_SUITE_TIMEOUT`, Go's `defaultSuiteTimeout` of 4m without it) must be a positive
/// duration, a test that starts after it fails at once, and the tally counts the skipped tests.
const RUNNER: RunnerConfig = RunnerConfig {
    prefix: "[it]",
    usage: "usage: integration [--run <pattern>]",
    timeout_env: "SDA_IT_SUITE_TIMEOUT",
    default_timeout: Duration::from_mins(4),
    timeout_rule: TimeoutRule::Positive,
    deadline_check: true,
    tally: TallyStyle::WithSkipped,
};

/// Every test, in the order they run: Go's order, then the cross-language upgrade gate and the
/// identity check's negative control.
const TESTS: &[Test] = &[
    Test {
        name: "TestDaemon_DryRun_DetectsDeviceMount",
        run: |test| Box::pin(daemon::detects_device_mount(test)),
    },
    Test {
        name: "TestDaemon_DryRun_PolicyMode_SkipsUnlabelledContainer",
        run: |test| Box::pin(daemon::policy_mode_skips_unlabelled_container(test)),
    },
    Test {
        name: "TestDaemon_DryRun_PolicyMode_All_ProcessesUnlabelledContainer",
        run: |test| Box::pin(daemon::policy_mode_all_processes_unlabelled_container(test)),
    },
    Test {
        name: "TestDaemon_DryRun_PolicyMode_All_SkipsOptedOutContainer",
        run: |test| Box::pin(daemon::policy_mode_all_skips_opted_out_container(test)),
    },
    Test {
        name: "TestDaemon_DryRun_InvalidLabel_SkipsContainer",
        run: |test| Box::pin(daemon::invalid_label_skips_container(test)),
    },
    Test {
        name: "TestDaemon_DryRun_GlobalDeviceDeny_BlocksDevice",
        run: |test| Box::pin(daemon::global_device_deny_blocks_device(test)),
    },
    Test {
        name: "TestDaemon_DryRun_GlobalDeviceAllow_BlocksNonMatchingDevice",
        run: |test| Box::pin(daemon::global_device_allow_blocks_non_matching_device(test)),
    },
    Test {
        name: "TestDaemon_DryRun_LabelDeviceDeny_BlocksDevice",
        run: |test| Box::pin(daemon::label_device_deny_blocks_device(test)),
    },
    Test {
        name: "TestDaemon_DryRun_LabelDeviceAllow_NarrowsAccess",
        run: |test| Box::pin(daemon::label_device_allow_narrows_access(test)),
    },
    Test {
        name: "TestDaemon_DryRun_ProcessesExistingContainers",
        run: |test| Box::pin(daemon::processes_existing_containers(test)),
    },
    Test {
        name: "TestDaemon_DryRun_UnpauseEvent",
        run: |test| Box::pin(daemon::unpause_event(test)),
    },
    Test {
        name: "TestDaemon_DryRun_MultipleDeviceMounts",
        run: |test| Box::pin(daemon::multiple_device_mounts(test)),
    },
    Test {
        name: "TestDaemon_DryRun_ConfigFile_LoadsPolicyMode",
        run: |test| Box::pin(daemon::config_file_loads_policy_mode(test)),
    },
    Test {
        name: "TestDaemon_DryRun_MetricsEndpoint",
        run: |test| Box::pin(daemon::metrics_endpoint(test)),
    },
    Test {
        name: "TestEnforce_GrantsDeviceAccess",
        run: |test| Box::pin(enforce::grants_device_access(test)),
    },
    Test {
        name: "TestEnforce_RestartReconcilesLeftoverGrants",
        run: |test| Box::pin(enforce::restart_reconciles_leftover_grants(test)),
    },
    Test {
        name: "TestLauncher_RunsDaemonFromItsOwnImage",
        run: |test| Box::pin(launcher::runs_daemon_from_its_own_image(test)),
    },
    Test {
        name: "TestUpgrade_GoToRustAndBack",
        run: |test| Box::pin(upgrade::go_to_rust_and_back(test)),
    },
    Test {
        name: "TestIdentity_RefusesGoImage",
        run: |test| Box::pin(identity::refuses_go_image(test)),
    },
];

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, pid] = args.as_slice()
        && flag == DUMP_FLAG
    {
        return dump_programs(pid);
    }
    let runner = Runner::new(RUNNER);
    let (filter, tests) = match runner.select(&args, TESTS) {
        Ok(selected) => selected,
        Err(code) => return ExitCode::from(code),
    };
    let runtime = match runner.runtime() {
        Ok(runtime) => runtime,
        Err(code) => return ExitCode::from(code),
    };
    runtime.block_on(run_suite(&runner, filter, &tests))
}

/// The suite's binary run as root by the upgrade test (`sudo -n <binary> --dump-device-programs
/// <pid>`): prints the device programs attached to the cgroup of `pid` and exits, running no test.
fn dump_programs(pid: &str) -> ExitCode {
    let dumped = pid
        .parse()
        .map_err(|error| format!("{DUMP_FLAG} {pid}: {error}"))
        .and_then(|pid| dump_device_programs(pid).map_err(|error| error.to_string()));
    match dumped {
        Ok(dump) => {
            let mut stdout = std::io::stdout().lock();
            match stdout
                .write_all(dump.render().as_bytes())
                .and_then(|()| stdout.flush())
            {
                Ok(()) => ExitCode::SUCCESS,
                Err(error) => {
                    say(&format!("[it] write the dump: {error}"));
                    ExitCode::FAILURE
                }
            }
        }
        Err(message) => {
            say(&format!("[it] {message}"));
            ExitCode::FAILURE
        }
    }
}

/// A variable that must be set, named in the failure with what sets it.
fn required(key: &str) -> Result<String, String> {
    let value = std::env::var(key).unwrap_or_default();
    if value.is_empty() {
        return Err(format!("{key} must be set (make test-integration sets it)"));
    }
    Ok(value)
}

/// Runs `binary` with the identity flag as a host process and checks the answer is the Rust
/// daemon's.
async fn check_binary_identity(ctx: &Context, binary: &str) -> Result<(), String> {
    let output = ctx
        .with_timeout(SETUP_TIMEOUT)
        .run(
            Command::new(binary)
                .arg(IDENTITY_FLAG)
                .kill_on_drop(true)
                .output(),
        )
        .await
        .map_err(|ended| format!("run {binary} {IDENTITY_FLAG}: {ended}"))?
        .map_err(|error| format!("run {binary} {IDENTITY_FLAG}: {error}"))?;
    let exit_code = output
        .status
        .code()
        .ok_or_else(|| format!("run {binary} {IDENTITY_FLAG}: {}", output.status))?;
    check_identity(
        &format!("binary {binary}"),
        i64::from(exit_code),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    )
    .map_err(|error| error.to_string())
}

/// Everything before the first test: the variables, the identity of the binary and the image,
/// the Docker client and the sweep. Any failure ends the run.
async fn set_up(ctx: &Context, timeout: Duration) -> Result<(), String> {
    let binary = required(ENV_BINARY)?;
    let image = required(ENV_IMAGE)?;
    if !PathBuf::from(&binary).is_file() {
        return Err(format!("{ENV_BINARY}={binary} is not a file"));
    }
    let bytes =
        std::fs::read(&binary).map_err(|error| format!("read {ENV_BINARY}={binary}: {error}"))?;
    if !is_static_elf(&bytes).map_err(|error| format!("{ENV_BINARY}={binary}: {error}"))? {
        return Err(format!(
            "{ENV_BINARY}={binary} is dynamic; the binary the image ships is static"
        ));
    }

    let env_id = ENV_IDS.generate().map_err(|error| error.to_string())?;
    let owner = current_owner().map_err(|error| error.to_string())?;
    let labels = LABELS.run_labels(&env_id, &owner, SystemClock.now());
    say(&format!(
        "integration run {env_id} (deadline {}); if it is killed, remove leftovers with:\n  {}",
        duration::format(i64::try_from(timeout.as_nanos()).unwrap_or(i64::MAX)),
        cleanup_command(&env_id)
    ));

    check_binary_identity(ctx, &binary).await?;
    let docker = host_client().map_err(|error| error.to_string())?;
    let setup = ctx.with_timeout(SETUP_TIMEOUT);
    setup
        .run(docker.ping())
        .await
        .map_err(|ended| format!("docker daemon not reachable: {ended}"))?
        .map_err(|error| format!("docker daemon not reachable: {error}"))?;
    check_image_identity(&setup, &docker, &image, &labels)
        .await
        .map_err(|error| error.to_string())?;
    say(&format!(
        "[it] identity: binary {binary} and image {image} are the Rust daemon"
    ));

    let rules = default_sweep_rules(&env_id, &owner).map_err(|error| error.to_string())?;
    let log = RedactingLogger::new(
        Box::new(std::io::stderr()),
        local_zone(|key| std::env::var(key).ok()),
        Arc::new(SystemClock),
    )
    .with_attrs(&[Attr::string("envid", env_id.clone())]);
    HOST_SWEEP.sweep(&setup, &docker, &rules, &log).await;

    install(Suite {
        docker,
        binary: PathBuf::from(binary),
        image,
        env_id,
        labels,
        ctx: ctx.clone(),
        test_image_pull: OnceCell::new(),
    });
    Ok(())
}

async fn run_suite(runner: &Runner, filter: RunFilter, tests: &[&Test]) -> ExitCode {
    // Read and checked before anything else, as Go's runSuite did: a bad value is a usage error.
    let timeout = match runner.suite_timeout(|key| std::env::var(key).ok()) {
        Ok(timeout) => timeout,
        Err(code) => return ExitCode::from(code),
    };
    let ctx = match runner.suite_context(timeout) {
        Ok(ctx) => ctx,
        Err(code) => return ExitCode::from(code),
    };
    if let Err(message) = set_up(&ctx, timeout).await {
        runner.say(&message);
        return ExitCode::FAILURE;
    }

    let failed = runner.run_tests(tests, &suite().ctx, &filter).await.failed;

    let mut code = if failed > 0 {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    };
    if let Err(message) = assert_nothing_left().await {
        runner.say(&format!("the tests left resources behind: {message}"));
        code = ExitCode::FAILURE;
    }
    code
}

/// After the last test, no container carries the run's envid any more: each test removes what
/// it created. What is left is removed here, and fails the run, since a test whose cleanup leaks
/// would fill the host one run at a time.
async fn assert_nothing_left() -> Result<(), String> {
    let teardown = Context::background().with_timeout(TEARDOWN_TIMEOUT);
    let env_id = &suite().env_id;
    let containers = with_timeout(
        &teardown,
        TEARDOWN_TIMEOUT,
        suite().docker.list_containers(Some(ListContainersOptions {
            all: true,
            filters: Some(HashMap::from([(
                "label".to_owned(),
                vec![ENV_IDS.label_filter(env_id)],
            )])),
            ..ListContainersOptions::default()
        })),
    )
    .await
    .map_err(|error| format!("list containers: {error}"))?;
    if containers.is_empty() {
        return Ok(());
    }
    let removed = HOST_SWEEP.cleanup(&teardown, &suite().docker, env_id).await;
    Err(format!(
        "{} containers still carry {}={env_id}{}",
        containers.len(),
        ENV_IDS.label,
        removed.err().map_or_else(String::new, |error| format!(
            "; cleanup: {error}\nClean up with:\n  {}",
            cleanup_command(env_id)
        ))
    ))
}

/// A directory for one test's files under cargo's temporary directory, removed when the test
/// ends: Go's `t.TempDir`.
///
/// # Errors
///
/// The directory cannot be created; the test has failed.
pub fn scratch_dir(test: &TestHandle, name: &str) -> Result<PathBuf, Fatal> {
    let dir = PathBuf::from(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("it-{}", suite().env_id))
        .join(name);
    std::fs::create_dir_all(&dir)
        .map_err(|error| test.fatal(format!("create {}: {error}", dir.display())))?;
    let cleanup_test = test.clone();
    let cleanup_dir = dir.clone();
    test.cleanup(move || async move {
        if let Err(error) = std::fs::remove_dir_all(&cleanup_dir) {
            cleanup_test.log(format!("remove {}: {error}", cleanup_dir.display()));
        }
        // The run's directory goes with its last test directory; one still in use stays.
        if let Some(run_dir) = cleanup_dir.parent() {
            let _ = std::fs::remove_dir(run_dir);
        }
    });
    Ok(dir)
}
