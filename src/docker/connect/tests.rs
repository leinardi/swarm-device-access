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

use super::{ConnectError, Connector};

/// Go's `net.OpError` text of a socket that does not exist, which moby's errors quote: the dial
/// is reported with the path and the errno text.
#[tokio::test]
async fn a_missing_socket_is_a_dial_error_worded_as_go() {
    let directory = std::env::temp_dir().join(format!("sda-connect-{}", std::process::id()));
    let socket = directory.join("missing.sock").display().to_string();
    let error = Connector::new(socket.clone())
        .connect()
        .await
        .expect_err("no socket there");
    assert!(matches!(error, ConnectError::Dial { .. }));
    assert_eq!(
        error.dial_error().and_then(std::io::Error::raw_os_error),
        Some(2)
    );
    assert!(!error.is_timeout());
    assert_eq!(
        error.to_string(),
        format!("dial unix {socket}: connect: no such file or directory")
    );
}

/// A dial that runs out of time is Go's `i/o timeout`, a timeout as `net.Error` reports it.
#[test]
fn a_dial_timeout_reads_as_go() {
    let error = ConnectError::DialTimeout {
        address: "/var/run/docker.sock".to_owned(),
    };
    assert!(error.is_timeout());
    assert!(error.dial_error().is_none());
    assert_eq!(
        error.to_string(),
        "dial unix /var/run/docker.sock: i/o timeout"
    );
}
