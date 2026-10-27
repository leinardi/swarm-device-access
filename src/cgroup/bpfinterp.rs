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

//! An interpreter for the eBPF subset device filters use (context loads, 32 and 64-bit mov, and,
//! or, left and right shifts, conditional jumps, ja and exit), so that tests check a program's
//! verdicts rather than count its instructions: counting cannot catch a wrong access check.
//!
//! Go: `bpfinterp_test.go`.

use super::asm::{
    self, AND, BYTE, Class, EXIT, HALF, JA, JEQ, JGE, JGT, JLE, JLT, JNE, JSET, LSH, MOV, OR, R0,
    R1, REG_SOURCE, RSH, WORD,
};
use super::ebpf::{ACC_READ, ACC_WRITE, DEV_BLOCK, DEV_CHAR};

/// Read access.
pub(super) const READ: u32 = ACC_READ.cast_unsigned();
/// Write access.
pub(super) const WRITE: u32 = ACC_WRITE.cast_unsigned();

/// A `bpf_cgroup_dev_ctx`: the access a process asks for.
#[derive(Debug, Clone, Copy)]
pub(super) struct DevRequest {
    /// `BPF_DEVCG_DEV_CHAR` or `BPF_DEVCG_DEV_BLOCK`.
    pub kind: u32,
    /// A `BPF_DEVCG_ACC_*` mask.
    pub access: u32,
    pub major: u32,
    pub minor: u32,
}

impl DevRequest {
    /// A request for the char device `major:minor`.
    pub(super) const fn char(major: u32, minor: u32, access: u32) -> Self {
        Self {
            kind: DEV_CHAR.cast_unsigned(),
            access,
            major,
            minor,
        }
    }

    /// A request for the block device `major:minor`.
    pub(super) const fn block(major: u32, minor: u32, access: u32) -> Self {
        Self {
            kind: DEV_BLOCK.cast_unsigned(),
            access,
            major,
            minor,
        }
    }

    fn context(self) -> Vec<u8> {
        [(self.access << 16) | self.kind, self.major, self.minor]
            .iter()
            .flat_map(|word| word.to_le_bytes())
            .collect()
    }
}

/// The verdict of the program `raw` for `request`: 1 allows, 0 denies. Panics on anything
/// outside the subset, or on a program that leaves its bounds.
pub(super) fn run_device_filter(raw: &[u8], request: DevRequest) -> u64 {
    let instructions = asm::decode(raw).unwrap();
    let context = request.context();
    let mut registers = [0_u64; 11];
    let mut pc: i64 = 0;
    let len = i64::try_from(instructions.len()).unwrap();
    for _ in 0..=10 * instructions.len() {
        assert!((0..len).contains(&pc), "program left its bounds at pc {pc}");
        let instruction = instructions[usize::try_from(pc).unwrap()];
        let opcode = instruction.opcode;
        let dst = usize::from(instruction.dst);
        match opcode.class() {
            Class::LdX => {
                assert_eq!(
                    instruction.src, R1,
                    "pc {pc}: load from non-context register"
                );
                let at = &context[usize::try_from(instruction.offset).unwrap()..];
                registers[dst] = match opcode.size() {
                    WORD => u64::from(u32::from_le_bytes(at[..4].try_into().unwrap())),
                    HALF => u64::from(u16::from_le_bytes(at[..2].try_into().unwrap())),
                    BYTE => u64::from(at[0]),
                    _ => panic!("pc {pc}: unsupported load size"),
                };
            }
            Class::Alu | Class::Alu64 => {
                let alu32 = matches!(opcode.class(), Class::Alu);
                let source = if opcode.source() == REG_SOURCE {
                    registers[usize::from(instruction.src)]
                } else {
                    instruction.constant.cast_unsigned()
                };
                let mut value = registers[dst];
                match opcode.alu_op() {
                    MOV => value = source,
                    AND => value &= source,
                    OR => value |= source,
                    LSH => value <<= source & 63,
                    RSH if alu32 => value = u64::from(low_u32(value) >> (source & 31)),
                    RSH => value >>= source & 63,
                    _ => panic!("pc {pc}: unsupported ALU op {opcode}"),
                }
                if alu32 {
                    value = u64::from(low_u32(value));
                }
                registers[dst] = value;
            }
            Class::Jump | Class::Jump32 => match opcode.jump_op() {
                EXIT => return registers[usize::from(R0)],
                JA => pc += i64::from(instruction.offset),
                op => {
                    let (mut left, mut right) =
                        (registers[dst], instruction.constant.cast_unsigned());
                    if opcode.source() == REG_SOURCE {
                        right = registers[usize::from(instruction.src)];
                    }
                    if matches!(opcode.class(), Class::Jump32) {
                        (left, right) = (u64::from(low_u32(left)), u64::from(low_u32(right)));
                    }
                    let taken = match op {
                        JEQ => left == right,
                        JNE => left != right,
                        JGT => left > right,
                        JGE => left >= right,
                        JLT => left < right,
                        JLE => left <= right,
                        JSET => left & right != 0,
                        _ => panic!("pc {pc}: unsupported jump {opcode}"),
                    };
                    if taken {
                        pc += i64::from(instruction.offset);
                    }
                }
            },
            Class::Ld | Class::St | Class::StX => {
                panic!("pc {pc}: unsupported instruction class {opcode}")
            }
        }
        pc += 1;
    }
    panic!("program left its bounds at pc {pc}")
}

fn low_u32(value: u64) -> u32 {
    u32::try_from(value & u64::from(u32::MAX)).unwrap()
}
