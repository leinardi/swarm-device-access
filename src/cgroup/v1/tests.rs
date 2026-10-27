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

//! Go's `v1_test.go`.

use super::*;
use crate::test_support::testdata;

/// Go: `TestScanMountInfoV1`; a v2-only mountinfo does not match.
#[test]
fn the_devices_mount_is_found_in_mountinfo() {
    let cases = [
        ("mountinfo_v1.txt", Ok(("/", "/sys/fs/cgroup/devices"))),
        (
            "mountinfo_v1_prefix.txt",
            Ok(("/docker/abc123", "/sys/fs/cgroup/devices")),
        ),
        (
            "mountinfo_no_match.txt",
            Err("no cgroup filesystem mounted for the devices subsytem in mountinfo file"),
        ),
        (
            "mountinfo_malformed.txt",
            Err("malformed mountinfo entry: tooshort"),
        ),
        (
            "mountinfo_v2.txt",
            Err("no cgroup filesystem mounted for the devices subsytem in mountinfo file"),
        ),
    ];
    for (fixture, want) in cases {
        let got = scan_mount_info_v1(&testdata(&format!("cgroup/{fixture}")), "mountinfo");
        assert_eq!(
            got.map_err(|error| error.to_string()),
            want.map(|(prefix, point)| (prefix.to_owned(), point.to_owned()))
                .map_err(str::to_owned),
            "{fixture}"
        );
    }
}

/// Only a mount whose point's last element is `devices` counts, a trailing slash included, and
/// only a `cgroup` filesystem: the type is the third field from the end.
#[test]
fn the_devices_mount_is_named_devices_and_typed_cgroup() {
    let line = |point: &str, kind: &str| {
        format!("43 35 0:31 / {point} rw shared:19 - {kind} cgroup rw,devices\n")
    };
    let scan = |text: String| scan_mount_info_v1(text.as_bytes(), "mountinfo");
    assert_eq!(
        scan(line("/sys/fs/cgroup/devices/", "cgroup")),
        Ok(("/".to_owned(), "/sys/fs/cgroup/devices/".to_owned()))
    );
    assert_eq!(
        scan(line("/sys/fs/cgroup/cpu,devices", "cgroup")),
        Err(ParseError::NoCgroupV1Fs)
    );
    assert_eq!(
        scan(line("/sys/fs/cgroup/devices", "tmpfs")),
        Err(ParseError::NoCgroupV1Fs)
    );
    // Two spaces make an empty field, which Go's strings.Split keeps: the third field from the
    // end is then empty, not the type.
    assert_eq!(
        scan("43 35 0:31 / /sys/fs/cgroup/devices rw - cgroup  cgroup rw\n".to_owned()),
        Err(ParseError::NoCgroupV1Fs)
    );
}

/// Go: `TestScanProcCgroupV1`.
#[test]
fn the_devices_cgroup_is_read_with_the_prefix_stripped() {
    let cases = [
        ("cgroup_v1.txt", "/", Ok("/docker/abc123def456")),
        ("cgroup_v1.txt", "/docker", Ok("/abc123def456")),
        ("cgroup_v2.txt", "/", Err("no devices cgroup entries found")),
        (
            "cgroup_no_match.txt",
            "/",
            Err("no devices cgroup entries found"),
        ),
        (
            "cgroup_malformed.txt",
            "/",
            Err("malformed cgroup entry: this-line-has-no-colons"),
        ),
    ];
    for (fixture, prefix, want) in cases {
        let got = scan_proc_cgroup_v1(&testdata(&format!("cgroup/{fixture}")), "cgroup", prefix);
        assert_eq!(
            got.map_err(|error| error.to_string()),
            want.map(str::to_owned).map_err(str::to_owned),
            "{fixture} {prefix}"
        );
    }
}
