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

//! Go's `api_test.go`. Not ported: `TestNew_InvalidVersion`, `TestNew_InvalidVersionIncludesValue`
//! and `TestNew_V1RequiresLedger`, because [`CgroupVersion`] has only the two versions and
//! [`super::super::CgroupV1::new`] takes its ledger, so neither failure can be expressed;
//! `TestNew_ValidVersions` waits for the cgroup v2 controller, which `New` also builds.

use super::*;
use crate::test_support::testdata;

/// Go: `TestDeviceRule_JSON_RoundTrip`.
#[test]
fn device_rule_json_round_trips_with_go_s_shape() {
    let cases = [
        (
            DeviceRule {
                allow: true,
                kind: "c".to_owned(),
                major: Some(254),
                minor: Some(0),
                access: "rwm".to_owned(),
            },
            r#"{"allow":true,"type":"c","major":254,"minor":0,"access":"rwm"}"#,
        ),
        (
            DeviceRule {
                allow: false,
                kind: "b".to_owned(),
                major: Some(254),
                minor: None,
                access: String::new(),
            },
            r#"{"allow":false,"type":"b","major":254}"#,
        ),
        (
            DeviceRule {
                allow: true,
                ..DeviceRule::default()
            },
            r#"{"allow":true}"#,
        ),
    ];
    for (rule, want) in cases {
        let marshaled = serde_json::to_string(&rule).unwrap();
        assert_eq!(marshaled, want);
        let unmarshaled: DeviceRule = serde_json::from_str(&marshaled).unwrap();
        assert_eq!(unmarshaled, rule);
    }
}

/// Go's `%+v` of the rule, as a scratch Go program printed it, but for the address of a set
/// number (Q7).
#[test]
fn rules_print_as_go_s_struct_format() {
    let rule = DeviceRule {
        allow: true,
        kind: "c".to_owned(),
        ..DeviceRule::default()
    };
    assert_eq!(
        rule.go_struct_text(),
        "{Allow:true Type:c Major:<nil> Minor:<nil> Access:}"
    );
    let rule = DeviceRule {
        allow: true,
        kind: "a".to_owned(),
        major: Some(10),
        minor: Some(200),
        access: "r".to_owned(),
    };
    assert_eq!(
        rule.go_struct_text(),
        "{Allow:true Type:a Major:10 Minor:200 Access:r}"
    );
}

/// Go: `TestScanCGroupVersion`; a hybrid host has both entries, and `devices` wins.
#[test]
fn the_cgroup_file_names_the_version() {
    let cases = [
        ("cgroup_v1.txt", Ok(CgroupVersion::V1)),
        ("cgroup_v2.txt", Ok(CgroupVersion::V2)),
        ("cgroup_hybrid.txt", Ok(CgroupVersion::V1)),
        (
            "cgroup_no_match.txt",
            Err("no devices or unified cgroup entries found"),
        ),
        (
            "cgroup_malformed.txt",
            Err("malformed cgroup entry: this-line-has-no-colons"),
        ),
    ];
    for (fixture, want) in cases {
        let got = scan_cgroup_version(&testdata(&format!("cgroup/{fixture}")), fixture);
        assert_eq!(
            got.map_err(|error| error.to_string()),
            want.map_err(str::to_owned),
            "{fixture}"
        );
    }
    assert_eq!(CgroupVersion::V1.number(), 1);
    assert_eq!(CgroupVersion::V2.number(), 2);
}

/// `ParseProcCgroup` on both versions and a hybrid host, with the mount prefix stripped from the
/// cgroup path.
#[test]
fn proc_cgroup_resolves_both_versions() {
    let parse = |cgroup: &str, mountinfo: &str| {
        parse_proc_cgroup(
            &testdata(&format!("cgroup/{cgroup}")),
            &testdata(&format!("cgroup/{mountinfo}")),
        )
    };
    assert_eq!(
        parse("cgroup_v1.txt", "mountinfo_v1.txt"),
        Ok(ProcCgroup {
            version: CgroupVersion::V1,
            mount_prefix: "/".to_owned(),
            mount_point: "/sys/fs/cgroup/devices".to_owned(),
            root: "/docker/abc123def456".to_owned(),
        })
    );
    // The prefix is stripped as a string, as Go's strings.TrimPrefix does, not as a path.
    assert_eq!(
        parse("cgroup_v1.txt", "mountinfo_v1_prefix.txt").map(|parsed| parsed.root),
        Ok("def456".to_owned())
    );
    assert_eq!(
        parse("cgroup_v2.txt", "mountinfo_v2.txt"),
        Ok(ProcCgroup {
            version: CgroupVersion::V2,
            mount_prefix: "/".to_owned(),
            mount_point: "/sys/fs/cgroup".to_owned(),
            root: "/docker/abc123def456".to_owned(),
        })
    );
    assert_eq!(
        parse("cgroup_hybrid.txt", "mountinfo_v1.txt").map(|parsed| parsed.root),
        Ok("/user.slice".to_owned())
    );
    assert_eq!(
        parse("cgroup_v2.txt", "mountinfo_v1.txt").map_err(|error| error.to_string()),
        Err("no cgroup2 filesystem in mountinfo file".to_owned())
    );
}

/// The prefix is stripped as Go's `strings.TrimPrefix` does, a `/..` prefix is refused, and a
/// line too long for Go's scanner fails the file it is in.
#[test]
fn proc_cgroup_errors_read_as_go() {
    let mountinfo = |prefix: &str| {
        format!(
            "43 35 0:31 {prefix} /sys/fs/cgroup/devices rw shared:19 - cgroup cgroup rw,devices\n"
        )
    };
    let cgroup = b"11:devices:/docker/abc\n";
    assert_eq!(
        parse_proc_cgroup(cgroup, mountinfo("/docker").as_bytes()).map(|parsed| parsed.root),
        Ok("/abc".to_owned())
    );
    assert_eq!(
        parse_proc_cgroup(cgroup, mountinfo("/other").as_bytes()).map(|parsed| parsed.root),
        Ok("/docker/abc".to_owned())
    );
    assert_eq!(
        parse_proc_cgroup(cgroup, mountinfo("/../escape").as_bytes())
            .map_err(|error| error.to_string()),
        Err("relative path in mount prefix: /../escape".to_owned())
    );
    let long = format!("11:devices:/{}\n", "a".repeat(70_000));
    assert_eq!(
        parse_proc_cgroup(long.as_bytes(), mountinfo("/").as_bytes())
            .map_err(|error| error.to_string()),
        Err(r#"read "cgroup": bufio.Scanner: token too long"#.to_owned())
    );
}
