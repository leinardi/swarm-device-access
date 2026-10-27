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

// Adapted from swarm-scheduler-exporter/src/go_compat/host_port/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Go's `TestSplitHostPort` and `TestSplitHostPortErrors` cases, from the net package's tests.

use super::*;

/// The well-formed cases of Go's `splitJoinTests`.
#[test]
fn splits_as_go() {
    for (host_port, host, port) in [
        ("www.google.com:80", "www.google.com", "80"),
        ("127.0.0.1:1234", "127.0.0.1", "1234"),
        ("[::1]:80", "::1", "80"),
        ("[fe80::1%lo0]:80", "fe80::1%lo0", "80"),
        ("localhost%lo0:80", "localhost%lo0", "80"),
        (":8888", "", "8888"),
        ("[::]:8888", "::", "8888"),
        ("host:", "host", ""),
        (":", "", ""),
        ("[]:", "", ""),
    ] {
        assert_eq!(split_host_port(host_port), Ok((host, port)), "{host_port}");
    }
}

/// Go's `splitFailureTests`, with their messages.
#[test]
fn fails_as_go() {
    for (host_port, message) in [
        ("www.google.com", "missing port in address"),
        ("127.0.0.1", "missing port in address"),
        ("[::1]", "missing port in address"),
        ("[fe80::1%lo0]", "missing port in address"),
        ("[localhost%lo0]", "missing port in address"),
        ("localhost%lo0", "missing port in address"),
        ("::1", "too many colons in address"),
        ("fe80::1%lo0", "too many colons in address"),
        ("fe80::1%lo0:80", "too many colons in address"),
        ("[foo:bar]", "missing port in address"),
        ("[foo:bar]baz", "missing port in address"),
        ("[foo]bar:baz", "missing port in address"),
        ("[foo]:[bar]:baz", "too many colons in address"),
        ("[foo]:[bar]baz", "unexpected '[' in address"),
        ("foo[bar]:baz", "unexpected '[' in address"),
        ("foo]bar:baz", "unexpected ']' in address"),
        ("[::1:80", "missing ']' in address"),
    ] {
        let error = split_host_port(host_port).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("address {host_port}: {message}"),
            "{host_port}"
        );
    }
    assert_eq!(
        split_host_port("").unwrap_err().to_string(),
        "missing port in address"
    );
}

#[test]
fn joins_as_go() {
    assert_eq!(join_host_port("127.0.0.1", "80"), "127.0.0.1:80");
    assert_eq!(join_host_port("::1", "80"), "[::1]:80");
    assert_eq!(join_host_port("", "80"), ":80");
}
