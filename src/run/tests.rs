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

//! The tests of `main_test.go` and `launch_test.go`'s `TestRun_DispatchesLaunch`, on
//! [`run_with`]'s exits before the daemon starts.

use std::sync::atomic::{AtomicBool, Ordering};

use super::*;

/// Runs `args` after the program name, recording whether the `SIGHUP` channel was registered.
async fn exit_of(args: &[&str]) -> (i32, bool) {
    let registered = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&registered);
    let exit = run_with(
        std::iter::once("swarm-device-access").chain(args.iter().copied()),
        |_| None,
        std::future::pending,
        move || {
            flag.store(true, Ordering::SeqCst);
            mpsc::channel(1).1
        },
    )
    .await;
    assert_eq!(exit.deadline, None);
    (exit.code, registered.load(Ordering::SeqCst))
}

/// Go: `TestRun_DispatchesLaunch`. A leading `launch` is the launcher's, before the daemon's
/// flags are parsed: `--help` and a bad `--config-dir` are the launcher's answers, and the
/// launcher takes no `SIGHUP`.
#[tokio::test]
async fn run_dispatches_launch() {
    assert_eq!(exit_of(&["launch", "--help"]).await, (0, false));
    assert_eq!(
        exit_of(&["launch", "--config-dir", "relative"]).await,
        (1, false)
    );
}

/// The help and a parse error exit before the `SIGHUP` channel is registered, as Go's run
/// returns before `signal.Notify`; a startup failure exits 1 after it.
#[tokio::test]
async fn sighup_is_registered_after_the_flags() {
    assert_eq!(exit_of(&["--help"]).await, (0, false));
    assert_eq!(exit_of(&["-dry-run"]).await, (2, false));
    assert_eq!(exit_of(&["--config=/nonexistent"]).await, (1, true));
}

/// Go: `TestStartServers_SecondFailureStopsTheFirst`, whose second server was the pprof one: with
/// Q2, a debug address fails the startup before any server binds, so the metrics address is
/// never taken.
#[tokio::test]
async fn a_debug_address_fails_before_the_metrics_server_binds() {
    let free = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = free.local_addr().unwrap().to_string();
    drop(free);
    let metrics = format!("--metrics-addr={address}");
    assert_eq!(
        exit_of(&[&metrics, "--debug-addr=127.0.0.1:0"]).await,
        (1, true)
    );
    drop(std::net::TcpListener::bind(&address).unwrap());
}

/// Go: `TestStartServers_NoneConfigured`. Without a metrics address no server starts, and the
/// stop is a no-op.
#[tokio::test]
async fn no_server_configured() {
    stop_server(None, Instant::now()).await;
}

/// The startup checks print Go's lines: the config file, then the merged settings.
#[test]
fn startup_settings_errors_read_as_go_s() {
    let parsed = |args: &[&str]| {
        cli::parse(std::iter::once("swarm-device-access").chain(args.iter().copied())).unwrap()
    };
    let check = |args: &[&str]| {
        let parsed = parsed(args);
        startup_settings(&Settings::from_flags(&parsed), &parsed).unwrap_err()
    };
    assert_eq!(
        check(&["--config=/nonexistent"]),
        "config file error: read config file \"/nonexistent\": open /nonexistent: no such file or \
         directory"
    );
    assert_eq!(
        check(&["--log-level=loud"]),
        "invalid config: setting \"log-level\": unknown value \"loud\", must be one of [debug \
         info warn error]"
    );
}
