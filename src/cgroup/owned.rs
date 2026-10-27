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

//! The daemon's own device filter: the runtime's original program wrapped as
//!
//! ```text
//! HEADER, init, rule blocks, TRAILER, original
//! ```
//!
//! HEADER and TRAILER are dead stores to R0 (R0 is written again before every exit), so they cost
//! nothing at run time but survive in the kernel's translated dump. The header carries
//! `magic_lo`, `magic_hi`, `format_version`, `nonce_lo`, `nonce_hi`, `digest_0` to `digest_3` and
//! `block_len`: a random 64-bit nonce per wrapper lineage, the first 128 bits of the SHA-256 of
//! the original's canonical bytes, and the instruction index of the trailer. The trailer repeats
//! `magic_lo`, `magic_hi`, `nonce_lo` and `nonce_hi`.
//!
//! Ownership is never inferred from a partial match: a program is owned only when the whole
//! wrapper regenerates byte for byte from the rules and nonce parsed out of it and the original
//! that follows it (which also checks the digest, the block length and the trailer). A program
//! that starts like a header but fails that is a conflict and is never modified. The encoding is
//! therefore the Go daemon's to the byte, so that the wrappers it attached validate here.
//!
//! The reloadability gate decides which originals may be wrapped at all.

use std::fmt;

use sha2::{Digest as _, Sha256};

use super::api::DeviceRule;
use super::asm::{
    self, AND, Class, DecodeError, EXIT, EncodeCause, IMM_SOURCE, Instruction, JA, JEQ, JGE, JGT,
    JLE, JLT, JNE, JSET, JSGE, JSGT, JSLE, JSLT, LSH, MODE_MEM, MOV, OR, OpCode, R0, R1, R2, R4,
    R5, R6, REG_SOURCE, RSH, Register, low_u32,
};
use super::ebpf::{
    ACC_MKNOD, ACC_READ, ACC_WRITE, DEV_BLOCK, DEV_CHAR, INIT_LEN, Program, RuleError,
};
use super::v1set::WILDCARD;

/// `"SDA_"` little-endian.
const MAGIC_LO: u32 = 0x5f41_4453;
/// `"DBLK"` little-endian.
const MAGIC_HI: u32 = 0x4b4c_4244;
const FORMAT_VERSION: u32 = 1;

const HEADER_LEN: usize = 10;
const TRAILER_LEN: usize = 4;

/// The name wrappers are loaded under, for `bpftool` users.
pub const OWNED_PROGRAM_NAME: &str = "sda_devfilter";

// Header field positions.
const HDR_MAGIC_HI: usize = 1;
const HDR_VERSION: usize = 2;
const HDR_NONCE_LO: usize = 3;
const HDR_NONCE_HI: usize = 4;
const HDR_DIGEST: usize = 5;
const HDR_BLOCK_LEN: usize = 9;

/// A validated wrapper taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedProgram {
    /// The wrapper lineage's nonce.
    pub nonce: u64,
    /// The rules the wrapper grants, in the order they were given.
    pub rules: Vec<DeviceRule>,
    /// The canonical bytes of the wrapped original.
    pub original: Vec<u8>,
}

/// Encodes `instructions` as raw kernel bytes, the form every comparison and digest uses.
///
/// # Errors
///
/// [`CanonicalError`] for an instruction cilium does not encode.
pub fn canonical_bytes(instructions: &[Instruction]) -> Result<Vec<u8>, CanonicalError> {
    let mut raw = Vec::with_capacity(instructions.len() * asm::INSTRUCTION_SIZE);
    for (index, instruction) in instructions.iter().enumerate() {
        instruction
            .encode(&mut raw)
            .map_err(|cause| CanonicalError { index, cause })?;
    }
    Ok(raw)
}

/// Decodes raw kernel bytes.
///
/// # Errors
///
/// [`DecodeError`] for bytes that are not whole instructions.
pub fn from_canonical(raw: &[u8]) -> Result<Vec<Instruction>, DecodeError> {
    asm::decode(raw)
}

/// The dead store of `value` the header and trailer are made of.
fn marker(value: u32) -> Instruction {
    Instruction::alu32_imm(MOV, R0, value.cast_signed())
}

/// The value of a dead-store marker.
fn marker_value(instruction: &Instruction) -> Option<u32> {
    (instruction.opcode == OpCode::alu32(MOV, IMM_SOURCE)
        && instruction.dst == R0
        && instruction.src == R0
        && instruction.offset == 0)
        .then_some(low_u32(instruction.constant))
}

/// The first 128 bits of the SHA-256 of `original`, as four little-endian words.
fn digest_of(original: &[u8]) -> [u32; 4] {
    let sum = Sha256::digest(original);
    let (words, _) = sum.as_chunks::<4>();
    let mut digest = [0; 4];
    for (word, bytes) in digest.iter_mut().zip(words) {
        *word = u32::from_le_bytes(*bytes);
    }
    digest
}

/// A random nonce for a new wrapper lineage.
///
/// # Errors
///
/// [`NonceError`] when the kernel's random source fails.
pub fn new_nonce() -> Result<u64, NonceError> {
    let mut raw = [0_u8; 8];
    let mut filled = 0;
    while let Some(rest) = raw.get_mut(filled..).filter(|rest| !rest.is_empty()) {
        match rustix::rand::getrandom(rest, rustix::rand::GetRandomFlags::empty()) {
            Ok(read) => filled += read,
            Err(rustix::io::Errno::INTR) => {}
            Err(errno) => return Err(NonceError(errno.into())),
        }
    }
    Ok(u64::from_le_bytes(raw))
}

/// Builds the wrapper for `rules` around the canonical `original`.
///
/// # Errors
///
/// [`EmitError`] when `original` does not decode or is empty, a rule cannot be expressed, or an
/// instruction does not encode.
pub fn emit_owned(rules: &[DeviceRule], nonce: u64, original: &[u8]) -> Result<Vec<u8>, EmitError> {
    let original_instructions = from_canonical(original).map_err(EmitError::Decode)?;
    if original_instructions.is_empty() {
        return Err(EmitError::Rule(RuleError::NoOriginal));
    }
    let mut body = Program::new();
    for rule in rules.iter().rev() {
        body.append_device(rule).map_err(EmitError::Rule)?;
    }
    let [lo_0, lo_1, lo_2, lo_3, hi_0, hi_1, hi_2, hi_3] = nonce.to_le_bytes();
    let nonce_lo = u32::from_le_bytes([lo_0, lo_1, lo_2, lo_3]);
    let nonce_hi = u32::from_le_bytes([hi_0, hi_1, hi_2, hi_3]);
    let digest = digest_of(original);
    // Go truncates the length to 32 bits; a program of 2^32 instructions does not fit in memory.
    let block_len = u32::try_from(HEADER_LEN + body.instruction_count()).unwrap_or(u32::MAX);
    let header = [
        MAGIC_LO,
        MAGIC_HI,
        FORMAT_VERSION,
        nonce_lo,
        nonce_hi,
        digest[0],
        digest[1],
        digest[2],
        digest[3],
        block_len,
    ]
    .map(marker);
    // The last rule block falls through to the trailer, which in turn falls through to the
    // original.
    let tail: Vec<Instruction> = [MAGIC_LO, MAGIC_HI, nonce_lo, nonce_hi]
        .map(marker)
        .into_iter()
        .chain(original_instructions)
        .collect();
    let all = body.layout(&header, &tail);
    let mut raw = Vec::with_capacity(all.len() * asm::INSTRUCTION_SIZE);
    for (index, instruction) in all.iter().enumerate() {
        instruction
            .encode(&mut raw)
            .map_err(|cause| EmitError::Encode { index, cause })?;
    }
    Ok(raw)
}

/// Takes a program's canonical bytes apart: `None` when it does not start like a header, which
/// makes it an opaque original whatever else it contains.
///
/// # Errors
///
/// [`OwnedError::Decode`] for bytes that are not whole instructions, and
/// [`OwnedError::Conflict`] for a program that starts like a header but does not validate
/// completely.
pub fn parse_owned(raw: &[u8]) -> Result<Option<OwnedProgram>, OwnedError> {
    let instructions = from_canonical(raw).map_err(OwnedError::Decode)?;
    let Some(first) = instructions.first() else {
        return Ok(None);
    };
    if marker_value(first) != Some(MAGIC_LO) {
        return Ok(None);
    }
    validate_owned(&instructions, raw)
        .map(Some)
        .map_err(OwnedError::Conflict)
}

fn validate_owned(instructions: &[Instruction], raw: &[u8]) -> Result<OwnedProgram, Conflict> {
    if instructions.len() < HEADER_LEN {
        return Err(Conflict::Header);
    }
    let mut header = [0; HEADER_LEN];
    for (index, (field, instruction)) in header.iter_mut().zip(instructions).enumerate() {
        *field = marker_value(instruction).ok_or(Conflict::HeaderField(index))?;
    }
    if header[HDR_MAGIC_HI] != MAGIC_HI || header[HDR_VERSION] != FORMAT_VERSION {
        return Err(Conflict::MagicOrVersion);
    }
    let block_len = usize::try_from(header[HDR_BLOCK_LEN]).unwrap_or(usize::MAX);
    if block_len < HEADER_LEN + INIT_LEN
        || block_len.saturating_add(TRAILER_LEN) >= instructions.len()
    {
        return Err(Conflict::BlockLen(header[HDR_BLOCK_LEN]));
    }
    let want = [
        MAGIC_LO,
        MAGIC_HI,
        header[HDR_NONCE_LO],
        header[HDR_NONCE_HI],
    ];
    let trailer = &instructions[block_len..block_len + TRAILER_LEN];
    if trailer
        .iter()
        .zip(want)
        .any(|(instruction, value)| marker_value(instruction) != Some(value))
    {
        return Err(Conflict::Trailer);
    }
    let rules = parse_rule_blocks(&instructions[HEADER_LEN + INIT_LEN..block_len])
        .map_err(Conflict::RuleBlock)?;
    let original =
        canonical_bytes(&instructions[block_len + TRAILER_LEN..]).map_err(Conflict::Original)?;
    if digest_of(&original)[..] != header[HDR_DIGEST..HDR_BLOCK_LEN] {
        return Err(Conflict::Digest);
    }
    let nonce = u64::from(header[HDR_NONCE_LO]) | (u64::from(header[HDR_NONCE_HI]) << 32);
    let regenerated = emit_owned(&rules, nonce, &original)
        .map_err(|error| Conflict::Regenerated(Some(Box::new(error))))?;
    if regenerated != raw {
        return Err(Conflict::Regenerated(None));
    }
    Ok(OwnedProgram {
        nonce,
        rules,
        original,
    })
}

fn is_jump_imm(instruction: &Instruction, op: u32, dst: Register) -> bool {
    instruction.opcode == OpCode::jump(op, IMM_SOURCE) && instruction.dst == dst
}

/// Reads back the rule blocks `append_device` emits. It only has to recover candidate rules:
/// `validate_owned` regenerates the block from them and compares bytes, so anything this accepts
/// loosely is still rejected unless it is exactly what the builder produces.
fn parse_rule_blocks(instructions: &[Instruction]) -> Result<Vec<DeviceRule>, RuleBlock> {
    let mut rules = Vec::new();
    let mut rest = instructions.iter().peekable();
    while rest.peek().is_some() {
        let mut rule = DeviceRule {
            allow: false,
            kind: "a".to_owned(),
            major: Some(WILDCARD),
            minor: Some(WILDCARD),
            access: "rwm".to_owned(),
        };
        if let Some(check) = rest.next_if(|next| is_jump_imm(next, JNE, R2)) {
            match check.constant {
                value if value == i64::from(DEV_CHAR) => "c",
                value if value == i64::from(DEV_BLOCK) => "b",
                value => return Err(RuleBlock::DeviceType(value)),
            }
            .clone_into(&mut rule.kind);
        }
        if rest
            .next_if(|next| next.opcode == OpCode::alu32(MOV, REG_SOURCE) && next.dst == R6)
            .is_some()
        {
            let mask = rest
                .next()
                .filter(|and| and.opcode == OpCode::alu32(AND, IMM_SOURCE) && and.dst == R6)
                .ok_or(RuleBlock::AccessMask)?;
            rule.access = bpf_access_string(mask.constant);
            rest.next().ok_or(RuleBlock::AccessCheck)?;
        }
        if let Some(check) = rest.next_if(|next| is_jump_imm(next, JNE, R4)) {
            rule.major = Some(check.constant);
        }
        if let Some(check) = rest.next_if(|next| is_jump_imm(next, JNE, R5)) {
            rule.minor = Some(check.constant);
        }
        let verdict = rest.next().ok_or(RuleBlock::MissingVerdict)?;
        match marker_value(verdict) {
            Some(value @ (0 | 1)) => rule.allow = value == 1,
            Some(_) | None => return Err(RuleBlock::Verdict),
        }
        rest.next()
            .filter(|exit| exit.opcode == OpCode::jump(EXIT, IMM_SOURCE))
            .ok_or(RuleBlock::MissingExit)?;
        rules.push(rule);
    }
    // append_device is fed the rules last to first.
    rules.reverse();
    Ok(rules)
}

/// A `BPF_DEVCG_ACC_*` mask as `rwm` letters.
fn bpf_access_string(mask: i64) -> String {
    [(ACC_READ, 'r'), (ACC_WRITE, 'w'), (ACC_MKNOD, 'm')]
        .into_iter()
        .filter(|&(bit, _)| mask & i64::from(bit) != 0)
        .map(|(_, letter)| letter)
        .collect()
}

/// What the gate needs to know about a program besides its code.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ProgMeta {
    /// Whether the kernel told us the program's map IDs.
    pub map_ids_known: bool,
    /// How many maps the program uses.
    pub map_count: usize,
}

/// The reloadability gate. A translated dump is only guaranteed to load back as the same program
/// when it uses no maps, calls or wide loads (whose kernel-side rewrites the dump does not undo),
/// so wrapping is limited to the instructions device filters use: context loads, simple ALU,
/// conditional and unconditional jumps within the program, and exit.
///
/// # Errors
///
/// [`GateError`] naming the first reason the program cannot be wrapped.
pub fn check_wrappable(instructions: &[Instruction], meta: ProgMeta) -> Result<(), GateError> {
    if !meta.map_ids_known {
        return Err(GateError::MapIdsUnavailable);
    }
    if meta.map_count > 0 {
        return Err(GateError::UsesMaps(meta.map_count));
    }
    if instructions.is_empty() {
        return Err(GateError::Empty);
    }
    for (index, instruction) in instructions.iter().enumerate() {
        if !in_subset(instruction) {
            return Err(GateError::Instruction {
                index,
                instruction: *instruction,
            });
        }
        let opcode = instruction.opcode;
        if opcode.class().is_jump() && opcode.jump_op() != EXIT {
            let target =
                i64::try_from(index).unwrap_or(i64::MAX) + i64::from(instruction.offset) + 1;
            if !usize::try_from(target).is_ok_and(|target| target < instructions.len()) {
                return Err(GateError::JumpsOut(index));
            }
        }
    }
    Ok(())
}

fn in_subset(instruction: &Instruction) -> bool {
    let opcode = instruction.opcode;
    match opcode.class() {
        Class::LdX => {
            opcode.mode() == MODE_MEM
                && instruction.src == R1
                && matches!(instruction.offset, 0 | 4 | 8)
        }
        // A non-zero offset turns Mov into a sign-extending move.
        Class::Alu | Class::Alu64 => {
            matches!(opcode.alu_op(), MOV | AND | OR | RSH | LSH) && instruction.offset == 0
        }
        Class::Jump | Class::Jump32 => match opcode.jump_op() {
            JEQ | JNE | JGT | JGE | JLT | JLE | JSET | JSGT | JSGE | JSLT | JSLE => true,
            // A JMP32 exit is not an instruction, and a JMP32 ja is a long jump with the offset
            // in the immediate.
            EXIT | JA => matches!(opcode.class(), Class::Jump),
            // Call, may_goto and anything unknown.
            _ => false,
        },
        Class::Ld | Class::St | Class::StX => false,
    }
}

/// Instructions that do not encode, with the index of the first, worded as the Go daemon's
/// `canonicalBytes` error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CanonicalError {
    /// The index of the instruction.
    pub index: usize,
    /// Why it does not encode.
    pub cause: EncodeCause,
}

impl fmt::Display for CanonicalError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(out, "encode instruction {}: {}", self.index, self.cause)
    }
}

impl std::error::Error for CanonicalError {}

/// Why a wrapper cannot be emitted, worded as the Go daemon's `emitOwned` errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EmitError {
    /// The original's bytes do not decode.
    Decode(DecodeError),
    /// The original is empty, or a rule cannot be expressed.
    Rule(RuleError),
    /// An instruction of the wrapper does not encode.
    Encode {
        /// Its index in the wrapper.
        index: usize,
        /// Why.
        cause: EncodeCause,
    },
}

impl fmt::Display for EmitError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(out),
            Self::Rule(error) => error.fmt(out),
            Self::Encode { index, cause } => {
                write!(out, "encode wrapper: instruction {index}: {cause}")
            }
        }
    }
}

impl std::error::Error for EmitError {}

/// Why a program's bytes cannot be taken apart.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OwnedError {
    /// The bytes are not whole instructions.
    Decode(DecodeError),
    /// Go's `ErrOwnedBlockConflict`: the program starts like one of this daemon's wrappers but
    /// does not validate as one. Nothing is changed: acting on a partial match could strip a
    /// runtime restriction or keep a stale grant. Restarting the container replaces the program.
    Conflict(Conflict),
}

impl OwnedError {
    /// Whether it is Go's `ErrOwnedBlockConflict`.
    #[must_use]
    pub const fn is_conflict(&self) -> bool {
        matches!(self, Self::Conflict(_))
    }
}

impl fmt::Display for OwnedError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Decode(error) => error.fmt(out),
            Self::Conflict(cause) => write!(
                out,
                "device filter carries an invalid swarm-device-access block: {cause}"
            ),
        }
    }
}

impl std::error::Error for OwnedError {}

/// Why a program that starts like a wrapper is not one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Conflict {
    /// Fewer instructions than a header.
    Header,
    /// A header field that is not a marker.
    HeaderField(usize),
    /// The second magic word or the format version differs.
    MagicOrVersion,
    /// The block length leaves no room for the blocks, the trailer or an original.
    BlockLen(u32),
    /// The trailer is missing or does not match the header.
    Trailer,
    /// The rule blocks do not parse.
    RuleBlock(RuleBlock),
    /// The original does not encode.
    Original(CanonicalError),
    /// The digest does not match the original.
    Digest,
    /// The wrapper regenerated from what was parsed differs, or does not regenerate at all.
    Regenerated(Option<Box<EmitError>>),
}

impl fmt::Display for Conflict {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Header => out.write_str("malformed header"),
            Self::HeaderField(index) => write!(out, "malformed header: field {index}"),
            Self::MagicOrVersion => out.write_str("malformed header: magic or version"),
            Self::BlockLen(block_len) => write!(out, "block length out of range: {block_len}"),
            Self::Trailer => out.write_str("trailer missing or not matching the header"),
            Self::RuleBlock(cause) => write!(out, "rule block does not parse: {cause}"),
            Self::Original(error) => error.fmt(out),
            Self::Digest => out.write_str("digest does not match the wrapped original"),
            Self::Regenerated(None) => out.write_str("block does not regenerate byte for byte"),
            Self::Regenerated(Some(error)) => {
                write!(out, "block does not regenerate byte for byte: {error}")
            }
        }
    }
}

/// Why the rule blocks of a wrapper do not parse.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RuleBlock {
    /// A type check for a type other than char and block.
    DeviceType(i64),
    /// An access check without its mask.
    AccessMask,
    /// An access check without its jump.
    AccessCheck,
    /// A block without its verdict.
    MissingVerdict,
    /// A verdict that is not a store of 0 or 1.
    Verdict,
    /// A block without its exit.
    MissingExit,
}

impl fmt::Display for RuleBlock {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DeviceType(value) => write!(out, "device type {value}"),
            Self::AccessMask => out.write_str("access mask"),
            Self::AccessCheck => out.write_str("access check"),
            Self::MissingVerdict => out.write_str("missing verdict"),
            Self::Verdict => out.write_str("verdict"),
            Self::MissingExit => out.write_str("missing exit"),
        }
    }
}

/// Go's `ErrProgramNotWrappable`: an original device filter outside the instruction subset whose
/// translated dump can be loaded back unchanged.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GateError {
    /// The kernel did not say which maps the program uses.
    MapIdsUnavailable,
    /// The program uses maps.
    UsesMaps(usize),
    /// The program has no instruction.
    Empty,
    /// An instruction outside the subset.
    Instruction {
        /// Its index.
        index: usize,
        /// The instruction, printed as cilium prints it.
        instruction: Instruction,
    },
    /// A jump whose target is outside the program.
    JumpsOut(usize),
}

impl fmt::Display for GateError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("device filter program cannot be wrapped safely: ")?;
        match self {
            Self::MapIdsUnavailable => out.write_str("map IDs unavailable"),
            Self::UsesMaps(count) => write!(out, "program uses {count} maps"),
            Self::Empty => out.write_str("empty program"),
            Self::Instruction { index, instruction } => write!(
                out,
                "instruction {index} ({instruction}): outside the wrappable instruction subset"
            ),
            Self::JumpsOut(index) => write!(out, "instruction {index} jumps out of the program"),
        }
    }
}

impl std::error::Error for GateError {}

/// The kernel's random source failed.
#[derive(Debug)]
pub struct NonceError(std::io::Error);

impl fmt::Display for NonceError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "generate nonce: {}",
            crate::go_compat::errno::io_error_text(&self.0)
        )
    }
}

impl std::error::Error for NonceError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.0)
    }
}

#[cfg(test)]
mod tests;
