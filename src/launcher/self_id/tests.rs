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

//! `TestParseSelfID` of `spec_test.go`, and the scanner's limits.

use std::io;

use super::{MAX_SCAN_TOKEN_SIZE, SelfIdError, parse_self_id};
use crate::launcher::fake::{TEST_DAEMON_ID, TEST_LAUNCHER_ID};

fn mount(id: &str, root_prefix: &str, file: &str, target: &str) -> String {
    format!("1581 1570 8:2 {root_prefix}/containers/{id}/{file} {target} rw - ext4 /dev/sda2 rw\n")
}

/// Go: `TestParseSelfID`.
#[test]
fn parse_self_id_cases() {
    let docker_root = format!(
        "1570 1511 0:420 / / rw,relatime - overlay root rw\n{}{}{}",
        mount(
            TEST_LAUNCHER_ID,
            "/var/lib/docker",
            "resolv.conf",
            "/etc/resolv.conf"
        ),
        mount(
            TEST_LAUNCHER_ID,
            "/var/lib/docker",
            "hostname",
            "/etc/hostname"
        ),
        mount(TEST_LAUNCHER_ID, "/var/lib/docker", "hosts", "/etc/hosts"),
    );
    let true_nas = format!(
        "901 800 0:61 /ix-apps/docker/containers/{TEST_LAUNCHER_ID}/hostname /etc/hostname \
         rw,relatime - zfs boot-pool/.ix-apps rw\n"
    );
    let separate_fs = mount(TEST_LAUNCHER_ID, "/docker", "hosts", "/etc/hosts");
    let no_match = "1570 1511 0:420 / / rw,relatime - overlay root rw\n\
                    1581 1570 8:2 /etc/resolv.conf /etc/resolv.conf rw - ext4 /dev/sda2 rw\n"
        .to_owned();
    // Another container's file mounted elsewhere says nothing about us.
    let other_target = mount(
        TEST_DAEMON_ID,
        "/var/lib/docker",
        "hostname",
        "/mnt/hostname",
    );
    let ambiguous = format!(
        "{}{}",
        mount(
            TEST_LAUNCHER_ID,
            "/var/lib/docker",
            "hostname",
            "/etc/hostname"
        ),
        mount(TEST_DAEMON_ID, "/var/lib/docker", "hosts", "/etc/hosts"),
    );

    let cases = [
        ("docker data-root", docker_root, Ok(TEST_LAUNCHER_ID)),
        ("truenas data-root", true_nas, Ok(TEST_LAUNCHER_ID)),
        (
            "data-root on its own filesystem",
            separate_fs,
            Ok(TEST_LAUNCHER_ID),
        ),
        ("no match", no_match, Err("not found")),
        ("other target", other_target, Err("not found")),
        ("ambiguous", ambiguous, Err("ambiguous")),
    ];
    for (name, table, want) in cases {
        let got = parse_self_id(table.as_bytes());
        match (want, got) {
            (Ok(want), Ok(got)) => assert_eq!(got, want, "{name}"),
            (Err("not found"), Err(SelfIdError::NotFound))
            | (Err("ambiguous"), Err(SelfIdError::Ambiguous)) => {}
            (want, got) => panic!("{name}: got {got:?}, want {want:?}"),
        }
    }
}

/// Only a 64-character lower-case hex ID under a `containers` directory names a container, as
/// Go's `containerFileRoot` pattern matches it.
#[test]
fn parse_self_id_needs_the_container_file_root() {
    let upper = TEST_LAUNCHER_ID.replace('1', "A");
    let roots = [
        format!("containers/{TEST_LAUNCHER_ID}/hostname"),
        format!("/containers/{}/hostname", &TEST_LAUNCHER_ID[1..]),
        format!("/containers/{upper}/hostname"),
        format!("/container/{TEST_LAUNCHER_ID}/hostname"),
        format!("/containers/{TEST_LAUNCHER_ID}/hostname.bak"),
    ];
    for root in roots {
        let table = format!("1 2 8:2 {root} /etc/hostname rw - ext4 /dev/sda2 rw\n");
        assert!(
            matches!(parse_self_id(table.as_bytes()), Err(SelfIdError::NotFound)),
            "{root}"
        );
    }
    let crlf = format!("1 2 8:2 /containers/{TEST_LAUNCHER_ID}/hosts /etc/hosts\r\n");
    assert_eq!(parse_self_id(crlf.as_bytes()).unwrap(), TEST_LAUNCHER_ID);
}

/// A line that does not fit `bufio.Scanner`'s 64 KiB buffer is Go's `token too long`, and one
/// that just fits is read.
#[test]
fn parse_self_id_scanner_limit() {
    let fits = format!("{}\n", "x".repeat(MAX_SCAN_TOKEN_SIZE - 1));
    assert!(matches!(
        parse_self_id(fits.as_bytes()),
        Err(SelfIdError::NotFound)
    ));
    for long in [
        format!("{}\n", "x".repeat(MAX_SCAN_TOKEN_SIZE)),
        "x".repeat(MAX_SCAN_TOKEN_SIZE),
    ] {
        let error = parse_self_id(long.as_bytes()).unwrap_err();
        assert_eq!(
            error.to_string(),
            "read mount table: bufio.Scanner: token too long"
        );
    }
}

/// The open error names the path twice, as Go's wrapped `PathError` does.
#[test]
fn open_error_reads_as_go() {
    let error = SelfIdError::Open(io::Error::from_raw_os_error(2));
    assert_eq!(
        error.to_string(),
        "open /proc/self/mountinfo: open /proc/self/mountinfo: no such file or directory"
    );
}
