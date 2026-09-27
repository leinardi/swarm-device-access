//go:build linux

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

package cgroup

import (
	"encoding/binary"
	"testing"

	"github.com/cilium/ebpf/asm"
	"golang.org/x/sys/unix"
)

// devRequest is a bpf_cgroup_dev_ctx: the access a process asks for.
type devRequest struct {
	typ    uint32 // unix.BPF_DEVCG_DEV_CHAR or unix.BPF_DEVCG_DEV_BLOCK
	access uint32 // unix.BPF_DEVCG_ACC_* mask
	major  uint32
	minor  uint32
}

func charReq(major, minor uint32, access uint32) devRequest {
	return devRequest{typ: unix.BPF_DEVCG_DEV_CHAR, access: access, major: major, minor: minor}
}

func (r devRequest) ctx() []byte {
	buf := make([]byte, 12)
	binary.LittleEndian.PutUint32(buf[0:], r.access<<16|r.typ)
	binary.LittleEndian.PutUint32(buf[4:], r.major)
	binary.LittleEndian.PutUint32(buf[8:], r.minor)

	return buf
}

// runDeviceFilter interprets the subset of eBPF that device filters use
// (context loads, 32/64-bit mov/and/or/lsh/rsh, conditional jumps, ja,
// exit) and returns the program's verdict for req: 1 allows, 0 denies. It
// exists because counting instructions cannot catch a wrong access check.
func runDeviceFilter(t *testing.T, raw []byte, req devRequest) uint64 {
	t.Helper()

	insts, err := fromCanonical(raw)
	if err != nil {
		t.Fatal(err)
	}

	ctx := req.ctx()

	var regs [11]uint64

	for pc, steps := 0, 0; ; pc, steps = pc+1, steps+1 {
		if pc < 0 || pc >= len(insts) || steps > 10*len(insts) {
			t.Fatalf("program left its bounds at pc %d", pc)
		}

		ins := insts[pc]
		op := ins.OpCode

		//nolint:exhaustive // the interpreter covers the wrappable subset only
		switch op.Class() {
		case asm.LdXClass:
			if ins.Src != asm.R1 {
				t.Fatalf("pc %d: load from non-context register", pc)
			}

			off := int(ins.Offset)

			//nolint:exhaustive // device filter context fields are at most 32 bits
			switch op.Size() {
			case asm.Word:
				regs[ins.Dst] = uint64(binary.LittleEndian.Uint32(ctx[off:]))
			case asm.Half:
				regs[ins.Dst] = uint64(binary.LittleEndian.Uint16(ctx[off:]))
			case asm.Byte:
				regs[ins.Dst] = uint64(ctx[off])
			default:
				t.Fatalf("pc %d: unsupported load size", pc)
			}

		case asm.ALUClass, asm.ALU64Class:
			src := uint64(ins.Constant)
			if op.Source() == asm.RegSource {
				src = regs[ins.Src]
			}

			dst := regs[ins.Dst]

			//nolint:exhaustive // the interpreter covers the wrappable subset only
			switch op.ALUOp() {
			case asm.Mov:
				dst = src
			case asm.And:
				dst &= src
			case asm.Or:
				dst |= src
			case asm.LSh:
				dst <<= src & 63
			case asm.RSh:
				if op.Class() == asm.ALUClass {
					dst = uint64(uint32(dst) >> (src & 31))
				} else {
					dst >>= src & 63
				}
			default:
				t.Fatalf("pc %d: unsupported ALU op %v", pc, op)
			}

			if op.Class() == asm.ALUClass {
				dst = uint64(uint32(dst))
			}

			regs[ins.Dst] = dst

		case asm.JumpClass, asm.Jump32Class:
			//nolint:exhaustive // conditional jumps are handled below
			switch op.JumpOp() {
			case asm.Exit:
				return regs[asm.R0]
			case asm.Ja:
				pc += int(ins.Offset)

				continue
			}

			left, right := regs[ins.Dst], uint64(ins.Constant)
			if op.Source() == asm.RegSource {
				right = regs[ins.Src]
			}

			if op.Class() == asm.Jump32Class {
				left, right = uint64(uint32(left)), uint64(uint32(right))
			}

			var taken bool

			//nolint:exhaustive // the interpreter covers the unsigned jumps device filters use
			switch op.JumpOp() {
			case asm.JEq:
				taken = left == right
			case asm.JNE:
				taken = left != right
			case asm.JGT:
				taken = left > right
			case asm.JGE:
				taken = left >= right
			case asm.JLT:
				taken = left < right
			case asm.JLE:
				taken = left <= right
			case asm.JSet:
				taken = left&right != 0
			default:
				t.Fatalf("pc %d: unsupported jump %v", pc, op)
			}

			if taken {
				pc += int(ins.Offset)
			}

		default:
			t.Fatalf("pc %d: unsupported instruction class %v", pc, op)
		}
	}
}
