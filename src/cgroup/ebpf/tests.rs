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

use super::{Program, RuleError, prepend_device_filter};
use crate::cgroup::api::DeviceRule;
use crate::cgroup::asm::{Instruction, MOV, R0};
use crate::cgroup::owned::{canonical_bytes, from_canonical};
use crate::cgroup::testing::{Golden, hex_lines, rule, rule_of};

fn no_number(major: Option<i64>, minor: Option<i64>) -> DeviceRule {
    DeviceRule {
        major,
        minor,
        ..rule("c", 0, 0, "rwm")
    }
}

/// Go: `TestAppendDevice_CharDevice_RWM`.
#[test]
fn char_device_rwm_has_type_major_and_minor_checks() {
    let mut program = Program::new();
    program.append_device(&rule("c", 195, 0, "rwm")).unwrap();
    // The type, major and minor checks and the verdict (rwm skips the access check) after the 5
    // context loads.
    assert_eq!(program.instruction_count(), 10);
}

/// Go: `TestAppendDevice_BlockDevice_ReadOnly`.
#[test]
fn block_device_read_only_adds_the_access_check() {
    let mut program = Program::new();
    program.append_device(&rule("b", 8, 1, "r")).unwrap();
    // The type check, the three access instructions, the major and minor checks and the
    // verdict.
    assert_eq!(program.instruction_count(), 13);
}

/// Go: `TestAppendDevice_Wildcard_SetsFlag`.
#[test]
fn a_wildcard_makes_later_appends_no_ops() {
    let mut program = Program::new();
    program.append_device(&rule("a", -1, -1, "rwm")).unwrap();
    assert!(program.has_wild_card());
    let before = program.instruction_count();
    program.append_device(&rule("c", 1, 0, "rwm")).unwrap();
    assert_eq!(program.instruction_count(), before);
}

/// Go: `TestAppendDevice_NoMajorNoMinor`.
#[test]
fn wildcard_numbers_have_no_number_checks() {
    let mut program = Program::new();
    program.append_device(&rule("c", -1, -1, "rwm")).unwrap();
    assert_eq!(program.instruction_count(), 8);
}

/// Go: `TestAppendDevice_NilMajor`.
#[test]
fn a_rule_without_a_major_is_refused() {
    let error = Program::new()
        .append_device(&no_number(None, Some(0)))
        .unwrap_err();
    assert_eq!(error, RuleError::NoMajor);
}

/// Go: `TestAppendDevice_NilMinor`.
#[test]
fn a_rule_without_a_minor_is_refused() {
    let error = Program::new()
        .append_device(&no_number(Some(1), None))
        .unwrap_err();
    assert_eq!(error, RuleError::NoMinor);
}

/// Go: `TestAppendDevice_Deny`.
#[test]
fn a_deny_rule_has_the_shape_of_an_allow_rule() {
    let mut program = Program::new();
    program
        .append_device(&rule_of(false, "c", 1, 3, "rwm"))
        .unwrap();
    assert_eq!(program.instruction_count(), 10);
}

/// Go: `TestAppendDevice_InvalidType`.
#[test]
fn an_invalid_type_is_refused() {
    let error = Program::new()
        .append_device(&rule("x", 1, 0, "rwm"))
        .unwrap_err();
    assert_eq!(error.to_string(), r#"invalid DeviceType "x""#);
}

/// Go: `TestAppendDevice_MajorOverflow`.
#[test]
fn a_major_above_32_bits_is_refused() {
    let error = Program::new()
        .append_device(&rule("c", 1 << 32, 0, "rwm"))
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid major 4294967296");
}

/// Go: `TestAppendDevice_MinorOverflow`: the error names the minor, not the major.
#[test]
fn a_minor_above_32_bits_is_refused() {
    let error = Program::new()
        .append_device(&rule("c", 1, 1 << 32, "rwm"))
        .unwrap_err();
    assert_eq!(error.to_string(), "invalid minor 4294967296");
}

/// Go: `TestAppendDevice_AfterFinalize`.
#[test]
fn appending_after_finalize_is_refused() {
    let mut program = Program::new();
    program.append_device(&rule("c", 1, 0, "rwm")).unwrap();
    program.finalize(&[Instruction::exit()]);
    let error = program.append_device(&rule("c", 1, 0, "rwm")).unwrap_err();
    assert_eq!(error.to_string(), "the program is finalized");
}

/// Go: `TestPrependDeviceFilter_RoundTrip`, which only checked for a non-empty result: here the
/// result also encodes and decodes back to itself, ending with the original.
#[test]
fn prepend_device_filter_ends_with_the_original() {
    let original = [Instruction::alu32_imm(MOV, R0, 0), Instruction::exit()];
    let rules = [rule("c", 195, 0, "rwm"), rule("b", 8, 0, "rwm")];
    let program = prepend_device_filter(&rules, &original).unwrap();
    assert!(program.ends_with(&original));
    let raw = canonical_bytes(&program).unwrap();
    assert_eq!(from_canonical(&raw).unwrap(), program);

    let error = prepend_device_filter(&rules, &[]).unwrap_err();
    assert_eq!(error, RuleError::NoOriginal);
}

/// The Go builder's output for `ebpf_test.go`'s cases, appended in order and finalized over a
/// deny-all original: each append's error, the wildcard flag, the encoded program with its jumps
/// resolved, and the error of one more append.
#[test]
fn the_builder_matches_the_go_golden() {
    let golden = Golden::load();
    assert!(!golden.builder.is_empty());
    for case in &golden.builder {
        let name = &case.case;
        if name == "no original" {
            let error = prepend_device_filter(&[rule("c", 1, 3, "rwm")], &[]).unwrap_err();
            assert_eq!(Some(error.to_string()), case.error, "{name}");
            continue;
        }
        let mut program = Program::new();
        let mut first_error = None;
        for append in &case.appends {
            let error = program
                .append_device(&append.rule)
                .err()
                .map(|error| error.to_string());
            assert_eq!(error, append.error, "{name}: {:?}", append.rule);
            first_error = first_error.or(error);
        }
        assert_eq!(first_error, case.error, "{name}");
        assert_eq!(program.has_wild_card(), case.has_wild_card, "{name}");
        if first_error.is_some() {
            assert!(case.program.is_empty(), "{name}");
            continue;
        }
        let original =
            from_canonical(&crate::cgroup::testing::unhex(&case.original.concat())).unwrap();
        let built = canonical_bytes(&program.finalize(&original)).unwrap();
        assert_eq!(hex_lines(&built), case.program, "{name}");
        let after = program
            .append_device(&rule("c", 1, 0, "rwm"))
            .err()
            .map(|error| error.to_string());
        assert_eq!(after, case.after_finalize, "{name}");
    }
}
