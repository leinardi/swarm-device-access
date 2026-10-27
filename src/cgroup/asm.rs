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

//! BPF instructions as the Go daemon handled them through `github.com/cilium/ebpf/asm` (v0.22.0):
//! decoded from and encoded to the kernel's 8-byte slots in host byte order, with that package's
//! normalisations, and printed as its `%v` prints them, since the reloadability gate's errors
//! carry the text. Jump targets are offsets here; the rule builder (`ebpf.rs`) resolves its own.
//!
//! The device filter's ownership is decided by regenerating a wrapper and comparing bytes, so
//! every rule of that package that changes bytes is kept, including the ones a plain model would
//! not have: a `ld_imm64` takes two slots and a missing or non-zero second half fails to decode;
//! a helper call's ID is checked for a platform tag; the ALU ops a non-zero offset turns into
//! signed division, signed modulo and sign-extending moves are decoded as their own ops, and
//! every other ALU instruction is encoded with offset 0; an atomic store carries its operation in
//! the opcode while decoded, and the immediate it was decoded from when encoded. The opcode is
//! the package's 32-bit `OpCode`, wider than the byte on the wire for exactly those cases.
//!
//! The model is written from that package's behaviour, which `tests/testdata/devfilter/asm.tsv`
//! (captured from it for every opcode byte) pins; no code of it is copied.

mod builtin;

use std::fmt;

/// The size of one instruction slot.
pub const INSTRUCTION_SIZE: usize = 8;

/// A register number; the kernel has `r0` to `r10`, and a slot has room for 0 to 15.
pub type Register = u8;

/// `r0`, the return value.
pub const R0: Register = 0;
/// `r1`, the context pointer at entry.
pub const R1: Register = 1;
/// `r2`.
pub const R2: Register = 2;
/// `r3`.
pub const R3: Register = 3;
/// `r4`.
pub const R4: Register = 4;
/// `r5`.
pub const R5: Register = 5;
/// `r6`.
pub const R6: Register = 6;
/// `r10`, the read-only frame pointer.
pub const RFP: Register = 10;

/// The source register of a `ld_imm64` of a map's pointer.
const PSEUDO_MAP_FD: Register = 1;
/// The source register of a `ld_imm64` of a value in a map.
const PSEUDO_MAP_VALUE: Register = 2;
/// The source register of a call to a BPF function.
const PSEUDO_CALL: Register = 1;
/// The source register of a call to a kernel function.
const PSEUDO_KFUNC_CALL: Register = 2;
/// The source register of `may_goto`.
const PSEUDO_MAY_GOTO: Register = 0;

const CLASS_MASK: u32 = 0x07;

/// The memory mode bits of a load or store.
const MODE_MASK: u32 = 0xe0;
/// An immediate load.
pub const MODE_IMM: u32 = 0x00;
const MODE_ABS: u32 = 0x20;
const MODE_IND: u32 = 0x40;
/// A load from or store to memory.
pub const MODE_MEM: u32 = 0x60;
const MODE_MEMSX: u32 = 0x80;
const MODE_ATOMIC: u32 = 0xc0;
const INVALID_MODE: u32 = 0xff;

const SIZE_MASK: u32 = 0x18;
/// A 32-bit access.
pub const WORD: u32 = 0x00;
/// A 16-bit access.
pub const HALF: u32 = 0x08;
/// An 8-bit access.
pub const BYTE: u32 = 0x10;
/// A 64-bit access.
pub const DWORD: u32 = 0x18;
const INVALID_SIZE: u32 = 0xff;

const SOURCE_MASK: u32 = 0x08;
/// The immediate is the operand.
pub const IMM_SOURCE: u32 = 0x00;
/// The source register is the operand.
pub const REG_SOURCE: u32 = 0x08;
const INVALID_SOURCE: u32 = 0xffff;

/// The operation bits of an ALU opcode; wider than the byte for the decoded signed and
/// sign-extending ops.
const ALU_MASK: u32 = 0x3ff0;
/// Addition.
pub const ADD: u32 = 0x00;
const SUB: u32 = 0x10;
const MUL: u32 = 0x20;
const DIV: u32 = 0x30;
const SDIV: u32 = DIV + 0x100;
/// Bitwise or.
pub const OR: u32 = 0x40;
/// Bitwise and.
pub const AND: u32 = 0x50;
/// Left shift.
pub const LSH: u32 = 0x60;
/// Logical right shift.
pub const RSH: u32 = 0x70;
const NEG: u32 = 0x80;
const MOD: u32 = 0x90;
const SMOD: u32 = MOD + 0x100;
const XOR: u32 = 0xa0;
/// Move.
pub const MOV: u32 = 0xb0;
const MOVSX8: u32 = MOV + 0x100;
const MOVSX16: u32 = MOV + 0x200;
const MOVSX32: u32 = MOV + 0x300;
const ARSH: u32 = 0xc0;
const SWAP: u32 = 0xd0;
const INVALID_ALU_OP: u32 = 0xffff;

const JUMP_MASK: u32 = 0xf0;
/// The unconditional jump.
pub const JA: u32 = 0x00;
/// Jump if equal.
pub const JEQ: u32 = 0x10;
/// Jump if greater, unsigned.
pub const JGT: u32 = 0x20;
/// Jump if greater or equal, unsigned.
pub const JGE: u32 = 0x30;
/// Jump if any bit is set.
pub const JSET: u32 = 0x40;
/// Jump if not equal.
pub const JNE: u32 = 0x50;
/// Jump if greater, signed.
pub const JSGT: u32 = 0x60;
/// Jump if greater or equal, signed.
pub const JSGE: u32 = 0x70;
/// A call.
pub const CALL: u32 = 0x80;
/// Return.
pub const EXIT: u32 = 0x90;
/// Jump if less, unsigned.
pub const JLT: u32 = 0xa0;
/// Jump if less or equal, unsigned.
pub const JLE: u32 = 0xb0;
/// Jump if less, signed.
pub const JSLT: u32 = 0xc0;
/// Jump if less or equal, signed.
pub const JSLE: u32 = 0xd0;
const JCOND: u32 = 0xe0;
const INVALID_JUMP_OP: u32 = 0xff;

/// The operation of a decoded atomic store, moved there from the immediate.
const ATOMIC_MASK: u32 = 0x1_ff00;
const FETCH: u32 = 0x100;
const XCHG: u32 = 0xe000 | FETCH;
const CMPXCHG: u32 = 0xf000 | FETCH;
const LOAD_ACQUIRE: u32 = 0x1_0000;
const STORE_RELEASE: u32 = 0x1_1000;
const INVALID_ATOMIC: u32 = 0xffff_ffff;

/// cilium's `InvalidOpCode`.
const INVALID_OPCODE: u32 = 0xffff;

/// A helper ID's platform tag sits in bits 28 to 30; Linux's is 0, Windows's 1.
const PLATFORM_SHIFT: u32 = 28;
const PLATFORM_MASK: u32 = 0x7 << PLATFORM_SHIFT;
const WINDOWS_TAG: u32 = 1 << PLATFORM_SHIFT;

/// The class of an opcode, its low three bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Class {
    /// Non-standard loads, `ld_imm64` among them.
    Ld,
    /// Loads into a register.
    LdX,
    /// Stores of an immediate.
    St,
    /// Stores of a register, and the atomics.
    StX,
    /// 32-bit arithmetic.
    Alu,
    /// 64-bit jumps.
    Jump,
    /// 32-bit jumps.
    Jump32,
    /// 64-bit arithmetic.
    Alu64,
}

impl Class {
    const fn name(self) -> &'static str {
        match self {
            Self::Ld => "Ld",
            Self::LdX => "LdX",
            Self::St => "St",
            Self::StX => "StX",
            Self::Alu => "ALU",
            Self::Jump => "Jump",
            Self::Jump32 => "Jump32",
            Self::Alu64 => "ALU64",
        }
    }

    const fn is_load_or_store(self) -> bool {
        matches!(self, Self::Ld | Self::LdX | Self::St | Self::StX)
    }

    /// Whether it is [`Class::Alu`] or [`Class::Alu64`].
    #[must_use]
    pub const fn is_alu(self) -> bool {
        matches!(self, Self::Alu | Self::Alu64)
    }

    /// Whether it is [`Class::Jump`] or [`Class::Jump32`].
    #[must_use]
    pub const fn is_jump(self) -> bool {
        matches!(self, Self::Jump | Self::Jump32)
    }
}

/// cilium's `OpCode`: the opcode byte, and for decoded instructions the extra bits that tell the
/// signed, sign-extending and atomic operations apart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct OpCode(pub u32);

impl OpCode {
    /// A 32-bit ALU opcode.
    #[must_use]
    pub const fn alu32(op: u32, source: u32) -> Self {
        Self(0x04 | op | source)
    }

    /// A 64-bit ALU opcode.
    #[must_use]
    pub const fn alu64(op: u32, source: u32) -> Self {
        Self(0x07 | op | source)
    }

    /// A 64-bit jump opcode.
    #[must_use]
    pub const fn jump(op: u32, source: u32) -> Self {
        Self(0x05 | op | source)
    }

    /// A load into a register (`LdX`) in `mode` and `size`.
    #[must_use]
    pub const fn load_x(mode: u32, size: u32) -> Self {
        Self(0x01 | mode | size)
    }

    /// A non-standard load (`Ld`) in `mode` and `size`; `MODE_IMM` with `DWORD` is `ld_imm64`.
    #[must_use]
    pub const fn load(mode: u32, size: u32) -> Self {
        Self(mode | size)
    }

    /// A store of a register (`StX`) in `mode` and `size`.
    #[must_use]
    pub const fn store_x(mode: u32, size: u32) -> Self {
        Self(0x03 | mode | size)
    }

    /// The class.
    #[must_use]
    pub const fn class(self) -> Class {
        match self.0 & CLASS_MASK {
            0 => Class::Ld,
            1 => Class::LdX,
            2 => Class::St,
            3 => Class::StX,
            4 => Class::Alu,
            5 => Class::Jump,
            6 => Class::Jump32,
            // The mask leaves 7 as the only other value.
            _ => Class::Alu64,
        }
    }

    /// The memory mode of a load or store, else cilium's `InvalidMode`.
    #[must_use]
    pub const fn mode(self) -> u32 {
        if self.class().is_load_or_store() {
            self.0 & MODE_MASK
        } else {
            INVALID_MODE
        }
    }

    /// The access size of a load or store, else cilium's `InvalidSize`.
    #[must_use]
    pub const fn size(self) -> u32 {
        if self.class().is_load_or_store() {
            self.0 & SIZE_MASK
        } else {
            INVALID_SIZE
        }
    }

    /// The operation of an atomic store, else cilium's `InvalidAtomic`.
    const fn atomic_op(self) -> u32 {
        if matches!(self.class(), Class::StX) && self.mode() == MODE_ATOMIC {
            self.0 & ATOMIC_MASK
        } else {
            INVALID_ATOMIC
        }
    }

    /// The operand source of a jump or a non-swap ALU op, else cilium's `InvalidSource`.
    #[must_use]
    pub const fn source(self) -> u32 {
        let class = self.class();
        if !(class.is_jump() || class.is_alu()) || self.alu_op() == SWAP {
            INVALID_SOURCE
        } else {
            self.0 & SOURCE_MASK
        }
    }

    /// The ALU operation, else cilium's `InvalidALUOp`.
    #[must_use]
    pub const fn alu_op(self) -> u32 {
        if self.class().is_alu() {
            self.0 & ALU_MASK
        } else {
            INVALID_ALU_OP
        }
    }

    /// The jump operation, else cilium's `InvalidJumpOp`, which a 32-bit exit or call also is.
    #[must_use]
    pub const fn jump_op(self) -> u32 {
        let class = self.class();
        if !class.is_jump() {
            return INVALID_JUMP_OP;
        }
        let op = self.0 & JUMP_MASK;
        if matches!(class, Class::Jump32) && (op == EXIT || op == CALL) {
            INVALID_JUMP_OP
        } else {
            op
        }
    }

    const fn with_alu_op(self, op: u32) -> Self {
        Self((self.0 & !ALU_MASK) | op)
    }

    /// Whether it is `ld_imm64`, the one instruction that takes two slots.
    #[must_use]
    pub const fn is_dword_load(self) -> bool {
        self.0 == Self::load(MODE_IMM, DWORD).0
    }

    fn write_load_or_store(self, class: Class, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(class.name())?;
        match self.mode() {
            MODE_IMM => out.write_str("Imm")?,
            MODE_ABS => out.write_str("Abs")?,
            MODE_IND => out.write_str("Ind")?,
            MODE_MEM => out.write_str("Mem")?,
            MODE_MEMSX => out.write_str("MemSX")?,
            MODE_ATOMIC => out.write_str("Atomic")?,
            mode => write!(out, "Mode({mode})")?,
        }
        let atomic = self.atomic_op();
        if atomic != INVALID_ATOMIC {
            out.write_str(&atomic_name(atomic))?;
        }
        out.write_str(match self.size() {
            DWORD => "DW",
            WORD => "W",
            HALF => "H",
            BYTE => "B",
            _ => "",
        })
    }

    fn write_alu(self, class: Class, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = self.alu_op();
        if op == SWAP && matches!(class, Class::Alu64) {
            out.write_str("B")?;
        }
        out.write_str(&alu_name(op))?;
        if op == SWAP {
            if matches!(class, Class::Alu) {
                out.write_str(if self.0 & SOURCE_MASK == 0 {
                    "LE"
                } else {
                    "BE"
                })?;
            }
            return Ok(());
        }
        out.write_str(source_name(self.source()))?;
        if matches!(class, Class::Alu) {
            out.write_str("32")?;
        }
        Ok(())
    }

    fn write_jump(self, class: Class, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let op = self.jump_op();
        out.write_str(&jump_name(op))?;
        if matches!(class, Class::Jump32) {
            out.write_str("32")?;
        }
        if op != EXIT && op != CALL && op != JA {
            out.write_str(source_name(self.source()))?;
        }
        Ok(())
    }
}

/// cilium's `OpCode.String`.
impl fmt::Display for OpCode {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let class = self.class();
        match class {
            Class::Ld | Class::LdX | Class::St | Class::StX => self.write_load_or_store(class, out),
            Class::Alu | Class::Alu64 => self.write_alu(class, out),
            Class::Jump | Class::Jump32 => self.write_jump(class, out),
        }
    }
}

fn source_name(source: u32) -> &'static str {
    match source {
        IMM_SOURCE => "Imm",
        REG_SOURCE => "Reg",
        // Only a swap has no source, and its name is written without one.
        _ => "Invalid",
    }
}

fn alu_name(op: u32) -> String {
    let name = match op {
        ADD => "Add",
        SUB => "Sub",
        MUL => "Mul",
        DIV => "Div",
        OR => "Or",
        AND => "And",
        LSH => "LSh",
        RSH => "RSh",
        NEG => "Neg",
        MOD => "Mod",
        XOR => "Xor",
        MOV => "Mov",
        ARSH => "ArSh",
        SWAP => "Swap",
        SDIV => "SDiv",
        SMOD => "SMod",
        MOVSX8 => "MovSX8",
        MOVSX16 => "MovSX16",
        MOVSX32 => "MovSX32",
        INVALID_ALU_OP => "InvalidALUOp",
        other => return format!("ALUOp({other})"),
    };
    name.to_owned()
}

fn jump_name(op: u32) -> String {
    let name = match op {
        JA => "Ja",
        JEQ => "JEq",
        JGT => "JGT",
        JGE => "JGE",
        JSET => "JSet",
        JNE => "JNE",
        JSGT => "JSGT",
        JSGE => "JSGE",
        CALL => "Call",
        EXIT => "Exit",
        JLT => "JLT",
        JLE => "JLE",
        JSLT => "JSLT",
        JSLE => "JSLE",
        JCOND => "JCOND",
        INVALID_JUMP_OP => "InvalidJumpOp",
        other => return format!("JumpOp({other})"),
    };
    name.to_owned()
}

fn atomic_name(op: u32) -> String {
    match op {
        0x0000 | 0x5000 | 0x4000 | 0xa000 => alu_name(op >> 8),
        0x0100 | 0x5100 | 0x4100 | 0xa100 => format!("Fetch{}", alu_name((op ^ FETCH) >> 8)),
        XCHG => "Xchg".to_owned(),
        CMPXCHG => "CmpXchg".to_owned(),
        LOAD_ACQUIRE => "LdAcq".to_owned(),
        STORE_RELEASE => "StRel".to_owned(),
        other => format!("AtomicOp({other:#x})"),
    }
}

/// `r<n>`, and `rfp` for `r10`.
struct RegisterName(Register);

impl fmt::Display for RegisterName {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == RFP {
            out.write_str("rfp")
        } else {
            write!(out, "r{}", self.0)
        }
    }
}

/// One instruction, as cilium's `asm.Instruction` holds it without metadata: a `ld_imm64` is one
/// instruction with its 64-bit immediate.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Instruction {
    /// The opcode, with the decoded extra bits.
    pub opcode: OpCode,
    /// The destination register.
    pub dst: Register,
    /// The source register.
    pub src: Register,
    /// The signed offset.
    pub offset: i16,
    /// The immediate, sign-extended from 32 bits except for a `ld_imm64`.
    pub constant: i64,
}

impl Instruction {
    /// A load of `size` from `src` plus `offset` into `dst`.
    #[must_use]
    pub const fn load_mem(dst: Register, src: Register, offset: i16, size: u32) -> Self {
        Self {
            opcode: OpCode::load_x(MODE_MEM, size),
            dst,
            src,
            offset,
            constant: 0,
        }
    }

    /// A 32-bit ALU op with an immediate operand.
    #[must_use]
    pub fn alu32_imm(op: u32, dst: Register, value: i32) -> Self {
        Self {
            opcode: OpCode::alu32(op, IMM_SOURCE),
            dst,
            src: R0,
            offset: 0,
            constant: i64::from(value),
        }
    }

    /// A 32-bit ALU op with a register operand.
    #[must_use]
    pub const fn alu32_reg(op: u32, dst: Register, src: Register) -> Self {
        Self {
            opcode: OpCode::alu32(op, REG_SOURCE),
            dst,
            src,
            offset: 0,
            constant: 0,
        }
    }

    /// A 64-bit conditional jump comparing `dst` with an immediate.
    #[must_use]
    pub fn jump_imm(op: u32, dst: Register, value: i32, offset: i16) -> Self {
        Self {
            opcode: OpCode::jump(op, IMM_SOURCE),
            dst,
            src: R0,
            offset,
            constant: i64::from(value),
        }
    }

    /// A 64-bit conditional jump comparing `dst` with `src`.
    #[must_use]
    pub const fn jump_reg(op: u32, dst: Register, src: Register, offset: i16) -> Self {
        Self {
            opcode: OpCode::jump(op, REG_SOURCE),
            dst,
            src,
            offset,
            constant: 0,
        }
    }

    /// `exit`, cilium's `Return()`.
    #[must_use]
    pub const fn exit() -> Self {
        Self {
            opcode: OpCode::jump(EXIT, IMM_SOURCE),
            dst: R0,
            src: R0,
            offset: 0,
            constant: 0,
        }
    }

    /// How many slots it takes: two for a `ld_imm64`.
    #[must_use]
    pub const fn width(&self) -> usize {
        if self.opcode.is_dword_load() { 2 } else { 1 }
    }

    /// Whether it is a call of a kernel helper.
    const fn is_builtin_call(&self) -> bool {
        self.opcode.jump_op() == CALL && self.src == R0 && self.dst == R0
    }

    /// Whether it is a `ld_imm64` of a map's pointer or of a value in it.
    const fn is_load_from_map(&self) -> bool {
        self.opcode.is_dword_load() && (self.src == PSEUDO_MAP_FD || self.src == PSEUDO_MAP_VALUE)
    }

    /// Appends the encoding in host byte order, as cilium's `Instruction.Marshal` writes it.
    ///
    /// # Errors
    ///
    /// [`EncodeCause`] for what cilium refuses to encode, before anything is appended.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<(), EncodeCause> {
        let mut opcode = self.opcode;
        if opcode.0 == INVALID_OPCODE {
            return Err(EncodeCause::InvalidOpCode);
        }
        let mut offset = self.offset;
        let constant = self.constant;
        // The immediate is taken before the atomic case below: cilium sets the constant from
        // the opcode there, but writes the immediate it took here.
        let mut immediate = if opcode.is_dword_load() {
            low_u32(constant).cast_signed()
        } else {
            low_i32(constant)
        };
        if self.is_builtin_call() {
            let id = low_u32(constant);
            match id & PLATFORM_MASK {
                0 => immediate = (id & !PLATFORM_MASK).cast_signed(),
                tag => {
                    return Err(EncodeCause::Unsupported {
                        function: builtin::name(id),
                        platform: if tag == WINDOWS_TAG { "windows" } else { "" },
                    });
                }
            }
        } else if opcode.class().is_alu() {
            let (base, wire_offset) = match opcode.alu_op() {
                SDIV => (DIV, 1),
                SMOD => (MOD, 1),
                MOVSX8 => (MOV, 8),
                MOVSX16 => (MOV, 16),
                MOVSX32 => (MOV, 32),
                op => (op, 0),
            };
            if wire_offset != 0 && offset != 0 {
                return Err(EncodeCause::ExtendedOffset);
            }
            opcode = opcode.with_alu_op(base);
            offset = wire_offset;
        } else if opcode.atomic_op() != INVALID_ATOMIC {
            opcode = OpCode(opcode.0 & !ATOMIC_MASK);
        }
        let Ok(op) = u8::try_from(opcode.0) else {
            return Err(EncodeCause::OpCodeBits(opcode.to_string()));
        };
        out.push(op);
        out.push(registers(self.dst, self.src));
        out.extend_from_slice(&offset.to_ne_bytes());
        out.extend_from_slice(&immediate.to_ne_bytes());
        if opcode.is_dword_load() {
            out.extend_from_slice(&0_u32.to_ne_bytes());
            out.extend_from_slice(&high_u32(constant).to_ne_bytes());
        }
        Ok(())
    }

    fn write_load_or_store(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (dst, src) = (RegisterName(self.dst), RegisterName(self.src));
        let (offset, constant) = (self.offset, self.constant);
        write!(out, "{} ", self.opcode)?;
        match self.opcode.mode() {
            MODE_IMM => write!(out, "dst: {dst} imm: {constant}"),
            MODE_ABS => write!(out, "imm: {constant}"),
            MODE_IND => write!(out, "dst: {dst} src: {src} imm: {constant}"),
            MODE_MEM | MODE_MEMSX => {
                write!(out, "dst: {dst} src: {src} off: {offset} imm: {constant}")
            }
            MODE_ATOMIC => write!(out, "dst: {dst} src: {src} off: {offset}"),
            _ => Ok(()),
        }
    }

    fn write_alu(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opcode = self.opcode;
        write!(out, "{opcode}")?;
        if opcode == OpCode::alu64(SWAP, IMM_SOURCE) {
            write!(out, "{}", self.constant)?;
        }
        write!(out, " dst: {} ", RegisterName(self.dst))?;
        if opcode.alu_op() == SWAP {
            Ok(())
        } else if opcode.source() == IMM_SOURCE {
            write!(out, "imm: {}", self.constant)
        } else {
            write!(out, "src: {}", RegisterName(self.src))
        }
    }

    fn write_jump(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opcode = self.opcode;
        match opcode.jump_op() {
            CALL => {
                write!(out, "{opcode} ")?;
                match self.src {
                    PSEUDO_CALL if self.offset != 0 => write!(out, "{}", self.offset),
                    PSEUDO_CALL => write!(out, "{}", self.constant),
                    PSEUDO_KFUNC_CALL => write!(out, "Kfunc({})", self.constant),
                    _ => out.write_str(&builtin::name(low_u32(self.constant))),
                }
            }
            JA => {
                write!(out, "{opcode} ")?;
                if matches!(opcode.class(), Class::Jump32) {
                    write!(out, "imm: {}", self.constant)
                } else {
                    write!(out, "off: {}", self.offset)
                }
            }
            JCOND if self.src == PSEUDO_MAY_GOTO => {
                write!(out, "JCond may_goto off: {}", self.offset)
            }
            JCOND => write!(out, "{opcode}"),
            _ => {
                write!(
                    out,
                    "{opcode} dst: {} off: {} ",
                    RegisterName(self.dst),
                    self.offset
                )?;
                if opcode.source() == IMM_SOURCE {
                    write!(out, "imm: {}", self.constant)
                } else {
                    write!(out, "src: {}", RegisterName(self.src))
                }
            }
        }
    }
}

/// cilium's `%v` of an instruction without metadata.
impl fmt::Display for Instruction {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        let opcode = self.opcode;
        if opcode.0 == INVALID_OPCODE {
            return out.write_str("INVALID");
        }
        if opcode.jump_op() == EXIT {
            return write!(out, "{opcode}");
        }
        if self.is_load_from_map() {
            let fd = low_i32(self.constant);
            let dst = RegisterName(self.dst);
            return if self.src == PSEUDO_MAP_FD {
                write!(out, "LoadMapPtr dst: {dst} fd: {fd}")
            } else {
                let value_offset = high_u32(self.constant);
                write!(out, "LoadMapValue dst: {dst}, fd: {fd} off: {value_offset}")
            };
        }
        match opcode.class() {
            Class::Ld | Class::LdX | Class::St | Class::StX => self.write_load_or_store(out),
            Class::Alu | Class::Alu64 => self.write_alu(out),
            Class::Jump | Class::Jump32 => self.write_jump(out),
        }
    }
}

/// The register byte: the destination in the low nibble on little-endian hosts, in the high one
/// on big-endian hosts.
fn registers(dst: Register, src: Register) -> u8 {
    if cfg!(target_endian = "little") {
        (src << 4) | (dst & 0x0f)
    } else {
        (dst << 4) | (src & 0x0f)
    }
}

/// The low 32 bits, as Go's `uint32(value)`.
pub(super) fn low_u32(value: i64) -> u32 {
    let [first, second, third, fourth, ..] = value.to_le_bytes();
    u32::from_le_bytes([first, second, third, fourth])
}

/// Go's `int32(value)`.
pub(super) fn low_i32(value: i64) -> i32 {
    low_u32(value).cast_signed()
}

/// Go's `uint32(value >> 32)`.
fn high_u32(value: i64) -> u32 {
    low_u32(value >> 32)
}

/// Decodes `raw` as cilium's `AppendInstructions` does for Linux in host byte order.
///
/// # Errors
///
/// [`DecodeError`] for a truncated slot, a `ld_imm64` without a valid second half, or a helper
/// call whose ID carries a platform tag.
pub fn decode(raw: &[u8]) -> Result<Vec<Instruction>, DecodeError> {
    let mut instructions = Vec::with_capacity(raw.len() / INSTRUCTION_SIZE);
    let mut offset = 0;
    while let Some(rest) = raw.get(offset..).filter(|rest| !rest.is_empty()) {
        let instruction = decode_one(rest).map_err(|cause| DecodeError { offset, cause })?;
        offset += instruction.width() * INSTRUCTION_SIZE;
        instructions.push(instruction);
    }
    Ok(instructions)
}

fn slot(raw: &[u8], index: usize) -> Option<[u8; INSTRUCTION_SIZE]> {
    let start = index * INSTRUCTION_SIZE;
    raw.get(start..start + INSTRUCTION_SIZE)?.try_into().ok()
}

fn decode_one(raw: &[u8]) -> Result<Instruction, DecodeCause> {
    let [op, regs, offset_0, offset_1, imm_0, imm_1, imm_2, imm_3] =
        slot(raw, 0).ok_or(DecodeCause::UnexpectedEof)?;
    let (dst, src) = if cfg!(target_endian = "little") {
        (regs & 0x0f, regs >> 4)
    } else {
        (regs >> 4, regs & 0x0f)
    };
    let mut instruction = Instruction {
        opcode: OpCode(u32::from(op)),
        dst,
        src,
        offset: i16::from_ne_bytes([offset_0, offset_1]),
        constant: i64::from(i32::from_ne_bytes([imm_0, imm_1, imm_2, imm_3])),
    };
    let opcode = instruction.opcode;
    if instruction.is_builtin_call() {
        // A negative ID is kept as it is; a non-negative one must carry no platform tag, and
        // Linux's tag is 0, so it is kept too.
        if let Ok(id) = u32::try_from(instruction.constant)
            && id >> PLATFORM_SHIFT > 0
        {
            return Err(DecodeCause::InvalidConstant(id));
        }
    } else if opcode.class().is_alu() {
        let extended = match (opcode.alu_op(), instruction.offset) {
            (DIV, 1) => Some(SDIV),
            (MOD, 1) => Some(SMOD),
            (MOV, 8) => Some(MOVSX8),
            (MOV, 16) => Some(MOVSX16),
            (MOV, 32) => Some(MOVSX32),
            _ => None,
        };
        if let Some(op) = extended {
            instruction.opcode = opcode.with_alu_op(op);
            instruction.offset = 0;
        }
    } else if matches!(opcode.class(), Class::StX) && opcode.mode() == MODE_ATOMIC {
        instruction.opcode = OpCode(opcode.0 | (low_u32(instruction.constant << 8) & ATOMIC_MASK));
    }
    if !opcode.is_dword_load() {
        return Ok(instruction);
    }
    let [
        zero_0,
        zero_1,
        zero_2,
        zero_3,
        high_0,
        high_1,
        high_2,
        high_3,
    ] = slot(raw, 1).ok_or(DecodeCause::MissingSecondHalf)?;
    if u32::from_ne_bytes([zero_0, zero_1, zero_2, zero_3]) != 0 {
        return Err(DecodeCause::NonZeroFields);
    }
    let high = i32::from_ne_bytes([high_0, high_1, high_2, high_3]);
    instruction.constant = (i64::from(high) << 32) | i64::from(low_u32(instruction.constant));
    Ok(instruction)
}

/// Why raw bytes do not decode, with the byte offset of the instruction, worded as the Go
/// daemon's `fromCanonical` error.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecodeError {
    /// The byte offset of the instruction that does not decode.
    pub offset: usize,
    /// Why.
    pub cause: DecodeCause,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            out,
            "decode instructions: offset {}: {}",
            self.offset, self.cause
        )
    }
}

impl std::error::Error for DecodeError {}

/// Why one instruction does not decode.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeCause {
    /// Fewer than 8 bytes are left.
    UnexpectedEof,
    /// A `ld_imm64` is the last slot.
    MissingSecondHalf,
    /// The second slot of a `ld_imm64` has something other than the immediate's high half.
    NonZeroFields,
    /// A helper call's ID carries a platform tag.
    InvalidConstant(u32),
}

impl fmt::Display for DecodeCause {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnexpectedEof => out.write_str("unexpected EOF"),
            Self::MissingSecondHalf => out.write_str("64bit immediate is missing second half"),
            Self::NonZeroFields => out.write_str("64bit immediate has non-zero fields"),
            Self::InvalidConstant(id) => write!(out, "invalid constant 0x{id:x}"),
        }
    }
}

/// Why cilium refuses to encode an instruction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeCause {
    /// cilium's `InvalidOpCode`.
    InvalidOpCode,
    /// An opcode wider than a byte after the extra bits are folded back.
    OpCodeBits(String),
    /// A call of a helper of another platform, or of an ID with an unknown platform tag.
    Unsupported {
        /// The helper's name, or `BuiltinFunc(<id>)`.
        function: String,
        /// `windows`, or empty for an unknown tag.
        platform: &'static str,
    },
    /// A signed or sign-extending ALU op that also has an offset.
    ExtendedOffset,
}

impl fmt::Display for EncodeCause {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidOpCode => out.write_str("invalid opcode"),
            // Go formats the opcode's String() with %x, so the name is printed in hex.
            Self::OpCodeBits(name) => {
                out.write_str("invalid opcode ")?;
                name.bytes().try_for_each(|byte| write!(out, "{byte:02x}"))
            }
            Self::Unsupported { function, platform } => {
                write!(
                    out,
                    "function {function} ({platform}): not supported on linux"
                )
            }
            // Go formats the instruction with %s, which its Format answers with this.
            Self::ExtendedOffset => {
                out.write_str("extended ALU opcodes should have an .Offset of 0: {UNRECOGNIZED: s}")
            }
        }
    }
}

impl std::error::Error for EncodeCause {}

#[cfg(test)]
mod tests;
