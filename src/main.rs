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

// Adapted from swarm-scheduler-exporter/src/main.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The swarm-device-access binary: runs the daemon, or the launcher, on a runtime of its own and
//! returns its exit code. The Go binary still ships until the cutover.

use std::io::Write as _;
use std::process::ExitCode;
use std::time::Duration;

use swarm_device_access::run::{hangup_channel, run_with, shutdown_signal};

/// The runtime's worker threads: the daemon's work is a few Docker calls and blocking cgroup
/// transactions, which run on the blocking pool, so two workers keep the resident set small.
const WORKER_THREADS: usize = 2;

/// Builds the runtime by hand, rather than with `#[tokio::main]`, so its shutdown is bounded by
/// what is left of the shutdown budget: dropping a runtime waits for its blocking tasks, with no
/// bound at all.
fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(WORKER_THREADS)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "tokio runtime: {error}");
            return ExitCode::FAILURE;
        }
    };
    let exit = runtime.block_on(run_with(
        std::env::args_os(),
        |key| std::env::var(key).ok(),
        shutdown_signal,
        hangup_channel,
    ));
    let remaining = exit.deadline.map_or(Duration::ZERO, |deadline| {
        deadline.saturating_duration_since(tokio::time::Instant::now())
    });
    runtime.shutdown_timeout(remaining);
    ExitCode::from(u8::try_from(exit.code).unwrap_or(1))
}
