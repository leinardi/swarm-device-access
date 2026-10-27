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

// Adapted from swarm-scheduler-exporter/src/docker/host/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

use super::{escape_host, escape_path, join_path, unix_socket};

/// moby client v0.6.0's `New(WithHost("unix://" + socket))` on these sockets, as a Go program
/// printed its errors: an empty path cannot be parsed, a path longer than 108 bytes is too long,
/// and a relative path is taken as it is.
#[test]
fn unix_sockets_are_checked_as_moby_checks_them() {
    assert_eq!(
        unix_socket("").unwrap_err(),
        "unable to parse docker host `unix://`"
    );
    let longest = "a".repeat(108);
    assert_eq!(unix_socket(&longest).unwrap(), longest);
    let too_long = format!("/{longest}");
    assert_eq!(
        unix_socket(&too_long).unwrap_err(),
        format!("unix socket path \"{too_long}\" is too long")
    );
    assert_eq!(unix_socket("relative.sock").unwrap(), "relative.sock");
    assert_eq!(
        unix_socket("/var/run/docker.sock").unwrap(),
        "/var/run/docker.sock"
    );
}

/// Go's `path.Join` and the URL escaping of paths and hosts.
#[test]
fn paths_join_and_escape_as_go() {
    assert_eq!(join_path(&["", "/v1.56", "/nodes"]), "/v1.56/nodes");
    assert_eq!(
        join_path(&["/base/", "/v1.56", "/nodes"]),
        "/base/v1.56/nodes"
    );
    assert_eq!(join_path(&["", "/_ping"]), "/_ping");
    assert_eq!(join_path(&["/a/./b/../c", "/d/"]), "/a/c/d");
    assert_eq!(escape_path("/base path/v1.56"), "/base%20path/v1.56");
    assert_eq!(
        escape_host("/var/run/docker.sock"),
        "%2Fvar%2Frun%2Fdocker.sock"
    );
    assert_eq!(escape_host("127.0.0.1:2375"), "127.0.0.1:2375");
}
