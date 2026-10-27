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

//! A placeholder for the daemon's entry point while the port is in progress. It is built and
//! linted with the workspace but goes into no image: the Go binary still ships. It refuses to run,
//! so that nothing mistakes it for a working daemon.

use std::io::{self, Write as _};
use std::process::ExitCode;

fn main() -> ExitCode {
    // Nothing useful can be done when stderr itself cannot be written; the exit status still says
    // the run failed.
    let _written = writeln!(
        io::stderr(),
        "swarm-device-access {}: the Rust port is not usable yet; run the Go build",
        option_env!("SWARM_DEVICE_ACCESS_VERSION").unwrap_or("dev")
    );
    ExitCode::FAILURE
}
