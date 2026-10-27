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

use std::collections::BTreeSet;

use super::{DecodeCause, DecodeError, Instruction, decode};
use crate::cgroup::owned::{ProgMeta, canonical_bytes, check_wrappable};
use crate::cgroup::testing::{hex, unhex};
use crate::test_support::testdata;

/// How cilium decodes, prints and re-encodes one instruction and what the gate says of it, for
/// every opcode byte with register, offset and immediate variants that reach each branch, a
/// `ld_imm64` with each kind of second half, and truncated input: the rows of
/// `tests/testdata/devfilter/asm.tsv`, written by the Go `TestGoldenAsm`.
#[test]
fn every_opcode_matches_the_go_golden() {
    let tsv = String::from_utf8(testdata("devfilter/asm.tsv")).unwrap();
    let mut rows = tsv.lines();
    assert_eq!(rows.next(), Some("raw\tresult\ttext\tcanonical\tgate"));
    let meta = ProgMeta {
        map_ids_known: true,
        map_count: 0,
    };
    let mut opcodes = BTreeSet::new();
    for row in rows {
        let raw = unhex(row.split('\t').next().unwrap());
        opcodes.insert(raw[0]);
        let got = match decode(&raw) {
            Err(error) => format!("{}\tdecode error\t{error}\t-\t-", hex(&raw)),
            Ok(instructions) => {
                let text: Vec<String> = instructions.iter().map(ToString::to_string).collect();
                let canonical = canonical_bytes(&instructions).map_or_else(
                    |error| format!("encode error: {error}"),
                    |encoded| hex(&encoded),
                );
                let gate = check_wrappable(&instructions, meta)
                    .map_or_else(|error| error.to_string(), |()| "-".to_owned());
                format!(
                    "{}\tok\t{}\t{canonical}\t{gate}",
                    hex(&raw),
                    text.join("; ")
                )
            }
        };
        assert_eq!(got, row);
    }
    assert_eq!(opcodes.len(), 256);
}

#[test]
fn empty_input_is_no_instruction() {
    assert_eq!(decode(&[]), Ok(Vec::new()));
}

/// The offset of a decode error is in bytes, a `ld_imm64` counting 16.
#[test]
fn decode_errors_name_the_byte_offset() {
    let mut raw = vec![0x18, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    raw.extend_from_slice(&[0xb4, 0, 0, 0]);
    assert_eq!(
        decode(&raw),
        Err(DecodeError {
            offset: 16,
            cause: DecodeCause::UnexpectedEof
        })
    );
    assert_eq!(
        decode(&raw).unwrap_err().to_string(),
        "decode instructions: offset 16: unexpected EOF"
    );
}

/// A `ld_imm64` keeps its 64-bit immediate and encodes back to its two slots.
#[test]
fn a_wide_load_round_trips() {
    let raw = unhex("1803000078563412000000000ff0ffff");
    let instructions = decode(&raw).unwrap();
    assert_eq!(
        instructions,
        [Instruction {
            opcode: super::OpCode(0x18),
            dst: 3,
            constant: 0xffff_f00f_1234_5678_u64.cast_signed(),
            ..Instruction::default()
        }]
    );
    assert_eq!(instructions[0].width(), 2);
    assert_eq!(canonical_bytes(&instructions).unwrap(), raw);
}
