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

// Adapted from swarm-scheduler-exporter/tests/run.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The binary's shutdown: one budget from the first signal, later signals swallowed.

use std::os::unix::net::UnixListener;
use std::process::{Command, Stdio};
use std::time::Duration;

use tokio::time::Instant;

use swarm_device_access::run::SHUTDOWN_BUDGET;

/// With every Docker request hanging, `SIGTERM` still ends the daemon with exit 0 within the
/// shutdown budget, and a `SIGINT` right after it is swallowed rather than killing the process.
#[test]
fn shutdown_within_budget_while_docker_blocks() {
    let dir = std::env::temp_dir().join(format!("sda-shutdown-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("create: {error}"));
    let socket = dir.join("docker.sock");
    let listener = UnixListener::bind(&socket).unwrap_or_else(|error| panic!("bind: {error}"));
    std::thread::spawn(move || {
        let mut held = Vec::new();
        for stream in listener.incoming().flatten() {
            held.push(stream);
        }
    });
    let mut child = Command::new(env!("CARGO_BIN_EXE_swarm-device-access"))
        .args([
            "--dry-run",
            &format!("--docker-socket={}", socket.display()),
        ])
        .env_clear()
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", dir.join("no-bus").display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn: {error}"));
    // Negative window: the first Docker request is in flight, and hangs.
    std::thread::sleep(Duration::from_millis(500));
    let signal = |name: &str| {
        Command::new("kill")
            .args([name, &child.id().to_string()])
            .status()
            .unwrap_or_else(|error| panic!("kill: {error}"))
            .success()
    };
    let start = Instant::now();
    assert!(signal("-TERM"), "kill -TERM");
    // Unchecked: the daemon may already be gone. If it is not, the signal must be swallowed, or
    // the exit below is a death by SIGINT, with no code.
    let _ = signal("-INT");
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|error| panic!("wait: {error}"))
        {
            break Some(status);
        }
        if start.elapsed() > SHUTDOWN_BUDGET + Duration::from_secs(5) {
            let _ = child.kill();
            break None;
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let elapsed = start.elapsed();
    let _ = std::fs::remove_dir_all(&dir);
    assert_eq!(status.and_then(|status| status.code()), Some(0));
    assert!(
        elapsed <= SHUTDOWN_BUDGET + Duration::from_millis(100),
        "exit after {elapsed:?}"
    );
}
