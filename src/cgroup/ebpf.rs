/*
 * Copyright (c) 2021, NVIDIA CORPORATION.  All rights reserved.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

// The implementation of the device filter eBPF program in this file is based on:
// https://github.com/containers/crun/blob/0.10.2/src/libcrun/ebpf.c
//
// Although ebpf.c is originally licensed under LGPL-3.0-or-later, the author (Giuseppe Scrivano)
// agreed to relicense the file in Apache License 2.0: https://github.com/opencontainers/runc/issues/2144#issuecomment-543116397
//
// Much of the go code in this file is borrowed heavily from the (Apache licensed) file found here:
// https://github.com/opencontainers/runc/blob/8e7ab26104352f4214ab1daec5c1d4bf75eddb54/libcontainer/cgroups/ebpf/devicefilter/devicefilter.go

// Modified by Roberto Leinardi in 2026: ported to Rust from internal/cgroup/ebpf.go.

//! The device filter program builder: the context loads, one block per device rule, appended
//! from the last rule to the first, and the jumps that send a request that does not match a
//! block to the next one. `owned.rs` wraps a runtime's original program with it.
//!
//! Go labels each block with a random prefix and lets cilium resolve the references when it
//! encodes the program. Here a block's jumps are resolved when the program is laid out, to the
//! first instruction of the next block or, for the last block, of what follows the rule blocks;
//! the offsets are the ones cilium computes, so no label (and no random prefix) is needed.
//!
//! The Go file's kernel calls (`FindAttachedCgroupDeviceFilters`, the attach and the detach) are
//! in `bpf_sys.rs` and the cgroup v2 controller.

use std::fmt;

use super::api::DeviceRule;
use super::asm::{
    AND, HALF, Instruction, JNE, MOV, R0, R1, R2, R3, R4, R5, R6, RSH, WORD, low_i32,
};
use crate::go_compat::quote::quote;

/// `BPF_DEVCG_DEV_BLOCK`.
pub(super) const DEV_BLOCK: i32 = 1;
/// `BPF_DEVCG_DEV_CHAR`.
pub(super) const DEV_CHAR: i32 = 2;
/// `BPF_DEVCG_ACC_MKNOD`.
pub(super) const ACC_MKNOD: i32 = 1;
/// `BPF_DEVCG_ACC_READ`.
pub(super) const ACC_READ: i32 = 2;
/// `BPF_DEVCG_ACC_WRITE`.
pub(super) const ACC_WRITE: i32 = 4;

/// The number of instructions the context loads take.
pub(super) const INIT_LEN: usize = 5;

/// One instruction of a rule block, and whether it is a jump to the next block.
#[derive(Debug, Clone, Copy)]
struct Step {
    instruction: Instruction,
    to_next_block: bool,
}

impl Step {
    const fn plain(instruction: Instruction) -> Self {
        Self {
            instruction,
            to_next_block: false,
        }
    }

    const fn jump(instruction: Instruction) -> Self {
        Self {
            instruction,
            to_next_block: true,
        }
    }
}

/// A device filter program being built: the context loads and the rule blocks.
#[derive(Debug)]
pub struct Program {
    init: [Instruction; INIT_LEN],
    blocks: Vec<Vec<Step>>,
    has_wild_card: bool,
    finalized: bool,
}

impl Default for Program {
    fn default() -> Self {
        Self::new()
    }
}

impl Program {
    /// A program with the context loads of `struct bpf_cgroup_dev_ctx` (`u32 access_type`,
    /// `u32 major`, `u32 minor`) and no rule.
    #[must_use]
    pub fn new() -> Self {
        Self {
            init: [
                // R2 <- type (lower 16 bit of u32 access_type at R1[0])
                Instruction::load_mem(R2, R1, 0, HALF),
                // R3 <- access (upper 16 bit of u32 access_type at R1[0])
                Instruction::load_mem(R3, R1, 0, WORD),
                Instruction::alu32_imm(RSH, R3, 16),
                // R4 <- major (u32 major at R1[4])
                Instruction::load_mem(R4, R1, 4, WORD),
                // R5 <- minor (u32 minor at R1[8])
                Instruction::load_mem(R5, R1, 8, WORD),
            ],
            blocks: Vec::new(),
            has_wild_card: false,
            finalized: false,
        }
    }

    /// The number of instructions so far.
    #[must_use]
    pub fn instruction_count(&self) -> usize {
        INIT_LEN + self.blocks.iter().map(Vec::len).sum::<usize>()
    }

    /// Whether a rule matching every request has been appended, after which appending does
    /// nothing.
    #[must_use]
    pub const fn has_wild_card(&self) -> bool {
        self.has_wild_card
    }

    /// Appends the block of `rule`. The rules of a set are appended from the last to the first,
    /// so the first rule's block is the last one tried.
    ///
    /// # Errors
    ///
    /// [`RuleError`] after [`Program::finalize`], or for a rule the program cannot express.
    pub fn append_device(&mut self, rule: &DeviceRule) -> Result<(), RuleError> {
        if self.finalized {
            return Err(RuleError::Finalized);
        }
        if self.has_wild_card {
            // All entries after the wildcard entry are ignored.
            return Ok(());
        }
        let device_type = match rule.kind.as_str() {
            "c" => Some(DEV_CHAR),
            "b" => Some(DEV_BLOCK),
            "a" => None,
            _ => return Err(RuleError::InvalidType(rule.kind.clone())),
        };
        let major = rule.major.ok_or(RuleError::NoMajor)?;
        let minor = rule.minor.ok_or(RuleError::NoMinor)?;
        if major > i64::from(u32::MAX) {
            return Err(RuleError::InvalidMajor(major));
        }
        if minor > i64::from(u32::MAX) {
            return Err(RuleError::InvalidMinor(minor));
        }
        // A negative number is the wildcard.
        let (has_major, has_minor) = (major >= 0, minor >= 0);
        let mut access = 0;
        for letter in rule.access.chars() {
            access |= match letter {
                'r' => ACC_READ,
                'w' => ACC_WRITE,
                'm' => ACC_MKNOD,
                _ => return Err(RuleError::UnknownAccess(letter)),
            };
        }
        // rwm needs no access check.
        let has_access = access != ACC_READ | ACC_WRITE | ACC_MKNOD;

        let mut block = Vec::new();
        if let Some(device_type) = device_type {
            // if (R2 != type) goto next
            block.push(Step::jump(Instruction::jump_imm(JNE, R2, device_type, -1)));
        }
        if has_access {
            // R2 holds the device type for the later blocks' type checks, so R6 is the scratch
            // register, and the check is containment (R3 & access == R3), NVIDIA's: an overlap
            // check would grant a request for more access than the rule allows.
            // if (R3 & access != R3) goto next
            block.push(Step::plain(Instruction::alu32_reg(MOV, R6, R3)));
            block.push(Step::plain(Instruction::alu32_imm(AND, R6, access)));
            block.push(Step::jump(Instruction::jump_reg(JNE, R6, R3, -1)));
        }
        if has_major {
            // if (R4 != major) goto next
            block.push(Step::jump(Instruction::jump_imm(
                JNE,
                R4,
                low_i32(major),
                -1,
            )));
        }
        if has_minor {
            // if (R5 != minor) goto next
            block.push(Step::jump(Instruction::jump_imm(
                JNE,
                R5,
                low_i32(minor),
                -1,
            )));
        }
        if device_type.is_none() && !has_access && !has_major && !has_minor {
            self.has_wild_card = true;
        }
        // R0 <- verdict, then return it.
        block.push(Step::plain(Instruction::alu32_imm(
            MOV,
            R0,
            i32::from(rule.allow),
        )));
        block.push(Step::plain(Instruction::exit()));
        self.blocks.push(block);
        Ok(())
    }

    /// The program followed by `original`, the last block falling through to it. No rule can be
    /// appended afterwards.
    pub fn finalize(&mut self, original: &[Instruction]) -> Vec<Instruction> {
        self.finalized = true;
        self.layout(&[], original)
    }

    /// `head`, the program and `tail`, with every block's jumps resolved to the first
    /// instruction of the next block, and the last block's to the first one of `tail`: the
    /// offsets cilium computes from the labels Go puts there, in raw slots.
    pub(super) fn layout(&self, head: &[Instruction], tail: &[Instruction]) -> Vec<Instruction> {
        let mut laid_out: Vec<Instruction> = head.iter().chain(&self.init).copied().collect();
        let mut position: usize = laid_out.iter().map(Instruction::width).sum();
        let mut next_starts = Vec::with_capacity(self.blocks.len());
        let mut cursor = position;
        for block in &self.blocks {
            cursor += block
                .iter()
                .map(|step| step.instruction.width())
                .sum::<usize>();
            next_starts.push(cursor);
        }
        for (block, next_start) in self.blocks.iter().zip(next_starts) {
            for step in block {
                let mut instruction = step.instruction;
                if step.to_next_block {
                    instruction.offset = jump_offset(next_start, position);
                }
                position += instruction.width();
                laid_out.push(instruction);
            }
        }
        laid_out.extend_from_slice(tail);
        laid_out
    }
}

/// cilium's `int16(target - position - 1)`, truncated as Go converts.
fn jump_offset(target: usize, position: usize) -> i16 {
    let [low, high, ..] = target.wrapping_sub(position).wrapping_sub(1).to_le_bytes();
    i16::from_le_bytes([low, high])
}

/// `PrependDeviceFilter`: the blocks of `rules` in front of the `original` program, which
/// handles every request no rule matches. Go returns the instructions with their labels, for
/// cilium to resolve when it encodes them; these are resolved already.
///
/// # Errors
///
/// [`RuleError`] when `original` is empty, or for a rule the program cannot express.
pub fn prepend_device_filter(
    rules: &[DeviceRule],
    original: &[Instruction],
) -> Result<Vec<Instruction>, RuleError> {
    // A bare exit at the end leaves R0 unset on fall-through, which the verifier rejects, and
    // there is no safe default verdict to invent here: the original program is what denies
    // everything not granted.
    if original.is_empty() {
        return Err(RuleError::NoOriginal);
    }
    let mut program = Program::new();
    for rule in rules.iter().rev() {
        program.append_device(rule)?;
    }
    Ok(program.finalize(original))
}

/// Why a rule cannot be added to a device filter program, worded as the Go daemon's errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleError {
    /// The program has been finalized.
    Finalized,
    /// A type other than `c`, `b` and `a`.
    InvalidType(String),
    /// The rule has no major number.
    NoMajor,
    /// The rule has no minor number.
    NoMinor,
    /// A major number above `u32::MAX`.
    InvalidMajor(i64),
    /// A minor number above `u32::MAX`.
    InvalidMinor(i64),
    /// An access letter other than `r`, `w` and `m`.
    UnknownAccess(char),
    /// There is no original program to put the rules in front of.
    NoOriginal,
}

impl fmt::Display for RuleError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Finalized => out.write_str("the program is finalized"),
            Self::InvalidType(kind) => write!(out, "invalid DeviceType {}", quote(kind)),
            Self::NoMajor => out.write_str("no major set in device rule"),
            Self::NoMinor => out.write_str("no minor set in device rule"),
            Self::InvalidMajor(major) => write!(out, "invalid major {major}"),
            Self::InvalidMinor(minor) => write!(out, "invalid minor {minor}"),
            // Go prints the rune with %v, as its number.
            Self::UnknownAccess(letter) => {
                write!(out, "unknown device access {}", u32::from(*letter))
            }
            Self::NoOriginal => out.write_str("no original device filter program to wrap"),
        }
    }
}

impl std::error::Error for RuleError {}

#[cfg(test)]
mod tests;
