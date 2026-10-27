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

//! Go: `owned_test.go`. The `TestSet_*` tests drive the cgroup v2 controller and come with it.

use super::*;
use crate::cgroup::asm::{ADD, CALL, DWORD, IMM_SOURCE, JEQ, MODE_IMM, R3, REG_SOURCE, RFP, WORD};
use crate::cgroup::bpfinterp::{DevRequest, READ, WRITE, run_device_filter};
use crate::cgroup::testing::{
    FIXTURE_NONCE, Golden, ParseGolden, RUNTIME_FIXTURES, fixture_rules, hex_lines, read_fixture,
    rule, rule_of, unhex,
};

const KNOWN: ProgMeta = ProgMeta {
    map_ids_known: true,
    map_count: 0,
};

/// A runtime-style original that allows exactly c 1:3 (any access) and denies everything else.
fn allow_null_original() -> Vec<Instruction> {
    vec![
        Instruction::load_mem(R2, R1, 0, WORD),
        Instruction::alu32_imm(AND, R2, 0xffff),
        Instruction::load_mem(R4, R1, 4, WORD),
        Instruction::load_mem(R5, R1, 8, WORD),
        Instruction::jump_imm(JNE, R2, DEV_CHAR, 4),
        Instruction::jump_imm(JNE, R4, 1, 3),
        Instruction::jump_imm(JNE, R5, 3, 2),
        Instruction::alu32_imm(MOV, R0, 1),
        Instruction::exit(),
        Instruction::alu32_imm(MOV, R0, 0),
        Instruction::exit(),
    ]
}

fn canonical(instructions: &[Instruction]) -> Vec<u8> {
    canonical_bytes(instructions).unwrap()
}

fn emit(rules: &[DeviceRule], nonce: u64, original: &[u8]) -> Vec<u8> {
    emit_owned(rules, nonce, original).unwrap()
}

/// The wrapper with instruction `index` replaced.
fn corrupt(raw: &[u8], index: usize, instruction: Instruction) -> Vec<u8> {
    let mut instructions = from_canonical(raw).unwrap();
    instructions[index] = instruction;
    canonical(&instructions)
}

/// Go: `TestFixtures_PassGateAndRoundTrip`. For the real runc, crun and systemd device filters,
/// the original passes the gate, reloads to the same bytes, is not mistaken for a wrapper, and
/// the kernel's dump of the Go daemon's wrapper around it is the wrapper emitted here, and strips
/// back to exactly the original.
#[test]
fn runtime_fixtures_pass_the_gate_and_round_trip() {
    for name in RUNTIME_FIXTURES {
        let original = read_fixture(&format!("{name}.orig"));
        let reloaded = read_fixture(&format!("{name}.reloaded"));
        let wrapped = read_fixture(&format!("{name}.wrapped"));

        check_wrappable(&from_canonical(&original).unwrap(), KNOWN).unwrap();
        assert_eq!(
            reloaded, original,
            "{name}: reloading the original changed its dump"
        );
        assert_eq!(parse_owned(&original).unwrap(), None, "{name}");
        assert_eq!(
            emit(&fixture_rules(), FIXTURE_NONCE, &original),
            wrapped,
            "{name}: the kernel's dump of the wrapper differs from the emitted wrapper"
        );
        let parsed = parse_owned(&wrapped).unwrap().unwrap();
        assert_eq!(parsed.original, original, "{name}");
        assert_eq!(parsed.nonce, FIXTURE_NONCE, "{name}");
    }
}

/// Go: `TestParseOwned_ValidatedBlockRoundTrips`.
#[test]
fn a_validated_block_round_trips() {
    let original = canonical(&allow_null_original());
    let rules = [
        rule("c", 10, 200, "rw"),
        rule("b", 8, 0, "rwm"),
        rule("c", 189, 3, "m"),
    ];
    let parsed = parse_owned(&emit(&rules, 42, &original)).unwrap().unwrap();
    assert_eq!(parsed.nonce, 42);
    assert_eq!(parsed.original, original);
    assert_eq!(
        emit(&parsed.rules, 42, &original),
        emit(&rules, 42, &original)
    );
}

/// Go: `TestParseOwned_AbsentOrMarkerless`.
#[test]
fn programs_without_a_header_are_opaque_originals() {
    // A program that starts with the very context loads the wrapper uses, but has no header, is
    // an opaque original.
    let starts_with_init = Program::new().layout(
        &[],
        &[Instruction::alu32_imm(MOV, R0, 1), Instruction::exit()],
    );
    let cases = [
        ("runtime original", allow_null_original()),
        ("starts with init", starts_with_init),
        (
            "starts with a non-magic dead store",
            vec![marker(7), marker(0), Instruction::exit()],
        ),
    ];
    for (name, instructions) in cases {
        assert_eq!(parse_owned(&canonical(&instructions)), Ok(None), "{name}");
    }
    assert_eq!(parse_owned(&read_fixture("runc.orig")), Ok(None));
}

/// The corruptions of `TestParseOwned_ConflictsNeverValidate` applied to `wrapped`, in the order
/// the Go golden lists them.
fn conflict_corpus(wrapped: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    let instructions = from_canonical(wrapped).unwrap();
    let block_len = marker_value(&instructions[HDR_BLOCK_LEN]).unwrap();
    let trailer_at = usize::try_from(block_len).unwrap();
    let first_block = HEADER_LEN + INIT_LEN;
    let len = u32::try_from(instructions.len()).unwrap();
    vec![
        (
            "header without trailer",
            corrupt(wrapped, trailer_at, marker(0)),
        ),
        (
            "trailer nonce differs",
            corrupt(wrapped, trailer_at + 2, marker(98)),
        ),
        (
            "wrong block_len",
            corrupt(wrapped, HDR_BLOCK_LEN, marker(block_len + 1)),
        ),
        (
            "block_len past the end",
            corrupt(wrapped, HDR_BLOCK_LEN, marker(len)),
        ),
        (
            "block_len below the minimum",
            corrupt(wrapped, HDR_BLOCK_LEN, marker(10)),
        ),
        ("wrong version", corrupt(wrapped, HDR_VERSION, marker(2))),
        ("wrong magic_hi", corrupt(wrapped, HDR_MAGIC_HI, marker(0))),
        (
            "digest mismatch",
            corrupt(wrapped, instructions.len() - 2, marker(1)),
        ),
        (
            "header field not marker",
            corrupt(wrapped, HDR_NONCE_HI, Instruction::alu32_imm(MOV, R1, 0)),
        ),
        // Parses as a rule block, but a jump no longer lands where the builder puts it.
        (
            "non-regenerable block",
            corrupt(
                wrapped,
                first_block,
                Instruction::jump_imm(JNE, R2, DEV_CHAR, 1),
            ),
        ),
        (
            "unknown device type",
            corrupt(wrapped, first_block, Instruction::jump_imm(JNE, R2, 3, 4)),
        ),
        (
            "unparseable block",
            corrupt(wrapped, first_block, Instruction::alu32_imm(MOV, 7, 0)),
        ),
        (
            "truncated header",
            canonical(&[marker(MAGIC_LO), marker(0), Instruction::exit()]),
        ),
    ]
}

/// Go: `TestParseOwned_ConflictsNeverValidate`.
#[test]
fn conflicts_never_validate() {
    let original = canonical(&allow_null_original());
    let wrapped = emit(&[rule("c", 10, 200, "rwm")], 99, &original);
    for (name, raw) in conflict_corpus(&wrapped) {
        let error = parse_owned(&raw).unwrap_err();
        assert!(error.is_conflict(), "{name}: {error}");
    }
}

/// `TestCheckWrappable_Subset`'s programs, in the order the Go golden lists them.
fn gate_corpus() -> Vec<(&'static str, Vec<Instruction>, ProgMeta)> {
    let with_tail = |instruction: Instruction| {
        vec![
            Instruction::alu32_imm(MOV, R0, 0),
            instruction,
            Instruction::exit(),
        ]
    };
    let unknown = ProgMeta::default();
    let one_map = ProgMeta {
        map_ids_known: true,
        map_count: 1,
    };
    vec![
        ("minimal original", allow_null_original(), KNOWN),
        ("map IDs unknown", allow_null_original(), unknown),
        ("uses a map", allow_null_original(), one_map),
        ("empty", Vec::new(), KNOWN),
        (
            "helper call",
            // FnGetCurrentPidTgid
            with_tail(Instruction {
                opcode: OpCode::jump(CALL, IMM_SOURCE),
                constant: 14,
                ..Instruction::default()
            }),
            KNOWN,
        ),
        (
            "64-bit immediate",
            with_tail(Instruction {
                opcode: OpCode::load(MODE_IMM, DWORD),
                dst: R3,
                constant: 1 << 40,
                ..Instruction::default()
            }),
            KNOWN,
        ),
        (
            "stack store",
            with_tail(Instruction {
                opcode: OpCode::store_x(MODE_MEM, WORD),
                dst: RFP,
                src: R2,
                offset: -8,
                constant: 0,
            }),
            KNOWN,
        ),
        (
            "load from R2",
            with_tail(Instruction::load_mem(R3, R2, 0, WORD)),
            KNOWN,
        ),
        (
            "ctx load at 12",
            with_tail(Instruction::load_mem(R3, R1, 12, WORD)),
            KNOWN,
        ),
        (
            "alu add",
            with_tail(Instruction {
                opcode: OpCode::alu64(ADD, IMM_SOURCE),
                dst: R3,
                constant: 1,
                ..Instruction::default()
            }),
            KNOWN,
        ),
        (
            "sign-extending move",
            with_tail(Instruction {
                opcode: OpCode::alu64(MOV, REG_SOURCE),
                dst: R3,
                src: R2,
                offset: 8,
                constant: 0,
            }),
            KNOWN,
        ),
        (
            "jump out of program",
            with_tail(Instruction {
                opcode: OpCode::jump(JA, IMM_SOURCE),
                offset: 5,
                ..Instruction::default()
            }),
            KNOWN,
        ),
        (
            "jump before start",
            with_tail(Instruction::jump_imm(JEQ, R2, 0, -5)),
            KNOWN,
        ),
    ]
}

/// Go: `TestCheckWrappable_Subset`.
#[test]
fn the_gate_admits_only_the_subset() {
    for (name, instructions, meta) in gate_corpus() {
        let verdict = check_wrappable(&instructions, meta);
        if name == "minimal original" {
            assert_eq!(verdict, Ok(()), "{name}");
        } else {
            assert!(verdict.is_err(), "{name}");
        }
    }
}

/// Go: `TestWrapper_Verdicts`.
#[test]
fn wrapper_verdicts() {
    let original = canonical(&allow_null_original());
    let wrapped = emit(
        &[rule("c", 10, 200, "rwm"), rule("c", 10, 201, "r")],
        1,
        &original,
    );
    let cases = [
        (
            "exact device allowed",
            DevRequest::char(10, 200, READ | WRITE),
            1,
        ),
        ("other minor denied", DevRequest::char(10, 202, READ), 0),
        ("other type denied", DevRequest::block(10, 200, READ), 0),
        ("r rule allows r", DevRequest::char(10, 201, READ), 1),
        (
            "r rule with rw request denied",
            DevRequest::char(10, 201, READ | WRITE),
            0,
        ),
        (
            "fall-through to original allows",
            DevRequest::char(1, 3, READ | WRITE),
            1,
        ),
        (
            "fall-through to original denies",
            DevRequest::char(1, 5, READ),
            0,
        ),
    ];
    for (name, request, want) in cases {
        assert_eq!(run_device_filter(&wrapped, request), want, "{name}");
    }

    // The same holds on top of a real runtime filter: /dev/null (c 1:3) is in every runtime's
    // default set, and the wrapper keeps it.
    let runc = read_fixture("runc.orig");
    let runc_wrapped = emit(&[rule("c", 10, 250, "rwm")], 1, &runc);
    assert_eq!(
        run_device_filter(&runc_wrapped, DevRequest::char(1, 3, READ | WRITE)),
        1
    );
    assert_eq!(
        run_device_filter(&runc_wrapped, DevRequest::char(10, 250, READ)),
        1
    );
    // The original must deny c 10:250 for this test to mean anything.
    assert_eq!(run_device_filter(&runc, DevRequest::char(10, 250, READ)), 0);
}

/// The rules are appended last to first, so of two rules for the same device the later one is
/// tried first and decides, as a later OCI device rule overrides an earlier one.
#[test]
fn the_last_matching_rule_decides() {
    let original = canonical(&allow_null_original());
    let request = DevRequest::char(10, 200, READ);
    let allow_last = [
        rule_of(false, "c", 10, 200, "rwm"),
        rule("c", 10, 200, "rwm"),
    ];
    assert_eq!(
        run_device_filter(&emit(&allow_last, 1, &original), request),
        1
    );
    let deny_last = [
        rule("c", 10, 200, "rwm"),
        rule_of(false, "c", 10, 200, "rwm"),
    ];
    assert_eq!(
        run_device_filter(&emit(&deny_last, 1, &original), request),
        0
    );
}

#[test]
fn nonces_are_random() {
    assert_ne!(new_nonce().unwrap(), new_nonce().unwrap());
}

/// What `parse_owned` makes of `raw`, in the golden's form.
fn parse_result(raw: &[u8], original: &[u8]) -> ParseGolden {
    match parse_owned(raw) {
        Ok(Some(parsed)) => ParseGolden {
            owned: true,
            nonce: format!("{:016x}", parsed.nonce),
            rules: Some(parsed.rules).filter(|rules| !rules.is_empty()),
            original_is_fixture: parsed.original == original,
            error: None,
        },
        Ok(None) => ParseGolden::default(),
        Err(error) => ParseGolden {
            error: Some(error.to_string()),
            ..ParseGolden::default()
        },
    }
}

fn gate_text(raw: &[u8]) -> Option<String> {
    let instructions = from_canonical(raw).unwrap();
    check_wrappable(&instructions, KNOWN)
        .err()
        .map(|error| error.to_string())
}

/// For each runtime fixture, everything the Go code made of it: its canonical bytes, gate
/// verdict and parse, the parse of the kernel's dump of its wrapper, the wrapper emitted for each
/// rule set and nonce of the corpus (or the error that refused it) with its gate verdict and
/// parse, and the parses of the corrupted wrappers.
#[test]
fn runtime_fixtures_match_the_go_golden() {
    let golden = Golden::load();
    let names: Vec<&str> = golden
        .fixtures
        .iter()
        .map(|fixture| fixture.name.as_str())
        .collect();
    assert_eq!(names, RUNTIME_FIXTURES);
    for fixture in &golden.fixtures {
        let name = &fixture.name;
        let original = read_fixture(&format!("{name}.orig"));
        let wrapped = read_fixture(&format!("{name}.wrapped"));
        let instructions = from_canonical(&original).unwrap();
        assert_eq!(
            hex_lines(&canonical(&instructions)),
            fixture.canonical,
            "{name}"
        );
        assert_eq!(gate_text(&original), fixture.gate, "{name}");
        assert_eq!(
            parse_result(&original, &original),
            fixture.orig_parse,
            "{name}"
        );
        assert_eq!(
            parse_result(&wrapped, &original),
            fixture.wrapped_parse,
            "{name}"
        );

        for case in &fixture.wrappers {
            let label = format!("{name}: {}", case.case);
            let nonce = u64::from_str_radix(&case.nonce, 16).unwrap();
            let rules = case.rules.as_deref().unwrap_or_default();
            match emit_owned(rules, nonce, &original) {
                Ok(raw) => {
                    assert_eq!(case.error, None, "{label}");
                    assert_eq!(Some(hex_lines(&raw)), case.wrapper, "{label}");
                    assert_eq!(gate_text(&raw), case.gate, "{label}");
                    assert_eq!(Some(parse_result(&raw, &original)), case.parse, "{label}");
                }
                Err(error) => {
                    assert_eq!(Some(error.to_string()), case.error, "{label}");
                    assert_eq!(case.wrapper, None, "{label}");
                }
            }
        }

        let first = emit(&fixture_rules(), FIXTURE_NONCE, &original);
        let corpus = conflict_corpus(&first);
        assert_eq!(corpus.len(), fixture.conflicts.len(), "{name}");
        for ((case, raw), golden) in corpus.iter().zip(&fixture.conflicts) {
            let label = format!("{name}: {case}");
            assert_eq!(*case, golden.case, "{label}");
            assert_eq!(hex_lines(raw), golden.program, "{label}");
            assert_eq!(parse_result(raw, &original), golden.parse, "{label}");
            assert_eq!(unhex(&golden.program.concat()), *raw, "{label}");
        }
    }
}

/// The gate's exact verdicts on `TestCheckWrappable_Subset`'s programs, built here as Go built
/// them (a sign-extending move keeps its offset only until it is encoded).
#[test]
fn the_gate_matches_the_go_golden() {
    let golden = Golden::load();
    let corpus = gate_corpus();
    assert_eq!(corpus.len(), golden.gate.len());
    for ((name, instructions, meta), case) in corpus.iter().zip(&golden.gate) {
        assert_eq!(*name, case.case);
        assert_eq!(hex_lines(&canonical(instructions)), case.program, "{name}");
        assert_eq!(
            (meta.map_ids_known, meta.map_count),
            (case.map_ids_known, case.map_count),
            "{name}"
        );
        let verdict = check_wrappable(instructions, *meta)
            .err()
            .map(|error| error.to_string());
        assert_eq!(verdict, case.verdict, "{name}");
    }
}
