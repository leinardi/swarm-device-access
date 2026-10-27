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

//! Go's `v2_test.go` (the parsers) and `v2swap_test.go` (the order of the kernel calls of a pass,
//! against the recording fake). `TestPrependDeviceFilter_RefusesMissingOriginal` is in
//! `ebpf/tests.rs`.

use rustix::io::Errno;

use super::*;
use crate::cgroup::v2fake::{
    FakeOps, INJECTED, LOAD_FAILS, ROLLBACK, Replace, controller, probe_in, set_rules,
    test_identity, test_rules,
};
use crate::cgroup::v2ops::{F_ALLOW_OVERRIDE, OpsError, ReplaceProbe};
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

/// One pass of [`test_rules`] with `probe` and no cache. Go: `run`.
fn run(ops: &FakeOps, probe: &'static ReplaceProbe) -> Result<(), Error> {
    let (controller, _) = controller(ops, probe, Arc::new(FilterCache::new()));
    controller.set_rules(rustix::fs::CWD, &test_identity(), &test_rules())
}

/// Go: `TestSwap_UsesAtomicReplaceWhenSupported`.
#[test]
fn atomic_replace_is_used_when_supported() {
    for state in [Replace::Unknown, Replace::Supported] {
        let ops = FakeOps::new(&["O1", "O2"]);
        let probe = probe_in(state);
        run(&ops, probe).unwrap();
        ops.assert_calls(&["load N1", "load N2", "replace O1->N1", "replace O2->N2"]);
        ops.assert_attached(&["N1", "N2"]);
        ops.assert_all_closed();
        assert!(
            probe.supported(),
            "{state:?}: the probe did not learn the support"
        );
    }
}

/// Go: `TestSwap_AttachBeforeDetachWhenReplaceUnsupported`.
#[test]
fn attach_comes_before_detach_without_replace() {
    let ops = FakeOps::new(&["O1", "O2"]);
    run(&ops, probe_in(Replace::Unsupported)).unwrap();
    ops.assert_calls(&[
        "load N1",
        "load N2",
        "attach N1",
        "detach O1",
        "attach N2",
        "detach O2",
    ]);
    ops.assert_attached(&["N1", "N2"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_EINVALOnFirstReplaceIsCachedAsUnsupported`. An `EINVAL` on the first replace
/// switches to attach-then-detach for this and every later pair, without retrying the replace.
#[test]
fn einval_on_the_first_replace_is_kept_as_unsupported() {
    let ops = FakeOps::new(&["O1", "O2"]);
    ops.fail("replace O1->N1", Errno::INVAL);
    let probe = probe_in(Replace::Unknown);
    run(&ops, probe).unwrap();
    ops.assert_calls(&[
        "load N1",
        "load N2",
        "replace O1->N1",
        "attach N1",
        "detach O1",
        "attach N2",
        "detach O2",
    ]);
    ops.assert_attached(&["N1", "N2"]);
    ops.assert_all_closed();
    assert!(!probe.usable(), "the probe still offers the replace");
}

/// Go: `TestSwap_ReplaceFailureOnSupportingKernelIsReturned`. A replace failure other than the
/// probe's `EINVAL` is an error, not a fallback.
#[test]
fn a_replace_failure_on_a_supporting_kernel_is_returned() {
    let ops = FakeOps::new(&["O1"]);
    ops.fail("replace O1->N1", INJECTED);
    let error = run(&ops, probe_in(Replace::Supported)).unwrap_err();
    assert_eq!(error.to_string(), REPLACE_FAILED);
    ops.assert_calls(&["load N1", "replace O1->N1"]);
    ops.assert_attached(&["O1"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_FirstAttachFailureDetachesNothing`.
#[test]
fn a_first_attach_failure_detaches_nothing() {
    let ops = FakeOps::new(&["O1", "O2"]);
    ops.fail("attach N1", INJECTED);
    let error = run(&ops, probe_in(Replace::Unsupported)).unwrap_err();
    assert_eq!(error.to_string(), ATTACH_FAILED_1_OF_2);
    ops.assert_calls(&["load N1", "load N2", "attach N1"]);
    ops.assert_attached(&["O1", "O2"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_LaterAttachFailureLeavesEarlierPairReplaced`.
#[test]
fn a_later_attach_failure_leaves_the_earlier_pair_replaced() {
    let ops = FakeOps::new(&["O1", "O2"]);
    ops.fail("attach N2", INJECTED);
    let error = run(&ops, probe_in(Replace::Unsupported)).unwrap_err();
    assert_eq!(error.to_string(), ATTACH_FAILED_2_OF_2);
    ops.assert_calls(&["load N1", "load N2", "attach N1", "detach O1", "attach N2"]);
    ops.assert_attached(&["O2", "N1"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_DetachFailureRollsBackThatPair`.
#[test]
fn a_detach_failure_rolls_that_pair_back() {
    let ops = FakeOps::new(&["O1"]);
    ops.fail("detach O1", INJECTED);
    let error = run(&ops, probe_in(Replace::Unsupported)).unwrap_err();
    assert_eq!(error.to_string(), DETACH_FAILED);
    ops.assert_calls(&["load N1", "attach N1", "detach O1", "detach N1"]);
    ops.assert_attached(&["O1"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_RollbackFailureIsJoined`.
#[test]
fn a_rollback_failure_is_joined() {
    let ops = FakeOps::new(&["O1"]);
    ops.fail("detach O1", INJECTED);
    ops.fail("detach N1", ROLLBACK);
    let error = run(&ops, probe_in(Replace::Unsupported)).unwrap_err();
    assert_eq!(error.to_string(), ROLLBACK_FAILED);
    assert!(matches!(
        &error,
        Error::Change { source, .. } if matches!(source.as_ref(), Error::Joined(joined) if joined.len() == 2)
    ));
    ops.assert_calls(&["load N1", "attach N1", "detach O1", "detach N1"]);
    ops.assert_attached(&["O1", "N1"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_SecondDetachFailureLeavesPairOneReplacedPairTwoRolledBack`.
#[test]
fn a_second_detach_failure_leaves_pair_one_replaced_and_pair_two_rolled_back() {
    let ops = FakeOps::new(&["O1", "O2"]);
    ops.fail("detach O2", INJECTED);
    let error = run(&ops, probe_in(Replace::Unsupported)).unwrap_err();
    assert_eq!(error.to_string(), DETACH_FAILED_2_OF_2);
    ops.assert_calls(&[
        "load N1",
        "load N2",
        "attach N1",
        "detach O1",
        "attach N2",
        "detach O2",
        "detach N2",
    ]);
    ops.assert_attached(&["O2", "N1"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_NonMultiAttachModeIsRefused`.
#[test]
fn a_non_multi_attach_mode_is_refused() {
    for (flags, text) in [
        (0, UNSUPPORTED_EXCLUSIVE),
        (F_ALLOW_OVERRIDE, UNSUPPORTED_OVERRIDE),
    ] {
        let ops = FakeOps::new(&["O1"]);
        ops.state().flags = flags;
        let error = run(&ops, probe_in(Replace::Unknown)).unwrap_err();
        assert!(
            error.is_unsupported_attach_mode(),
            "flags {flags:#x}: {error}"
        );
        assert_eq!(error.to_string(), text);
        ops.assert_calls(&[]);
        ops.assert_attached(&["O1"]);
        ops.assert_all_closed();
    }
}

/// Go: `TestSwap_AnyInaccessibleProgramRefusesEverything`.
#[test]
fn any_inaccessible_program_refuses_everything() {
    let ops = FakeOps::new(&["O1"]);
    ops.state().inaccessible = 1;
    let error = run(&ops, probe_in(Replace::Unknown)).unwrap_err();
    assert!(error.is_filters_inaccessible(), "{error}");
    assert_eq!(error.to_string(), INACCESSIBLE_1_OF_2);
    ops.assert_calls(&[]);
    ops.assert_attached(&["O1"]);
    ops.assert_all_closed();
}

/// Go: `TestSwap_OnlyInaccessibleProgramsIsNotMissing`. No handles is not read as nothing
/// attached when the kernel counted programs.
#[test]
fn only_inaccessible_programs_is_not_missing() {
    let ops = FakeOps::new(&[]);
    ops.state().inaccessible = 2;
    let error = run(&ops, probe_in(Replace::Unknown)).unwrap_err();
    assert!(
        error.is_filters_inaccessible() && !error.is_filter_missing(),
        "{error}"
    );
    ops.assert_calls(&[]);
}

/// Go: `TestSwap_NoProgramAttachedFailsClosed`.
#[test]
fn no_program_attached_fails_closed() {
    let ops = FakeOps::new(&[]);
    let error = run(&ops, probe_in(Replace::Unknown)).unwrap_err();
    assert!(error.is_filter_missing(), "{error}");
    assert_eq!(error.to_string(), FILTER_MISSING);
    ops.assert_calls(&[]);
    ops.assert_attached(&[]);
}

/// Go: `TestSwap_LoadFailureMutatesNothing`.
#[test]
fn a_load_failure_mutates_nothing() {
    let ops = FakeOps::new(&["O1", "O2"]);
    ops.fail("load N2", LOAD_FAILS);
    let error = set_rules(&ops, Replace::Unknown, None, &test_rules()).unwrap_err();
    assert_eq!(error.to_string(), LOAD_FAILED);
    ops.assert_calls(&["load N1", "load N2"]);
    ops.assert_attached(&["O1", "O2"]);
    ops.assert_all_closed();
}

/// A query failure is wrapped as Go wraps it.
#[test]
fn a_query_failure_is_wrapped() {
    let error = Error::Query(OpsError::Query(Errno::PERM));
    assert_eq!(error.to_string(), QUERY_FAILED);
}

// The texts below are the Go daemon's, printed by its setDeviceRules over the Go fake whose
// injected errors carry the text of the kernel errors this fake returns.
const REPLACE_FAILED: &str = "device filter change 1 of 1: unable to replace device filters program: failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, flags BPF_F_ALLOW_MULTI): attach program: input/output error";
const ATTACH_FAILED_1_OF_2: &str = "device filter change 1 of 2: unable to attach new device filters program: failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, flags BPF_F_ALLOW_MULTI): attach program: input/output error";
const ATTACH_FAILED_2_OF_2: &str = "device filter change 2 of 2: unable to attach new device filters program: failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, flags BPF_F_ALLOW_MULTI): attach program: input/output error";
const DETACH_FAILED: &str = "device filter change 1 of 1: unable to detach original device filters program: failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): can't detach program: input/output error";
const ROLLBACK_FAILED: &str = "device filter change 1 of 1: unable to detach original device filters program: failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): can't detach program: input/output error\nroll back new device filters program: failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): can't detach program: device or resource busy";
const DETACH_FAILED_2_OF_2: &str = "device filter change 2 of 2: unable to detach original device filters program: failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): can't detach program: input/output error";
const UNSUPPORTED_EXCLUSIVE: &str =
    "device filter attached without BPF_F_ALLOW_MULTI: attach mode none (exclusive)";
const UNSUPPORTED_OVERRIDE: &str =
    "device filter attached without BPF_F_ALLOW_MULTI: attach mode BPF_F_ALLOW_OVERRIDE";
const INACCESSIBLE_1_OF_2: &str =
    "device filters attached to the cgroup cannot be accessed: 1 of 2 programs";
const FILTER_MISSING: &str = "no device filter attached to the cgroup";
const LOAD_FAILED: &str =
    "unable to create new device filters program: load program: cannot allocate memory";
const QUERY_FAILED: &str = "unable to find any existing device filters attached to the cgroup: bpf_prog_query(BPF_CGROUP_DEVICE) failed: operation not permitted";
