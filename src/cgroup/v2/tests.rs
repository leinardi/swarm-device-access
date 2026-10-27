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

//! Go's `v2_test.go`: the parsers. The device filter's tests come with it.

use super::*;
use crate::test_support::testdata;

/// Go: `TestScanMountInfoV2`; a v1-only mountinfo does not match.
#[test]
fn the_unified_mount_is_found_in_mountinfo() {
    let cases = [
        ("mountinfo_v2.txt", Ok(("/", "/sys/fs/cgroup"))),
        (
            "mountinfo_no_match.txt",
            Err("no cgroup2 filesystem in mountinfo file"),
        ),
        (
            "mountinfo_malformed.txt",
            Err("malformed mountinfo entry: tooshort"),
        ),
        (
            "mountinfo_v1.txt",
            Err("no cgroup2 filesystem in mountinfo file"),
        ),
    ];
    for (fixture, want) in cases {
        let got = scan_mount_info_v2(&testdata(&format!("cgroup/{fixture}")), "mountinfo");
        assert_eq!(
            got.map_err(|error| error.to_string()),
            want.map(|(prefix, point)| (prefix.to_owned(), point.to_owned()))
                .map_err(str::to_owned),
            "{fixture}"
        );
    }
}

/// Go: `TestScanProcCgroupV2`.
#[test]
fn the_unified_cgroup_is_read_with_the_prefix_stripped() {
    let cases = [
        ("cgroup_v2.txt", "/", Ok("/docker/abc123def456")),
        ("cgroup_v2.txt", "/docker", Ok("/abc123def456")),
        ("cgroup_v1.txt", "/", Err("no cgroupv2 entries in file")),
        (
            "cgroup_no_match.txt",
            "/",
            Err("no cgroupv2 entries in file"),
        ),
        (
            "cgroup_malformed.txt",
            "/",
            Err("malformed cgroup entry: this-line-has-no-colons"),
        ),
    ];
    for (fixture, prefix, want) in cases {
        let got = scan_proc_cgroup_v2(&testdata(&format!("cgroup/{fixture}")), "cgroup", prefix);
        assert_eq!(
            got.map_err(|error| error.to_string()),
            want.map(str::to_owned).map_err(str::to_owned),
            "{fixture} {prefix}"
        );
    }
}
