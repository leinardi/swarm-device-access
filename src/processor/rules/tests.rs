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

//! Go's `rules_test.go`, against its `fakeDevFS` (`fake_devfs.rs`). The tests that drive
//! `Processor.computeDesired` are ported here in their collection halves; their `computeDesired`
//! halves, and `TestProcessor_DevFSOpenFailureEmptiesTheSet` and
//! `TestProcessor_UnresolvedCandidateEmptiesTheSet`, which test only `computeDesired`, are with
//! the reconcile tests.

use std::collections::{HashMap, HashSet};

use rustix::io::Errno;

use super::*;
use crate::logger::Level;
use crate::policy::{LABEL_DEVICE_ALLOW, LABEL_DEVICE_DENY};
use crate::processor::devfs::{RealDevFs, probe_openat2};
use crate::processor::fake_devfs::{
    DEV_CARD, DEV_NULL, DEV_REND, DEV_SDA, FakeDevFs, FakeNode, char_dev,
};
use crate::test_support::capture_logs;

fn globs(list: &[&str]) -> Option<Vec<String>> {
    (!list.is_empty()).then(|| list.iter().map(|glob| (*glob).to_owned()).collect())
}

/// Go's `policy.Global{Mode: policy.ModeAll, DeviceAllow: allow, DeviceDeny: deny}`, an empty
/// list standing for Go's nil.
fn global(allow: &[&str], deny: &[&str]) -> Global {
    Global::new("all", globs(allow), globs(deny)).unwrap()
}

fn all() -> Global {
    global(&[], &[])
}

/// A container policy from its labels (Go's `policy.Container{DeviceAllow: ..., DeviceDeny:
/// ...}`).
fn container(allow: &[&str], deny: &[&str]) -> Container {
    let mut labels = HashMap::new();
    if !allow.is_empty() {
        labels.insert(LABEL_DEVICE_ALLOW.to_owned(), allow.join(","));
    }
    if !deny.is_empty() {
        labels.insert(LABEL_DEVICE_DENY.to_owned(), deny.join(","));
    }
    Container::parse(&labels).unwrap()
}

fn bind(source: &str) -> MountPoint {
    MountPoint {
        kind: BIND_MOUNT.to_owned(),
        source: source.to_owned(),
        destination: source.to_owned(),
    }
}

fn collect_mounts(
    dev: &dyn DevFs,
    global: &Global,
    mounts: &[MountPoint],
    log: &LogHandle,
) -> ContainerRules {
    let container = Container::default();
    let judge = Judge {
        global,
        container: &container,
        log,
    };
    collect_container_rules(dev, "abc", 1, mounts, judge, &Recorder::noop())
}

/// Go's `collectFrom`, logging to `log`.
fn collect_logged(
    dev: &dyn DevFs,
    global: &Global,
    log: &LogHandle,
    sources: &[&str],
) -> ContainerRules {
    let mounts: Vec<MountPoint> = sources.iter().map(|source| bind(source)).collect();
    collect_mounts(dev, global, &mounts, log)
}

/// Go's `collectFrom`.
fn collect_from(dev: &dyn DevFs, global: &Global, sources: &[&str]) -> ContainerRules {
    collect_logged(dev, global, &capture_logs(Level::DEBUG).0, sources)
}

fn rule_key(kind: &str, major: i64, minor: i64) -> String {
    format!("{kind} {major}:{minor}")
}

/// Go's `wantRules`: no errors, and exactly the listed devices granted.
#[track_caller]
fn want_rules(got: &ContainerRules, want: &[DeviceId]) {
    assert!(
        got.device_errs.is_empty(),
        "unexpected errors: {:?}",
        got.device_errs
    );
    let set: HashSet<String> = got
        .granted
        .iter()
        .map(|rule| rule_key(&rule.kind, rule.major.unwrap(), rule.minor.unwrap()))
        .collect();
    let wanted: HashSet<String> = want
        .iter()
        .map(|id| rule_key(id.kind.letter(), id.major.into(), id.minor.into()))
        .collect();
    assert_eq!(set, wanted);
    assert_eq!(got.granted.len(), want.len());
    for rule in &got.granted {
        assert!(
            rule.allow && rule.access == "rwm",
            "rule has unexpected allow/access: {rule:?}"
        );
    }
}

/// Go's `wantIncomplete`: an error of the `sentinel` class, naming every fragment.
#[track_caller]
fn want_incomplete(got: &ContainerRules, sentinel: fn(&DeviceError) -> bool, fragments: &[&str]) {
    assert!(!got.device_errs.is_empty(), "want an error, got none");
    assert!(
        got.device_errs.iter().any(sentinel),
        "errors {:?} are not of the wanted class",
        got.device_errs
    );
    let joined = got
        .device_errs
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    for fragment in fragments {
        assert!(
            joined.contains(fragment),
            "err = {joined}, want {fragment:?}"
        );
    }
}

/// Go's `wantUnresolved`.
#[track_caller]
fn want_unresolved(got: &ContainerRules, fragments: &[&str]) {
    want_incomplete(got, DeviceError::is_unresolved, fragments);
}

fn warn_lines(output: &str) -> Vec<&str> {
    output
        .lines()
        .filter(|line| line.contains("level=WARN"))
        .collect()
}

/// Go: `TestEvaluateIdentity`.
#[test]
fn evaluate_identity_judges_every_name() {
    const ALIAS: &str = "/dev/disk/by-id/usb-x";
    const RESOLVED: &str = "/dev/sda";
    const CANONICAL: &str = "/dev/sda";
    let none = Container::default();
    let granted = Decision {
        denied_by: None,
        authorized: true,
    };
    for (name, global_policy, container_policy, want) in [
        ("no policy", all(), none.clone(), granted.clone()),
        (
            "alias denied",
            global(&[], &["/dev/disk/by-id/*"]),
            none.clone(),
            Decision {
                denied_by: Some(ALIAS.to_owned()),
                authorized: false,
            },
        ),
        (
            "canonical denied by the container",
            all(),
            container(&[], &["/dev/sd*"]),
            Decision {
                denied_by: Some(RESOLVED.to_owned()),
                authorized: false,
            },
        ),
        (
            "alias allowed but not the device",
            global(&["/dev/disk/by-id/*"], &[]),
            none.clone(),
            Decision::default(),
        ),
        (
            "device allowed, alias not listed",
            global(&["/dev/sda"], &[]),
            none.clone(),
            granted.clone(),
        ),
        (
            "container allow list narrows",
            global(&["/dev/sd*"], &[]),
            container(&["/dev/sdb"], &[]),
            Decision::default(),
        ),
    ] {
        let got = evaluate_identity(
            ALIAS,
            RESOLVED,
            CANONICAL,
            &global_policy,
            &container_policy,
        );
        assert_eq!(got, want, "{name}");
    }

    // Authorized needs both the canonical and the resolved name.
    let got = evaluate_identity(
        "/dev/x",
        "/dev/dri/card0",
        "/dev/sda",
        &global(&["/dev/dri/*"], &[]),
        &none,
    );
    assert!(!got.authorized, "authorized on the resolved name alone");
}

/// Go: `TestCanonicalName`.
#[test]
fn canonical_name_needs_one_clean_relative_devname() {
    for (name, uevent, want) in [
        ("plain", "MAJOR=8\nDEVNAME=sda\nDEVTYPE=disk\n", "/dev/sda"),
        ("nested", "DEVNAME=dri/card0\n", "/dev/dri/card0"),
        ("missing", "MAJOR=8\n", ""),
        ("two lines", "DEVNAME=sda\nDEVNAME=sdb\n", ""),
        ("empty", "DEVNAME=\n", ""),
        ("absolute", "DEVNAME=/dev/sda\n", ""),
        ("dot-dot", "DEVNAME=../sda\n", ""),
        ("inner dot-dot", "DEVNAME=dri/../../sda\n", ""),
        ("unclean", "DEVNAME=dri//card0\n", ""),
        ("trailing slash", "DEVNAME=sda/\n", ""),
    ] {
        let mut dev = FakeDevFs::new();
        dev.uevents.insert(DEV_SDA, uevent.to_owned());
        let got = canonical_name(&dev, DEV_SDA);
        if want.is_empty() {
            assert!(
                matches!(got, Err(Cause::BadDevname(_))),
                "{name}: canonical_name = {got:?}, want a bad DEVNAME"
            );
        } else {
            assert_eq!(got, Ok(want.to_owned()), "{name}");
        }
    }
}

/// The texts of a bad `DEVNAME`, as Go's `%d DEVNAME lines` and `%q`.
#[test]
fn bad_devname_texts() {
    assert_eq!(
        Cause::BadDevname(BadDevname::Count(2)).to_string(),
        "uevent has no single valid DEVNAME: 2 DEVNAME lines"
    );
    assert_eq!(
        Cause::BadDevname(BadDevname::Invalid("../sda".to_owned())).to_string(),
        "uevent has no single valid DEVNAME: \"../sda\""
    );
}

/// The error texts of the collections that fail, each the output of a Go program running the
/// same collection.
#[test]
fn device_error_texts() {
    let texts = |got: &ContainerRules| -> Vec<String> {
        got.device_errs.iter().map(ToString::to_string).collect()
    };
    let mut dev = FakeDevFs::new();
    dev.abs_link("a", "/dev/b");
    dev.abs_link("b", "/dev/a");
    assert_eq!(
        texts(&collect_from(&dev, &all(), &["/dev/a"])),
        ["device identity could not be established: /dev/a: too many levels of symbolic links"]
    );

    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");
    dev.uevents.remove(&DEV_SDA);
    assert_eq!(
        texts(&collect_from(&dev, &all(), &["/dev/sda"])),
        [
            "device identity could not be established: /dev/sda (resolved /dev/sda, b 8:0): read \
             uevent b 8:0: no such file or directory"
        ]
    );
    assert_eq!(
        texts(&collect_from(&dev, &all(), &["/dev/ttyUSB0"])),
        [
            "device identity could not be established: mount source /dev/ttyUSB0: no such file \
             or directory"
        ]
    );
    dev.other("regular", NodeKind::Other, "/dev/regular");
    assert_eq!(
        texts(&collect_from(&dev, &all(), &["/dev/regular"])),
        ["device \"/dev/regular\" is neither character nor block device"]
    );
    dev.directory("dri");
    dev.directory("dri/by-path");
    dev.walk_errs
        .insert("dri/by-path".to_owned(), SysError::new("", Errno::ACCESS));
    assert_eq!(
        texts(&collect_from(&dev, &all(), &["/dev/dri"])),
        ["device mount enumeration incomplete: /dev/dri/by-path: permission denied"]
    );
    dev.directory("big");
    for index in 0..=MAX_MOUNT_ENTRIES {
        let minor = u32::try_from(index).unwrap();
        dev.device(&format!("big/n{index}"), char_dev(250, minor), "n");
    }
    assert_eq!(
        texts(&collect_from(&dev, &all(), &["/dev/big"])),
        [
            "device mount enumeration incomplete: /dev/big: mount too large; narrow the bind \
             mount: more than 4096 entries"
        ]
    );
}

/// Go: `TestCollect_PlantedNodeUnderShmJudgedAsSda`. A node planted under `/dev/shm` by anyone
/// who can write there is judged by its sysfs identity, not by where it was planted.
#[test]
fn collect_planted_node_under_shm_judged_as_sda() {
    let mut dev = FakeDevFs::new();
    dev.directory("shm");
    dev.device("shm/evil", DEV_SDA, "sda");

    want_rules(
        &collect_from(&dev, &global(&["/dev/shm/*"], &[]), &["/dev/shm"]),
        &[],
    );
    want_rules(
        &collect_from(&dev, &global(&[], &["/dev/sda"]), &["/dev/shm"]),
        &[],
    );
}

/// Go: `TestCollect_SymlinkOutsideDevRejected`.
#[test]
fn collect_symlink_outside_dev_rejected() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.abs_link("dri/x", "/tmp/x");
    dev.abs_link("dri/up", "/dev/../tmp/x");
    let (log, sink) = capture_logs(Level::DEBUG);

    let got = collect_logged(&dev, &all(), &log, &["/dev/dri"]);
    want_rules(&got, &[]);
    assert_eq!(got.skipped, 2);
    let output = sink.contents();
    assert_eq!(
        output.matches("device path resolves outside /dev").count(),
        2,
        "want one outside_dev record per link, got:\n{output}"
    );
    assert!(output.contains("reason=outside_dev"), "{output}");
    assert!(
        !output.contains("level=WARN"),
        "a link no allow glob names must log at DEBUG, got:\n{output}"
    );
}

/// Go: `TestCollect_SymlinkOutsideDevWarnsOnlyWhenAllowed`. Symlinks out of `/dev` are in every
/// whole-`/dev` mount; only one an explicit allow glob names, where the operator expected a
/// device, is a warning.
#[test]
fn collect_symlink_outside_dev_warns_only_when_allowed() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.abs_link("dri/stdin", "/proc/self/fd/0");
    dev.abs_link("dri/card9", "/tmp/card9");
    let (log, sink) = capture_logs(Level::DEBUG);

    want_rules(
        &collect_logged(&dev, &global(&["/dev/dri/card*"], &[]), &log, &["/dev/dri"]),
        &[],
    );
    let output = sink.contents();
    let records: Vec<&str> = output
        .lines()
        .filter(|line| line.contains("device path resolves outside /dev"))
        .collect();
    let warned: Vec<&&str> = records
        .iter()
        .filter(|line| line.contains("level=WARN"))
        .collect();
    let debugged: Vec<&&str> = records
        .iter()
        .filter(|line| line.contains("level=DEBUG"))
        .collect();
    assert!(
        warned.len() == 1 && warned[0].contains("path=/dev/dri/card9"),
        "WARN records {warned:?}, want one for /dev/dri/card9"
    );
    assert!(
        debugged.len() == 1 && debugged[0].contains("path=/dev/dri/stdin"),
        "DEBUG records {debugged:?}, want one for /dev/dri/stdin"
    );
}

/// Go: `TestCollect_RelativeSymlinkPolicyOnAllThreeNames`.
#[test]
fn collect_relative_symlink_policy_on_all_three_names() {
    const SOURCE: &str = "/dev/disk/by-id/usb-x";
    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");
    dev.directory("disk/by-id");
    dev.alias("disk/by-id/usb-x", "sda"); // ../../sda

    want_rules(&collect_from(&dev, &all(), &[SOURCE]), &[DEV_SDA]);
    for deny in ["/dev/disk/by-id/*", "/dev/sda"] {
        want_rules(&collect_from(&dev, &global(&[], &[deny]), &[SOURCE]), &[]);
    }
    want_rules(
        &collect_from(&dev, &global(&["/dev/disk/by-id/*"], &[]), &[SOURCE]),
        &[],
    );
    want_rules(
        &collect_from(&dev, &global(&["/dev/sda"], &[]), &[SOURCE]),
        &[DEV_SDA],
    );
}

/// Go: `TestCollect_AliasDenySuppressesDeviceAcrossMounts`. A deny on one name holds against the
/// device under every other name, even one that is walked separately and allowed.
#[test]
fn collect_alias_deny_suppresses_device_across_mounts() {
    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");
    dev.directory("disk/by-id");
    dev.alias("disk/by-id/usb-x", "sda");
    let policy = global(&["/dev/sda"], &["/dev/disk/by-id/*"]);
    let (log, sink) = capture_logs(Level::DEBUG);

    for order in [
        ["/dev/sda", "/dev/disk/by-id"],
        ["/dev/disk/by-id", "/dev/sda"],
    ] {
        let got = collect_logged(&dev, &policy, &log, &order);
        want_rules(&got, &[]);
        assert_eq!(got.skipped, 2, "{order:?}: want both candidates");
    }
    let output = sink.contents();
    assert!(
        output.contains("device denied by policy under one of its names")
            && output.contains("denied_by=/dev/disk/by-id/usb-x"),
        "want an INFO naming the denying alias, got:\n{output}"
    );
}

/// Go: `TestCollect_ByPathAliasGrantedByCanonical`.
#[test]
fn collect_by_path_alias_granted_by_canonical() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.device("dri/card0", DEV_CARD, "dri/card0");
    dev.device("dri/renderD128", DEV_REND, "dri/renderD128");
    dev.directory("dri/by-path");
    dev.alias("dri/by-path/pci-0000:00:02.0-card", "dri/card0");

    let got = collect_from(&dev, &global(&["/dev/dri/*"], &[]), &["/dev/dri"]);
    want_rules(&got, &[DEV_CARD, DEV_REND]);
    assert_eq!(got.skipped, 0);
}

/// Go: `TestCollect_AbsoluteSymlinkUnderDeniedAliasVetoed`. `openat2` refuses an absolute
/// symlink even into `/dev`; the retry follows it by hand so a planted `/dev/evil -> /dev/sda` is
/// still judged under both names.
#[test]
fn collect_absolute_symlink_under_denied_alias_vetoed() {
    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");
    dev.directory("evil");
    dev.abs_link("evil/link", "/dev/sda");

    want_rules(
        &collect_from(
            &dev,
            &global(&[], &["/dev/evil/*"]),
            &["/dev/sda", "/dev/evil"],
        ),
        &[],
    );
    want_rules(&collect_from(&dev, &all(), &["/dev/evil"]), &[DEV_SDA]);
}

/// Go: `TestCollect_DirectoryBehindAbsoluteSymlink`. A directory mounted through an absolute
/// symlink into `/dev` is walked, and each entry is still followed through the link and judged
/// under its alias.
#[test]
fn collect_directory_behind_absolute_symlink() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.device("dri/card0", DEV_CARD, "dri/card0");
    dev.abs_link("gpu", "/dev/dri");

    want_rules(
        &collect_from(&dev, &global(&["/dev/dri/*"], &[]), &["/dev/gpu"]),
        &[DEV_CARD],
    );
    want_rules(
        &collect_from(&dev, &global(&[], &["/dev/gpu/*"]), &["/dev/gpu"]),
        &[],
    );
}

/// Go: `TestCollect_AbsoluteSymlinkLoopUnresolved`.
#[test]
fn collect_absolute_symlink_loop_unresolved() {
    let mut dev = FakeDevFs::new();
    dev.abs_link("a", "/dev/b");
    dev.abs_link("b", "/dev/a");

    want_unresolved(
        &collect_from(&dev, &all(), &["/dev/a"]),
        &["/dev/a", "too many levels"],
    );
}

/// Go: `TestCollect_DotDotSourceRejected`.
#[test]
fn collect_dot_dot_source_rejected() {
    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");

    for source in ["/dev/../etc/passwd", "/dev//sda", "/dev/sda/"] {
        let got = collect_from(&dev, &all(), &[source]);
        want_rules(&got, &[]);
        assert_eq!(got.skipped, 1, "{source}");
    }
}

/// Go: `TestCollect_UeventProblemsEmptyTheSet`.
#[test]
fn collect_uevent_problems_empty_the_set() {
    for (name, uevent) in [
        ("missing", ""),
        ("no DEVNAME", "MAJOR=8\n"),
        ("two DEVNAME", "DEVNAME=sda\nDEVNAME=sdb\n"),
        ("DEVNAME=../sda", "DEVNAME=../sda\n"),
        ("absolute", "DEVNAME=/dev/sda\n"),
        ("empty DEVNAME", "DEVNAME=\n"),
        ("unclean DEVNAME", "DEVNAME=./sda\n"),
    ] {
        let mut dev = FakeDevFs::new();
        dev.device("null", DEV_NULL, "null");
        dev.device("sda", DEV_SDA, "sda");
        if uevent.is_empty() {
            dev.uevents.remove(&DEV_SDA);
        } else {
            dev.uevents.insert(DEV_SDA, uevent.to_owned());
        }

        let got = collect_from(&dev, &all(), &["/dev/null", "/dev/sda"]);
        assert!(!got.device_errs.is_empty(), "{name}");
        want_unresolved(&got, &["/dev/sda", "b 8:0"]);
    }
}

/// Go: `TestCollect_UeventMissingButExcludedByPolicy`. Without a canonical name a device cannot
/// be authorized; when its other names already exclude it the set is still known.
#[test]
fn collect_uevent_missing_but_excluded_by_policy() {
    let pts = char_dev(136, 0);
    let mut dev = FakeDevFs::new();
    dev.directory("pts");
    dev.device("pts/0", pts, "");
    dev.uevents.remove(&pts);
    dev.device("null", DEV_NULL, "null");
    let sources = ["/dev/null", "/dev/pts"];

    want_rules(
        &collect_from(&dev, &global(&["/dev/null"], &[]), &sources),
        &[DEV_NULL],
    );
    want_rules(
        &collect_from(&dev, &global(&[], &["/dev/pts/*"]), &sources),
        &[DEV_NULL],
    );
    want_unresolved(&collect_from(&dev, &all(), &["/dev/pts"]), &["/dev/pts/0"]);
}

/// Go: `TestCollect_EntryReplacedAfterEnumerationFdIdentityWins`. The walk only enumerates
/// names: an entry swapped after enumeration is judged by what its descriptor is, not by the
/// name it had.
#[test]
fn collect_entry_replaced_after_enumeration_fd_identity_wins() {
    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");
    dev.directory("dri");
    // By the time it is opened, dri/card0 is sda.
    dev.other("dri/card0", NodeKind::Device(DEV_SDA), "/dev/dri/card0");

    want_rules(
        &collect_from(&dev, &global(&["/dev/dri/*"], &[]), &["/dev/dri"]),
        &[],
    );
    want_rules(&collect_from(&dev, &all(), &["/dev/dri"]), &[DEV_SDA]);
}

/// Go: `TestCollect_OtherOpenErrorUnresolved`.
#[test]
fn collect_other_open_error_unresolved() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.touch("dri/card0");
    dev.errs.insert(
        "dri/card0".to_owned(),
        SysError::new("openat2 dri/card0", Errno::ACCESS),
    );

    want_unresolved(
        &collect_from(&dev, &all(), &["/dev/dri"]),
        &["/dev/dri/card0"],
    );
}

/// Go: `TestCollect_MissingSourceUnresolved`.
#[test]
fn collect_missing_source_unresolved() {
    let dev = FakeDevFs::new();

    want_unresolved(
        &collect_from(&dev, &all(), &["/dev/ttyUSB0"]),
        &["/dev/ttyUSB0"],
    );
}

/// Go: `TestCollect_SingleFileNonDevice`.
#[test]
fn collect_single_file_non_device() {
    let mut dev = FakeDevFs::new();
    dev.other("regular", NodeKind::Other, "/dev/regular");

    let got = collect_from(&dev, &all(), &["/dev/regular"]);
    assert!(got.granted.is_empty(), "rules {:?}", got.granted);
    assert_eq!(
        got.device_errs,
        [DeviceError::NotDevice("/dev/regular".to_owned())]
    );
}

/// Go: `TestCollect_DirectorySkips`. Dangling names and non-device entries of a directory mount
/// are skipped, never errors; only a dangling name an explicit allow glob matches warns.
#[test]
fn collect_directory_skips() {
    let mut dev = FakeDevFs::new();
    dev.directory("d");
    dev.device("null", DEV_NULL, "null");
    dev.alias("d/null", "null");
    dev.touch("d/nullish"); // dangling: not registered
    dev.other("d/regular", NodeKind::Other, "/dev/d/regular");
    dev.directory("d/sub");
    dev.other("d/linktodir", NodeKind::Dir, "/dev/d/sub");
    let (log, sink) = capture_logs(Level::DEBUG);

    let got = collect_logged(
        &dev,
        &global(&["/dev/null", "/dev/d/nullish"], &[]),
        &log,
        &["/dev/d"],
    );
    want_rules(&got, &[DEV_NULL]);
    assert_eq!(got.skipped, 3, "want nullish, regular and linktodir");
    let output = sink.contents();
    let warns = warn_lines(&output);
    assert!(
        warns.len() == 1
            && warns[0].contains("device symlink matches allow policy but cannot be resolved")
            && warns[0].contains("path=/dev/d/nullish"),
        "want one WARN for nullish, got {warns:?}"
    );
    for debug in ["non-device entry skipped", "symlink to directory skipped"] {
        assert!(
            output.contains(debug),
            "want DEBUG {debug:?}, got:\n{output}"
        );
    }
}

/// Go: `TestCollect_IncompleteEnumeration`, its collection half (see the module comment). Every
/// way a directory mount's names can go unseen leaves the set unknown: an alias that was not
/// enumerated might deny a device that was.
#[test]
fn collect_incomplete_enumeration() {
    type Setup = fn(&mut FakeDevFs);
    type Class = fn(&DeviceError) -> bool;
    let cases: [(&str, Setup, Class); 4] = [
        (
            "root cannot be opened",
            |dev| {
                dev.directory("dri");
                dev.device("dri/card0", DEV_CARD, "dri/card0");
                dev.errs.insert(
                    "dri".to_owned(),
                    SysError::new("openat2 dri", Errno::ACCESS),
                );
            },
            DeviceError::is_unresolved,
        ),
        (
            "root vanished before the walk",
            |dev| {
                dev.nodes.insert(
                    "dri".to_owned(),
                    FakeNode {
                        kind: NodeKind::Dir,
                        path: "/dev/dri".to_owned(),
                    },
                );
            },
            DeviceError::is_incomplete_walk,
        ),
        (
            "directory read error",
            |dev| {
                dev.directory("dri");
                dev.directory("dri/by-path");
                dev.walk_errs
                    .insert("dri/by-path".to_owned(), SysError::new("", Errno::ACCESS));
            },
            DeviceError::is_incomplete_walk,
        ),
        (
            "entry error",
            |dev| {
                dev.directory("dri");
                dev.touch("dri/renderD128");
                dev.walk_errs
                    .insert("dri/renderD128".to_owned(), SysError::new("", Errno::IO));
            },
            DeviceError::is_incomplete_walk,
        ),
    ];
    for (name, setup, sentinel) in cases {
        let mut dev = FakeDevFs::new();
        dev.device("null", DEV_NULL, "null");
        setup(&mut dev);

        let got = collect_from(&dev, &all(), &["/dev/null", "/dev/dri"]);
        assert!(!got.device_errs.is_empty(), "{name}");
        want_incomplete(&got, sentinel, &["/dev/dri"]);
    }
}

/// Go: `TestCollect_EntryCapOverflowIsAnError`.
#[test]
fn collect_entry_cap_overflow_is_an_error() {
    let mut dev = FakeDevFs::new();
    dev.directory("big");
    for index in 0..=MAX_MOUNT_ENTRIES {
        let minor = u32::try_from(index).unwrap();
        dev.device(&format!("big/n{index}"), char_dev(250, minor), "n");
    }

    let got = collect_from(&dev, &all(), &["/dev/big"]);
    want_incomplete(
        &got,
        |error| matches!(error, DeviceError::MountTooLarge(_)),
        &["/dev/big", "narrow the bind mount"],
    );

    // At the cap exactly, the mount is enumerated in full.
    dev.nodes.remove("big/n0");
    std::fs::remove_file(dev.dir.path().join("big/n0")).unwrap();
    let got = collect_from(&dev, &all(), &["/dev/big"]);
    assert!(
        got.device_errs.is_empty(),
        "at the cap: {:?}",
        got.device_errs
    );
    assert_eq!(got.granted.len(), MAX_MOUNT_ENTRIES);
}

/// Go: `TestCollect_DirectoryNoChildrenMatchWarns`.
#[test]
fn collect_directory_no_children_match_warns() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.device("dri/card0", DEV_CARD, "dri/card0");
    dev.device("dri/renderD128", DEV_REND, "dri/renderD128");
    let (log, sink) = capture_logs(Level::DEBUG);

    want_rules(
        &collect_logged(
            &dev,
            &global(&["/dev/dri/nonexistent"], &[]),
            &log,
            &["/dev/dri"],
        ),
        &[],
    );
    let output = sink.contents();
    assert!(
        output.contains("mount excluded: no children matched")
            && output.contains("card0")
            && output.contains("renderD128"),
        "want a WARN naming the children, got:\n{output}"
    );
}

/// Go: `TestCollect_ExclusionWarnDoesNotAliasTheGlobalPolicy`. The Go test guards a slice append
/// writing the container's globs into the global policy's spare capacity; borrowing cannot do
/// that here, so the port pins what the warning lists and that the global policy is unchanged.
#[test]
fn collect_exclusion_warn_does_not_alias_the_global_policy() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.device("dri/card0", DEV_CARD, "dri/card0");
    let global_policy = global(&["/dev/dri/nonexistent"], &["/dev/dri/card0"]);
    let before = global_policy.clone();
    let container_policy = container(&["/dev/dri/other"], &["/dev/dri/x"]);
    let (log, sink) = capture_logs(Level::DEBUG);

    let result = collect_mount_rules(
        &dev,
        "/dev/dri",
        Judge {
            global: &global_policy,
            container: &container_policy,
            log: &log,
        },
    );
    assert!(result.errs.is_empty(), "{:?}", result.errs);
    assert_eq!(
        warn_lines(&sink.contents()),
        [
            "level=WARN mount excluded: no children matched allow/deny policy path=/dev/dri \
             children_seen=[card0] allow_globs=[/dev/dri/nonexistent /dev/dri/other] \
             deny_globs=[/dev/dri/card0 /dev/dri/x]"
        ]
    );
    assert_eq!(global_policy, before);
}

/// The globs of the exclusion warning are Go's nil list when neither policy has any. Both lines
/// of the warning are the output of a Go program running the same collections.
#[test]
fn collect_exclusion_warn_lists_nil_globs() {
    let mut dev = FakeDevFs::new();
    dev.directory("dri");
    dev.device("dri/card0", DEV_CARD, "dri/card0");
    dev.uevents
        .insert(DEV_CARD, "DEVNAME=dri/card0\n".to_owned());
    let (log, sink) = capture_logs(Level::DEBUG);
    let container_policy = container(&[], &["/dev/dri/*"]);

    let result = collect_mount_rules(
        &dev,
        "/dev/dri",
        Judge {
            global: &all(),
            container: &container_policy,
            log: &log,
        },
    );
    assert!(result.errs.is_empty(), "{:?}", result.errs);
    assert_eq!(
        warn_lines(&sink.contents()),
        [
            "level=WARN mount excluded: no children matched allow/deny policy path=/dev/dri \
             children_seen=[card0] allow_globs=[] deny_globs=[/dev/dri/*]"
        ]
    );
}

/// Go: `TestCollect_OnlyBindMountsAreWalked`, its collection halves (see the module comment).
/// Only a bind mount's `Source` is a host path under `/dev`; a volume or tmpfs mount that reports
/// such a `Source` is never walked.
#[test]
fn collect_only_bind_mounts_are_walked() {
    let mut dev = FakeDevFs::new();
    dev.device("sda", DEV_SDA, "sda");
    let mount = |kind: &str, destination: &str| MountPoint {
        kind: kind.to_owned(),
        source: "/dev/sda".to_owned(),
        destination: destination.to_owned(),
    };
    let mut mounts = vec![
        mount("volume", "/data"),
        mount("tmpfs", "/tmp"),
        mount("npipe", "/pipe"),
        mount("", "/untyped"),
    ];
    let log = capture_logs(Level::DEBUG).0;

    let got = collect_mounts(&dev, &all(), &mounts, &log);
    assert!(
        got.dev_mounts == 0 && got.granted.is_empty() && got.device_errs.is_empty(),
        "collected {got:?} from non-bind mounts, want nothing"
    );

    mounts.push(bind("/dev/sda"));
    want_rules(&collect_mounts(&dev, &all(), &mounts, &log), &[DEV_SDA]);
}

/// Go: `TestIsDeviceMountSource` covers `IsMountSource` in `processor_test.go`; the lexical
/// helpers it rests on are pinned here.
#[test]
fn mount_source_and_dev_relative_paths() {
    for (path, want) in [
        ("/dev", true),
        ("/dev/", true),
        ("/dev/sda", true),
        ("/devops/x", false),
        ("/sys/dev", false),
        ("dev/sda", false),
        ("", false),
    ] {
        assert_eq!(is_mount_source(path), want, "{path}");
    }
    assert_eq!(rel_to_dev("/dev"), ".");
    assert_eq!(rel_to_dev("/dev/dri/card0"), "dri/card0");
    assert!(!under_dev("/dev/"));
    assert!(under_dev("/dev"));
}

/// Go: `TestCollect_RealDevNull`.
#[test]
fn collect_real_dev_null() {
    if probe_openat2().is_err() {
        // Go skips without openat2; the daemon refuses to start there.
        return;
    }
    let dev = RealDevFs::open(DEV_ROOT, "/sys").unwrap();

    want_rules(&collect_from(&dev, &all(), &["/dev/null"]), &[DEV_NULL]);
    let got = collect_from(&dev, &global(&[], &["/dev/null"]), &["/dev/null"]);
    want_rules(&got, &[]);
    assert_eq!(got.skipped, 1);
}
