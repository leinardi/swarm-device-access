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

//! The guards and the program checks of the enforcement tests, each with its negative control.
//! The programs are the real runtime filters under `tests/testdata/devfilter` (runc's and
//! systemd's), and `runc.wrapped` is the kernel's dump of the Go daemon's wrapper around runc's.

use swarm_device_access::cgroup::{DeviceRule, emit_owned};

use super::*;

/// The nonce `runc.wrapped` was produced with (`src/cgroup/testing.rs`).
const GO_FIXTURE_NONCE: u64 = 0x0123_4567_89ab_cdef;

fn fixture(name: &str) -> Vec<u8> {
    let path = format!(
        "{}/../tests/testdata/devfilter/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let text = std::fs::read_to_string(&path).expect("read the fixture");
    unhex(&text.split_whitespace().collect::<String>()).expect("a hex fixture")
}

fn rule(kind: &str, major: i64, minor: i64, access: &str) -> DeviceRule {
    DeviceRule {
        allow: true,
        kind: kind.to_owned(),
        major: Some(major),
        minor: Some(minor),
        access: access.to_owned(),
    }
}

fn grant() -> Vec<DeviceRule> {
    vec![rule("c", 10, 237, "rwm")]
}

fn wrap(rules: &[DeviceRule], nonce: u64, original: &[u8]) -> Vec<u8> {
    emit_owned(rules, nonce, original).expect("emit a wrapper")
}

fn dump(programs: Vec<Vec<u8>>) -> ProgramDump {
    ProgramDump {
        cgroup: "/sys/fs/cgroup/system.slice/docker-1.scope".to_owned(),
        inode: 42,
        total: programs.len(),
        inaccessible: 0,
        attach_flags: 2,
        programs,
    }
}

#[test]
fn only_one_turns_a_guard_on() {
    assert!(flag_set(Some("1")));
    for off in [
        None,
        Some(""),
        Some("0"),
        Some("true"),
        Some("1 "),
        Some("yes"),
    ] {
        assert!(!flag_set(off), "{off:?} turned a guard on");
    }
}

/// Go's `requireOrSkip`: strict fails, otherwise skips, with Go's texts.
#[test]
fn a_missing_prerequisite_fails_only_when_strict() {
    assert_eq!(
        missing_prerequisite(true, "cgroup v2: not found"),
        Verdict::Fail(
            "missing prerequisite (SDA_IT_ENFORCE=1 makes this a failure): cgroup v2: not found"
                .to_owned()
        )
    );
    assert_eq!(
        missing_prerequisite(false, "cgroup v2: not found"),
        Verdict::Skip("missing prerequisite: cgroup v2: not found".to_owned())
    );
}

/// Go's `reloadUnsupported`: required fails, otherwise skips, with Go's texts.
#[test]
fn an_unsupported_reload_fails_only_when_required() {
    assert_eq!(
        reload_unsupported(true, "no wipe"),
        Verdict::Fail("reload scenario required by SDA_IT_REQUIRE_RELOAD=1: no wipe".to_owned())
    );
    assert_eq!(
        reload_unsupported(false, "no wipe"),
        Verdict::Skip(
            "reload scenario not supported on this host (set SDA_IT_REQUIRE_RELOAD=1 to fail \
             instead): no wipe"
                .to_owned()
        )
    );
}

#[test]
fn the_go_image_has_no_default() {
    assert_eq!(go_test_image(None), None);
    assert_eq!(go_test_image(Some("")), None);
    assert_eq!(go_test_image(Some("sda:go")).as_deref(), Some("sda:go"));
}

/// Each build gets its own spelling: Go's single dash, which the Rust daemon refuses, and Rust's
/// double dash.
#[test]
fn each_build_spells_its_flags() {
    assert_eq!(
        Build::Go.daemon_args(&["device-deny=/dev/loop0", "dry-run"]),
        [
            "-policy-mode=opt-in",
            "-log-format=json",
            "-log-level=debug",
            "-device-deny=/dev/loop0",
            "-dry-run"
        ]
    );
    assert_eq!(
        Build::Rust.daemon_args(&[]),
        [
            "--policy-mode=opt-in",
            "--log-format=json",
            "--log-level=debug"
        ]
    );
}

#[test]
fn a_dump_round_trips() {
    let original = dump(vec![
        fixture("runc.orig"),
        fixture("systemd.orig"),
        Vec::new(),
    ]);
    assert_eq!(ProgramDump::parse(&original.render()), Ok(original));
}

#[test]
fn a_malformed_dump_is_refused() {
    let good = dump(vec![fixture("runc.orig")]).render();
    let cases = [
        ("no cgroup", good.replacen("cgroup ", "group ", 1)),
        ("no inode", good.replacen("inode 42\n", "", 1)),
        ("two totals", format!("{good}total 1\n")),
        ("two cgroups", format!("{good}cgroup /x\n")),
        ("a bad number", good.replacen("total 1", "total -1", 1)),
        ("odd hex", format!("{good}program abc\n")),
        ("not hex", format!("{good}program +f\n")),
        ("a bare word", format!("{good}program\n")),
    ];
    for (name, text) in cases {
        assert!(ProgramDump::parse(&text).is_err(), "{name} was accepted");
    }
}

/// The daemon's classification: a runtime filter is bare, the Go daemon's wrapper is owned with
/// its nonce and rules and runc's filter inside, and a wrapper around it is nested.
#[test]
fn programs_are_classified_as_the_daemon_does() {
    let runc = fixture("runc.orig");
    assert_eq!(classify(&runc), Ok(Classified::Bare));
    let go_wrapper = fixture("runc.wrapped");
    let Ok(Classified::Wrapper {
        nonce,
        original,
        nested,
        rules,
    }) = classify(&go_wrapper)
    else {
        panic!("the Go wrapper is not owned");
    };
    assert_eq!((nonce, &original, nested), (GO_FIXTURE_NONCE, &runc, false));
    assert_eq!(rules, [rule("c", 10, 200, "rwm"), rule("b", 8, 0, "r")]);
    let Ok(Classified::Wrapper {
        original, nested, ..
    }) = classify(&wrap(&grant(), 5, &go_wrapper))
    else {
        panic!("the nested wrapper is not owned");
    };
    assert_eq!((original, nested), (runc, true));

    let mut conflict = go_wrapper;
    let last = conflict.len() - 1;
    conflict[last] ^= 1;
    assert!(
        classify(&conflict).is_err(),
        "a corrupted wrapper validated"
    );
}

#[test]
fn the_runtime_originals_are_the_bare_baseline() {
    let originals = vec![fixture("runc.orig"), fixture("systemd.orig")];
    assert_eq!(runtime_originals(&dump(originals.clone())), Ok(originals));

    assert!(
        runtime_originals(&dump(Vec::new())).is_err(),
        "nothing attached"
    );
    assert!(
        runtime_originals(&dump(vec![fixture("runc.wrapped")])).is_err(),
        "a wrapper before any daemon"
    );
    let mut hidden = dump(vec![fixture("runc.orig")]);
    hidden.total = 2;
    hidden.inaccessible = 1;
    assert!(
        runtime_originals(&hidden).is_err(),
        "an inaccessible program"
    );
}

/// After a grant: one wrapper per original, whichever daemon emitted it, passes.
#[test]
fn a_grant_with_one_wrapper_per_original_passes() {
    let runc = fixture("runc.orig");
    let systemd = fixture("systemd.orig");
    assert_eq!(
        granted_problems(
            std::slice::from_ref(&runc),
            &dump(vec![fixture("runc.wrapped")])
        ),
        Vec::<String>::new()
    );
    let originals = vec![runc.clone(), systemd.clone()];
    assert_eq!(
        granted_problems(
            &originals,
            &dump(vec![wrap(&grant(), 2, &systemd), wrap(&grant(), 1, &runc)])
        ),
        Vec::<String>::new()
    );
    // Two identical runtime filters, wrapped separately: one wrapper each.
    assert_eq!(
        granted_problems(
            &[runc.clone(), runc.clone()],
            &dump(vec![wrap(&grant(), 1, &runc), wrap(&grant(), 2, &runc)])
        ),
        Vec::<String>::new()
    );
}

/// The negative controls of the grant check, one problem each.
#[test]
fn a_grant_with_a_stale_or_missing_wrapper_fails() {
    let runc = fixture("runc.orig");
    let systemd = fixture("systemd.orig");
    let originals = vec![runc.clone(), systemd.clone()];
    let old_rules = vec![rule("c", 10, 200, "rwm")];
    let mut hidden = dump(vec![wrap(&grant(), 1, &runc), wrap(&grant(), 2, &systemd)]);
    hidden.total = 3;
    hidden.inaccessible = 1;
    let cases = [
        (
            "an original left bare",
            dump(vec![wrap(&grant(), 1, &runc), systemd.clone()]),
        ),
        (
            "a stale wrapper with older rules beside the new one",
            dump(vec![
                wrap(&grant(), 1, &runc),
                wrap(&grant(), 2, &systemd),
                wrap(&old_rules, 3, &runc),
            ]),
        ),
        (
            "wrappers with different rules",
            dump(vec![
                wrap(&grant(), 1, &runc),
                wrap(&old_rules, 2, &systemd),
            ]),
        ),
        (
            "two wrappers of one lineage",
            dump(vec![wrap(&grant(), 1, &runc), wrap(&grant(), 1, &systemd)]),
        ),
        (
            "a nested wrapper",
            dump(vec![
                wrap(&grant(), 1, &wrap(&grant(), 4, &runc)),
                wrap(&grant(), 2, &systemd),
            ]),
        ),
        (
            "an original not wrapped at all",
            dump(vec![wrap(&grant(), 1, &runc)]),
        ),
        (
            "a wrapper of a foreign original",
            dump(vec![
                wrap(&grant(), 1, &runc),
                wrap(&grant(), 2, &fixture("crun.orig")),
            ]),
        ),
        ("an inaccessible program", hidden),
    ];
    for (name, programs) in cases {
        let problems = granted_problems(&originals, &programs);
        assert!(!problems.is_empty(), "{name} passed");
    }
}

/// After a revocation: the originals bare, once each, in any order, pass.
#[test]
fn a_revocation_back_to_the_bare_originals_passes() {
    let runc = fixture("runc.orig");
    let systemd = fixture("systemd.orig");
    assert_eq!(
        revoked_problems(
            &[runc.clone(), systemd.clone()],
            &dump(vec![systemd.clone(), runc.clone()])
        ),
        Vec::<String>::new()
    );
}

/// The negative controls of the revocation check, as `plan_restore` must never leave the cgroup.
#[test]
fn a_revocation_that_leaves_a_wrapper_or_a_wrong_count_fails() {
    let runc = fixture("runc.orig");
    let systemd = fixture("systemd.orig");
    let originals = vec![runc.clone(), systemd.clone()];
    let cases = [
        (
            "the Go wrapper left in place",
            dump(vec![fixture("runc.wrapped"), systemd.clone()]),
        ),
        (
            "the original beside its wrapper",
            dump(vec![
                runc.clone(),
                systemd.clone(),
                wrap(&grant(), 1, &runc),
            ]),
        ),
        (
            "an original attached twice",
            dump(vec![runc.clone(), runc.clone(), systemd.clone()]),
        ),
        ("an original missing", dump(vec![runc.clone()])),
        ("nothing attached", dump(Vec::new())),
        (
            "a foreign program",
            dump(vec![runc, systemd, fixture("crun.orig")]),
        ),
    ];
    for (name, programs) in cases {
        let problems = revoked_problems(&originals, &programs);
        assert!(!problems.is_empty(), "{name} passed");
    }
}

#[test]
fn the_cgroup_must_survive() {
    let baseline = dump(Vec::new());
    assert_eq!(cgroup_problem(&baseline, &baseline), None);
    let mut moved = baseline.clone();
    moved.cgroup.push('x');
    assert!(cgroup_problem(&baseline, &moved).is_some());
    let mut recreated = baseline.clone();
    recreated.inode += 1;
    assert!(cgroup_problem(&baseline, &recreated).is_some());
}
