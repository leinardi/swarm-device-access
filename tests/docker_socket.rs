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

//! The daemon's `--docker-socket` naming a socket that does not exist: the client is refused when
//! it is built, and the daemon exits 1 with moby's text for that dial (docs/migration.md, Q16).

use std::process::Command;

/// `--docker-socket` at a path with no socket logs `docker client init failed` with moby's
/// wording of the dial, naming the daemon host, and exits 1.
#[test]
#[expect(
    clippy::disallowed_methods,
    reason = "the test runs the daemon as a child"
)]
fn a_missing_docker_socket_is_worded_as_moby_words_it() {
    let dir = std::env::temp_dir().join(format!("sda-no-socket-{}", std::process::id()));
    let socket = dir.join("docker.sock");
    let path = socket.display();
    let output = Command::new(env!("CARGO_BIN_EXE_swarm-device-access"))
        .args(["--dry-run", &format!("--docker-socket={path}")])
        .env_clear()
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", dir.join("no-bus").display()),
        )
        .output()
        .unwrap_or_else(|error| panic!("run: {error}"));
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(output.status.code(), Some(1), "{stdout}");
    let line = stdout
        .lines()
        .find(|line| line.contains("docker client init failed"))
        .unwrap_or_else(|| panic!("no init failure in {stdout}"));
    let wanted = format!(
        "failed to connect to the docker API at unix://{path}; check if the path is correct and if \
         the daemon is running: dial unix {path}: connect: no such file or directory"
    );
    assert!(line.contains(&wanted), "{line}");
}
