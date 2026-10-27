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

//! Go's `converge_test.go` (a pass that failed halfway converges on the next, and the filter
//! cache rebuilds a wiped cgroup) and the `TestSet_*` tests of `owned_test.go` (what a pass
//! does to wrappers and unmarked programs), against the recording fake.

use std::sync::Arc;

use super::*;
use crate::cgroup::asm::{Instruction, MOV, R0};
use crate::cgroup::bpfinterp::{DevRequest, READ, run_device_filter};
use crate::cgroup::owned::{HDR_BLOCK_LEN, marker_value, parse_owned};
use crate::cgroup::testing::{
    allow_null_original, canonical, corrupt, emit, rule, runtime_original,
};
use crate::cgroup::v2fake::{
    FakeOps, INJECTED, ROLLBACK, Replace, controller, pass, probe_in, set_rules, test_identity,
    test_rules,
};

/// Every attached program parsed: wrappers by lineage nonce and original, bare programs by their
/// bytes. Go: `attachedPrograms`.
fn attached_programs(ops: &FakeOps) -> (Vec<u64>, Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let mut nonces = Vec::new();
    let mut originals = Vec::new();
    let mut bares = Vec::new();
    for name in ops.attached() {
        let raw = ops.raw(&name);
        match parse_owned(&raw).unwrap() {
            Some(owned) => {
                nonces.push(owned.nonce);
                originals.push(owned.original);
            }
            None => bares.push(raw),
        }
    }
    (nonces, originals, bares)
}

/// The wrapper of the allow-null original. Go: `wrapperOf`.
fn wrapper_of(rules: &[DeviceRule], nonce: u64) -> Vec<u8> {
    emit(rules, nonce, &canonical(&allow_null_original()))
}

/// Go: `TestConverge_WrapRollbackFailureThenRetry`. The first wrap attaches N1 but neither the
/// original's detach nor the rollback succeed, leaving O1 and N1 attached. The retry keeps N1
/// and wraps O1 as its own pair: two identical wrappers (one redundant, not wider), no bare
/// program left, and nothing changes on later passes.
#[test]
fn a_wrap_whose_rollback_failed_converges() {
    let ops = FakeOps::owned(&[("O1", &canonical(&allow_null_original()))]);
    ops.fail("detach O1", INJECTED);
    ops.fail("detach N1", ROLLBACK);
    let error = pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("roll back new device filters program"),
        "{error}"
    );
    ops.assert_attached(&["O1", "N1"]);

    ops.state().errs.clear();
    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    ops.assert_calls(&["load N2", "attach N2", "detach O1"]);
    let (nonces, originals, bares) = attached_programs(&ops);
    assert!(
        bares.is_empty()
            && nonces.len() == 2
            && nonces[0] != nonces[1]
            && originals[0] == originals[1],
        "want two identical-original wrappers with distinct nonces and no bare, got nonces \
         {nonces:?} bares {}",
        bares.len()
    );

    for _ in 0..2 {
        pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
        ops.assert_calls(&[]);
    }
    ops.assert_all_closed();
}

/// Go: `TestConverge_RollbackSucceededThenRetryIsOneProgramPerOriginal`. The 0.1 case whose
/// rollback worked: pair 1 replaced, pair 2 back to its original; the retry ends with exactly one
/// wrapper per runtime original.
#[test]
fn a_rolled_back_pass_converges_to_one_wrapper_per_original() {
    let ops = FakeOps::owned(&[
        ("O1", &canonical(&allow_null_original())),
        ("O2", &canonical(&runtime_original())),
    ]);
    ops.fail("detach O2", INJECTED);
    let error = pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap_err();
    assert!(
        error
            .to_string()
            .starts_with("device filter change 2 of 2: "),
        "{error}"
    );
    ops.assert_attached(&["O2", "N1"]);

    ops.state().errs.clear();
    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    let (_, originals, bares) = attached_programs(&ops);
    assert!(
        bares.is_empty() && originals.len() == 2 && originals[0] != originals[1],
        "want one wrapper per original, got {} wrappers {} bares",
        originals.len(),
        bares.len()
    );

    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    ops.assert_calls(&[]);
}

/// Go: `TestConverge_IdenticalBaresBecomeTwoWrappersEachByItsOwnPair`.
#[test]
fn identical_bares_become_two_wrappers_each_by_its_own_pair() {
    let original = canonical(&allow_null_original());
    let ops = FakeOps::owned(&[("O1", &original), ("O2", &original)]);
    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    // Each bare program is detached only right after its own wrapper is in.
    ops.assert_calls(&[
        "load N1",
        "load N2",
        "attach N1",
        "detach O1",
        "attach N2",
        "detach O2",
    ]);
    let (nonces, _, bares) = attached_programs(&ops);
    assert!(
        bares.is_empty() && nonces.len() == 2 && nonces[0] != nonces[1],
        "want two wrappers with distinct nonces, got {nonces:?} (bares {})",
        bares.len()
    );
}

/// Go: `TestConverge_EmptyRestoreDetachFailureThenRetry`.
#[test]
fn a_restore_whose_detach_failed_converges() {
    let original = canonical(&allow_null_original());
    let ops = FakeOps::owned(&[("W1", &wrapper_of(&test_rules(), 11))]);
    ops.fail("detach W1", INJECTED);
    let error = pass(&ops, Replace::Unsupported, None, &[]).unwrap_err();
    assert!(
        error.to_string().contains("unable to detach original"),
        "{error}"
    );
    // The rollback worked: only the wrapper is attached.
    ops.assert_attached(&["W1"]);

    ops.state().errs.clear();
    pass(&ops, Replace::Unsupported, None, &[]).unwrap();
    let (nonces, _, bares) = attached_programs(&ops);
    assert!(
        nonces.is_empty() && bares == [original],
        "want a single bare original, got {} wrappers {} bares",
        nonces.len(),
        bares.len()
    );
}

/// Go: `TestConverge_EmptyRestoreRollbackFailureThenRetry`. The bare original is attached, the
/// wrapper's detach fails and so does the rollback of the original. The retry sees the original
/// already bare and only detaches the wrapper (removing a wrapper next to its own original only
/// narrows).
#[test]
fn a_restore_whose_rollback_failed_converges() {
    let original = canonical(&allow_null_original());
    let ops = FakeOps::owned(&[("W1", &wrapper_of(&test_rules(), 11))]);
    ops.fail("detach W1", INJECTED);
    ops.fail("detach N1", ROLLBACK);
    let error = pass(&ops, Replace::Unsupported, None, &[]).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("roll back new device filters program"),
        "{error}"
    );
    ops.assert_attached(&["W1", "N1"]);

    ops.state().errs.clear();
    pass(&ops, Replace::Unsupported, None, &[]).unwrap();
    ops.assert_calls(&["detach W1"]);
    ops.assert_attached(&["N1"]);
    assert_eq!(
        ops.raw("N1"),
        original,
        "remaining program is not the bare original"
    );

    pass(&ops, Replace::Unsupported, None, &[]).unwrap();
    ops.assert_calls(&[]);
}

/// Go: `TestConverge_EmptyRestoreKeepsIdenticalRuntimeProgramCount`. One bare program satisfies
/// one lineage: two identical originals, each wrapped, are restored as two bare programs.
#[test]
fn a_restore_keeps_the_count_of_identical_runtime_programs() {
    let ops = FakeOps::owned(&[
        ("W1", &wrapper_of(&test_rules(), 11)),
        ("W2", &wrapper_of(&test_rules(), 12)),
    ]);
    pass(&ops, Replace::Supported, None, &[]).unwrap();
    let (nonces, _, bares) = attached_programs(&ops);
    assert!(
        nonces.is_empty() && bares.len() == 2,
        "want two bare originals, got {} wrappers {} bares",
        nonces.len(),
        bares.len()
    );
}

/// Go: `TestConverge_StaleSameNonceWrappersLeaveOne`, both subtests.
#[test]
fn stale_wrappers_of_one_lineage_leave_one() {
    let older = [rule("c", 10, 201, "r")];

    // None equal to the desired wrapper.
    let ops = FakeOps::owned(&[
        ("W1", &wrapper_of(&older, 11)),
        ("W2", &wrapper_of(&[], 11)),
    ]);
    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    ops.assert_calls(&["load N1", "attach N1", "detach W1", "detach W2"]);
    ops.assert_attached(&["N1"]);

    // One already equal.
    let ops = FakeOps::owned(&[
        ("W1", &wrapper_of(&older, 11)),
        ("W2", &wrapper_of(&test_rules(), 11)),
    ]);
    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    ops.assert_calls(&["detach W1"]);
    ops.assert_attached(&["W2"]);
}

/// Go: `TestConverge_IdenticalWrappersWithDifferentNoncesBothRemain`.
#[test]
fn identical_wrappers_with_different_nonces_both_remain() {
    let ops = FakeOps::owned(&[
        ("W1", &wrapper_of(&test_rules(), 11)),
        ("W2", &wrapper_of(&test_rules(), 12)),
    ]);
    pass(&ops, Replace::Unsupported, None, &test_rules()).unwrap();
    ops.assert_calls(&[]);
    ops.assert_attached(&["W1", "W2"]);
}

/// Go: `TestConverge_WrapperOverWrapperIsNormalised`.
#[test]
fn a_wrapper_over_a_wrapper_is_normalised() {
    let original = canonical(&allow_null_original());
    let inner = emit(&[rule("c", 10, 201, "r")], 21, &original);
    let ops = FakeOps::owned(&[("W1", &emit(&test_rules(), 22, &inner))]);
    pass(&ops, Replace::Supported, None, &test_rules()).unwrap();
    assert_eq!(
        ops.raw("N1"),
        emit(&test_rules(), 22, &original),
        "wrapper over a wrapper was not re-emitted as one wrapper over the innermost original"
    );
}

/// Go: `TestConverge_WipeAfterWrapRebuildsSameLineage`.
#[test]
fn a_wipe_after_a_wrap_rebuilds_the_same_lineage() {
    let original = canonical(&allow_null_original());
    let cache = Arc::new(FilterCache::new());
    let ops = FakeOps::owned(&[("O1", &original)]);
    pass(&ops, Replace::Supported, Some(&cache), &test_rules()).unwrap();
    let (nonces, _, _) = attached_programs(&ops);

    ops.state().attached.clear(); // systemd daemon-reload
    let (controller, logs) = controller(&ops, probe_in(Replace::Supported), Arc::clone(&cache));
    ops.state().calls.clear();
    controller
        .set_rules(rustix::fs::CWD, &test_identity(), &test_rules())
        .unwrap();
    ops.assert_calls(&["load N2", "attach N2"]);
    assert_eq!(
        logs.contents(),
        "level=INFO device filter missing; reattaching the cached runtime originals \
         cgroup=/sys/fs/cgroup/test programs=1 with_grants=true\n"
    );

    let (rebuilt, originals, _) = attached_programs(&ops);
    assert!(
        rebuilt == nonces && originals == [original],
        "rebuilt lineage {rebuilt:?}, want nonce {nonces:?} around the original"
    );
    let raw = ops.raw("N2");
    assert_eq!(run_device_filter(&raw, DevRequest::char(10, 200, READ)), 1);
    assert_eq!(run_device_filter(&raw, DevRequest::char(10, 201, READ)), 0);
}

/// Go: `TestConverge_WipeWithEmptyRulesRestoresBareOriginal`.
#[test]
fn a_wipe_with_empty_rules_restores_the_bare_original() {
    let original = canonical(&allow_null_original());
    let cache = Arc::new(FilterCache::new());
    let ops = FakeOps::owned(&[("O1", &original)]);
    pass(&ops, Replace::Supported, Some(&cache), &test_rules()).unwrap();

    ops.state().attached.clear();
    pass(&ops, Replace::Supported, Some(&cache), &[]).unwrap();
    ops.assert_calls(&["load N2", "attach N2"]);
    assert_eq!(
        ops.raw("N2"),
        original,
        "wipe with empty rules did not restore the bare runtime original"
    );
}

/// Go: `TestConverge_ForgottenCacheCannotRebuild`.
#[test]
fn a_forgotten_cache_cannot_rebuild() {
    let cache = Arc::new(FilterCache::new());
    let ops = FakeOps::owned(&[("O1", &canonical(&allow_null_original()))]);
    pass(&ops, Replace::Supported, Some(&cache), &test_rules()).unwrap();

    cache.forget(&test_identity());
    ops.state().attached.clear();
    let error = pass(&ops, Replace::Supported, Some(&cache), &test_rules()).unwrap_err();
    assert!(error.is_filter_missing(), "{error}");
    ops.assert_calls(&[]);
}

/// Go: `TestConverge_CacheIsKeyedByIdentity`.
#[test]
fn the_cache_is_keyed_by_identity() {
    let cache = FilterCache::new();
    cache.set(
        &test_identity(),
        CacheEntry {
            lineages: vec![CachedLineage {
                nonce: 0,
                original: vec![1],
                meta: ProgMeta::default(),
            }],
            rebuilding: false,
        },
    );
    let mut restarted = test_identity();
    restarted.inode += 1;
    assert!(
        cache.get(&restarted).lineages.is_empty(),
        "a new cgroup inode at the same path sees cached lineages"
    );
}

/// Go: `TestConverge_PartialRebuildResumes`. The cgroup had two runtime programs, the wipe
/// removed both, and the rebuild attaches the first but fails on the second. The retry sees one
/// program attached (not a wiped cgroup) and must still reattach the lost lineage instead of
/// forgetting it.
#[test]
fn a_partial_rebuild_resumes() {
    let first = canonical(&allow_null_original());
    let second = canonical(&runtime_original());
    let cache = Arc::new(FilterCache::new());
    let ops = FakeOps::owned(&[("O1", &first), ("O2", &second)]);

    for rules in [test_rules(), Vec::new()] {
        pass(&ops, Replace::Supported, Some(&cache), &test_rules()).unwrap();

        ops.state().attached.clear();
        ops.state().errs.clear();
        let failing = format!("attach N{}", ops.state().loads + 2);
        ops.fail(&failing, INJECTED);
        let error = pass(&ops, Replace::Supported, Some(&cache), &rules).unwrap_err();
        assert!(
            error
                .to_string()
                .starts_with("device filter change 2 of 2: "),
            "rules {rules:?}: {error}, want the injected {failing} failure"
        );
        assert_eq!(
            ops.attached().len(),
            1,
            "rules {rules:?}: want the first lineage only"
        );

        ops.state().errs.clear();
        pass(&ops, Replace::Supported, Some(&cache), &rules).unwrap();
        let (nonces, originals, bares) = attached_programs(&ops);
        let got: Vec<Vec<u8>> = originals.into_iter().chain(bares.iter().cloned()).collect();
        assert!(
            got.len() == 2 && got.contains(&first) && got.contains(&second),
            "rules {rules:?}: after the retry want both runtime originals, got {} wrappers {} \
             bares",
            nonces.len(),
            bares.len()
        );
        assert!(
            !cache.get(&test_identity()).rebuilding,
            "rules {rules:?}: rebuild still pending after a complete pass"
        );

        pass(&ops, Replace::Supported, Some(&cache), &rules).unwrap();
        ops.assert_calls(&[]);
    }
}

/// Go: `TestSet_ApplyTwiceKeepsLengthAndLineage`.
#[test]
fn applying_twice_keeps_the_length_and_the_lineage() {
    let original = canonical(&allow_null_original());
    let ops = FakeOps::owned(&[("O1", &original)]);
    set_rules(&ops, Replace::Supported, None, &test_rules()).unwrap();
    let first = ops.raw(&ops.attached()[0]);
    set_rules(&ops, Replace::Supported, None, &test_rules()).unwrap();
    let second = ops.raw(&ops.attached()[0]);
    assert_eq!(
        second.len(),
        first.len(),
        "the program grew on a second identical apply"
    );
    let first = parse_owned(&first).unwrap().unwrap();
    let second = parse_owned(&second).unwrap().unwrap();
    assert_eq!(second.nonce, first.nonce);
    assert_eq!(second.original, original);
    assert_eq!(ops.attached().len(), 1);
}

/// Go: `TestSet_WrapperIsNamedAndRestoreIsBare`.
#[test]
fn the_wrapper_is_named_and_the_restore_is_bare() {
    let original = canonical(&allow_null_original());
    let ops = FakeOps::owned(&[("O1", &original)]);
    let (controller, logs) = controller(
        &ops,
        probe_in(Replace::Supported),
        Arc::new(FilterCache::new()),
    );
    controller
        .set_rules(rustix::fs::CWD, &test_identity(), &test_rules())
        .unwrap();
    assert_eq!(ops.state().names["N1"], OWNED_PROGRAM_NAME);
    assert_eq!(
        logs.contents(),
        "level=INFO wrapping device filter with no prior owned block; pre-upgrade grants, if \
         any, are not managed cgroup=/sys/fs/cgroup/test\n"
    );

    set_rules(&ops, Replace::Supported, None, &[]).unwrap();
    ops.assert_calls(&["load N1", "replace O1->N1", "load N2", "replace N1->N2"]);
    assert_eq!(
        ops.raw("N2"),
        original,
        "empty rules did not restore the bare original"
    );
    assert_eq!(
        ops.state().names["N2"],
        "",
        "the restored original carries a name"
    );
    ops.assert_all_closed();
}

/// Go: `TestSet_EmptyRulesLeaveUnmarkedProgramsAlone`.
#[test]
fn empty_rules_leave_unmarked_programs_alone() {
    let ops = FakeOps::owned(&[("O1", &canonical(&allow_null_original()))]);
    set_rules(&ops, Replace::Supported, None, &[]).unwrap();
    ops.assert_calls(&[]);
    ops.assert_attached(&["O1"]);
    ops.assert_all_closed();
}

/// Go: `TestSet_NothingAttachedAndNothingCachedIsFilterMissing`.
#[test]
fn nothing_attached_and_nothing_cached_is_filter_missing() {
    for rules in [Vec::new(), test_rules()] {
        let ops = FakeOps::new(&[]);
        let error = set_rules(&ops, Replace::Supported, None, &rules).unwrap_err();
        assert!(
            error.is_filter_missing(),
            "rules {rules:?}: {error}, want the missing filter (never a silent success)"
        );
        ops.assert_calls(&[]);
    }
}

/// Go: `TestSet_ConflictMutatesNothing`.
#[test]
fn a_conflict_mutates_nothing() {
    let original = canonical(&allow_null_original());
    let wrapped = emit(&test_rules(), 5, &original);
    let instructions = from_canonical(&wrapped).unwrap();
    let block_len = marker_value(&instructions[HDR_BLOCK_LEN]).unwrap();
    let broken = corrupt(
        &wrapped,
        usize::try_from(block_len).unwrap(),
        Instruction::alu32_imm(MOV, R0, 0),
    );

    // O1 is fine; O2 conflicts. Nothing may happen to either.
    let ops = FakeOps::owned(&[("O1", &original), ("O2", &broken)]);
    for rules in [test_rules(), Vec::new()] {
        let error = set_rules(&ops, Replace::Supported, None, &rules).unwrap_err();
        assert!(error.is_owned_block_conflict(), "{error}");
    }
    ops.assert_calls(&[]);
    ops.assert_attached(&["O1", "O2"]);
    ops.assert_all_closed();
}

/// Go: `TestSet_UnwrappableProgramMutatesNothing`.
#[test]
fn an_unwrappable_program_mutates_nothing() {
    let ops = FakeOps::owned(&[("O1", &canonical(&allow_null_original()))]);
    ops.state().meta = ProgMeta {
        map_ids_known: true,
        map_count: 1,
    };
    let error = set_rules(&ops, Replace::Supported, None, &test_rules()).unwrap_err();
    assert!(error.is_program_not_wrappable(), "{error}");
    assert_eq!(
        error.to_string(),
        "device filter program cannot be wrapped safely: program uses 1 maps"
    );
    ops.assert_calls(&[]);
    ops.assert_all_closed();
}

/// Go: `TestSet_StripsOnlyValidatedBlocks`.
#[test]
fn only_validated_blocks_are_stripped() {
    let original = canonical(&allow_null_original());
    let older = emit(&test_rules(), 7, &original);
    let ops = FakeOps::owned(&[("O1", &older)]);
    set_rules(&ops, Replace::Supported, None, &[rule("b", 8, 0, "r")]).unwrap();
    let parsed = parse_owned(&ops.raw("N1")).unwrap().unwrap();
    assert!(
        parsed.original == original && parsed.nonce == 7,
        "new wrapper does not wrap the stripped original under the old nonce"
    );
    assert_eq!(
        run_device_filter(&ops.raw("N1"), DevRequest::char(10, 200, READ)),
        0,
        "grant from the replaced wrapper survived"
    );
}
