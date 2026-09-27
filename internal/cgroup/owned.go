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
	"bytes"
	"crypto/rand"
	"crypto/sha256"
	"encoding/binary"
	"errors"
	"fmt"
	"slices"

	"github.com/cilium/ebpf/asm"
	"golang.org/x/sys/unix"
)

// An owned device filter is the runtime's original program wrapped by this
// daemon:
//
//	HEADER, init, rule blocks, TRAILER, original
//
// HEADER and TRAILER are dead stores to R0 (R0 is written again before every
// exit), so they cost nothing at run time but survive in the kernel's
// translated dump. The header carries magic_lo, magic_hi, format_version,
// nonce_lo, nonce_hi, digest_0..digest_3 and block_len: a random 64-bit
// nonce per wrapper lineage, the first 128 bits of the SHA-256 of the
// original's canonical bytes, and the instruction index of the trailer. The
// trailer repeats magic_lo, magic_hi, nonce_lo and nonce_hi.
//
// Ownership is never inferred from a partial match: a program is owned only
// when the whole wrapper regenerates byte for byte from the rules and nonce
// parsed out of it and the original that follows it (which also checks the
// digest, the block length and the trailer). A program that starts like a
// header but fails that is a conflict and is never modified.
const (
	ownedMagicLo       uint32 = 0x5f414453 // "SDA_" little-endian
	ownedMagicHi       uint32 = 0x4b4c4244 // "DBLK" little-endian
	ownedFormatVersion uint32 = 1

	ownedHeaderLen  = 10
	ownedTrailerLen = 4
	ownedInitLen    = 5

	// ownedProgramName names wrappers for bpftool users.
	ownedProgramName = "sda_devfilter"
)

// Header field positions.
const (
	hdrMagicLo = iota
	hdrMagicHi
	hdrVersion
	hdrNonceLo
	hdrNonceHi
	hdrDigest0
	hdrDigest1
	hdrDigest2
	hdrDigest3
	hdrBlockLen
)

var (
	// ErrOwnedBlockConflict reports a device filter that starts like one of
	// this daemon's wrappers but does not validate as one. Nothing is changed:
	// acting on a partial match could strip a runtime restriction or keep a
	// stale grant. Restarting the container replaces the program.
	ErrOwnedBlockConflict = errors.New("device filter carries an invalid swarm-device-access block")

	// ErrProgramNotWrappable reports an original device filter outside the
	// instruction subset whose translated dump can be loaded back unchanged.
	ErrProgramNotWrappable = errors.New("device filter program cannot be wrapped safely")
)

// hostByteOrder is the byte order of kernel instruction dumps. cilium only
// accepts the concrete binary.LittleEndian or binary.BigEndian values, not
// binary.NativeEndian.
var hostByteOrder = func() binary.ByteOrder {
	if binary.NativeEndian.Uint16([]byte{1, 0}) == 1 {
		return binary.LittleEndian
	}

	return binary.BigEndian
}()

// ownedProgram is a validated wrapper taken apart.
type ownedProgram struct {
	nonce    uint64
	rules    []DeviceRule
	original []byte // canonical bytes of the wrapped original
}

// canonicalBytes encodes instructions as raw kernel bytes with symbols and
// other metadata stripped, the form every comparison and digest uses.
func canonicalBytes(insts asm.Instructions) ([]byte, error) {
	var buf bytes.Buffer

	for idx, ins := range insts {
		ins.Metadata = asm.Metadata{}

		_, err := ins.Marshal(&buf, hostByteOrder)
		if err != nil {
			return nil, fmt.Errorf("encode instruction %d: %w", idx, err)
		}
	}

	return buf.Bytes(), nil
}

// fromCanonical decodes raw kernel bytes into instructions without symbols.
func fromCanonical(raw []byte) (asm.Instructions, error) {
	insts, err := asm.AppendInstructions(nil, bytes.NewReader(raw), hostByteOrder, "linux")
	if err != nil {
		return nil, fmt.Errorf("decode instructions: %w", err)
	}

	return insts, nil
}

func markerInst(value uint32) asm.Instruction {
	return asm.Mov.Imm32(asm.R0, int32(value))
}

// markerValue returns the value of a dead-store marker instruction.
func markerValue(ins asm.Instruction) (uint32, bool) {
	if ins.OpCode != asm.Mov.Op32(asm.ImmSource) || ins.Dst != asm.R0 || ins.Src != asm.R0 ||
		ins.Offset != 0 {
		return 0, false
	}

	return uint32(ins.Constant), true
}

func digestOf(original []byte) [4]uint32 {
	sum := sha256.Sum256(original)

	var digest [4]uint32
	for idx := range digest {
		digest[idx] = binary.LittleEndian.Uint32(sum[idx*4:])
	}

	return digest
}

func newNonce() (uint64, error) {
	var raw [8]byte

	_, err := rand.Read(raw[:])
	if err != nil {
		return 0, fmt.Errorf("generate nonce: %w", err)
	}

	return binary.LittleEndian.Uint64(raw[:]), nil
}

// emitOwned builds the wrapper for rules around the canonical original.
func emitOwned(rules []DeviceRule, nonce uint64, original []byte) ([]byte, error) {
	origInsts, err := fromCanonical(original)
	if err != nil {
		return nil, err
	}

	if len(origInsts) == 0 {
		return nil, errNoOriginalProgram
	}

	labelPrefix, err := randomLabelPrefix()
	if err != nil {
		return nil, fmt.Errorf("generate label prefix: %w", err)
	}

	body := &program{}
	body.init()

	for _, dev := range slices.Backward(rules) {
		err = body.appendDevice(dev, labelPrefix)
		if err != nil {
			return nil, err
		}
	}

	nonceLo, nonceHi := uint32(nonce), uint32(nonce>>32)
	digest := digestOf(original)
	blockLen := ownedHeaderLen + len(body.insts)

	header := asm.Instructions{
		markerInst(ownedMagicLo), markerInst(ownedMagicHi), markerInst(ownedFormatVersion),
		markerInst(nonceLo), markerInst(nonceHi),
		markerInst(digest[0]), markerInst(digest[1]), markerInst(digest[2]), markerInst(digest[3]),
		markerInst(uint32(blockLen)),
	}

	// The last rule block falls through to the trailer, which in turn falls
	// through to the original.
	fallThrough := fmt.Sprintf("%s-block-%d", labelPrefix, body.blockID)
	trailer := asm.Instructions{
		markerInst(ownedMagicLo).WithSymbol(fallThrough), markerInst(ownedMagicHi),
		markerInst(nonceLo), markerInst(nonceHi),
	}

	all := slices.Concat(header, body.insts, trailer, origInsts)

	var buf bytes.Buffer

	err = all.Marshal(&buf, hostByteOrder)
	if err != nil {
		return nil, fmt.Errorf("encode wrapper: %w", err)
	}

	return buf.Bytes(), nil
}

// parseOwned takes a program's canonical bytes apart. owned is false, with no
// error, when the program does not start like a header: it is then an opaque
// original, whatever else it contains. A program that starts like a header
// but does not validate completely returns ErrOwnedBlockConflict.
func parseOwned(raw []byte) (*ownedProgram, bool, error) {
	insts, err := fromCanonical(raw)
	if err != nil {
		return nil, false, err
	}

	if len(insts) == 0 {
		return nil, false, nil
	}

	first, isMarker := markerValue(insts[0])
	if !isMarker || first != ownedMagicLo {
		return nil, false, nil
	}

	owned, err := validateOwned(insts, raw)
	if err != nil {
		return nil, false, fmt.Errorf("%w: %w", ErrOwnedBlockConflict, err)
	}

	return owned, true, nil
}

var (
	errHeader      = errors.New("malformed header")
	errBlockLen    = errors.New("block length out of range")
	errTrailer     = errors.New("trailer missing or not matching the header")
	errRuleBlock   = errors.New("rule block does not parse")
	errDigest      = errors.New("digest does not match the wrapped original")
	errRegenerated = errors.New("block does not regenerate byte for byte")
)

func validateOwned(insts asm.Instructions, raw []byte) (*ownedProgram, error) {
	if len(insts) < ownedHeaderLen {
		return nil, errHeader
	}

	var header [ownedHeaderLen]uint32

	for idx := range header {
		value, ok := markerValue(insts[idx])
		if !ok {
			return nil, fmt.Errorf("%w: field %d", errHeader, idx)
		}

		header[idx] = value
	}

	if header[hdrMagicHi] != ownedMagicHi || header[hdrVersion] != ownedFormatVersion {
		return nil, fmt.Errorf("%w: magic or version", errHeader)
	}

	blockLen := int(header[hdrBlockLen])
	if blockLen < ownedHeaderLen+ownedInitLen || blockLen+ownedTrailerLen >= len(insts) {
		return nil, fmt.Errorf("%w: %d", errBlockLen, blockLen)
	}

	want := []uint32{ownedMagicLo, ownedMagicHi, header[hdrNonceLo], header[hdrNonceHi]}
	for idx, value := range want {
		got, ok := markerValue(insts[blockLen+idx])
		if !ok || got != value {
			return nil, errTrailer
		}
	}

	rules, err := parseRuleBlocks(insts[ownedHeaderLen+ownedInitLen : blockLen])
	if err != nil {
		return nil, err
	}

	original, err := canonicalBytes(insts[blockLen+ownedTrailerLen:])
	if err != nil {
		return nil, err
	}

	if digestOf(
		original,
	) != [4]uint32{
		header[hdrDigest0],
		header[hdrDigest1],
		header[hdrDigest2],
		header[hdrDigest3],
	} {
		return nil, errDigest
	}

	nonce := uint64(header[hdrNonceLo]) | uint64(header[hdrNonceHi])<<32

	regenerated, err := emitOwned(rules, nonce, original)
	if err != nil {
		return nil, fmt.Errorf("%w: %w", errRegenerated, err)
	}

	if !bytes.Equal(regenerated, raw) {
		return nil, errRegenerated
	}

	return &ownedProgram{nonce: nonce, rules: rules, original: original}, nil
}

func isJumpImm(ins asm.Instruction, op asm.JumpOp, dst asm.Register) bool {
	return ins.OpCode == op.Op(asm.ImmSource) && ins.Dst == dst
}

// parseRuleBlocks reads back the rule blocks appendDevice emits. It only
// has to recover candidate rules: validateOwned regenerates the block from
// them and compares bytes, so anything this accepts loosely is still
// rejected unless it is exactly what the generator produces.
func parseRuleBlocks(insts asm.Instructions) ([]DeviceRule, error) {
	var rules []DeviceRule

	idx := 0
	next := func() (asm.Instruction, bool) {
		if idx >= len(insts) {
			return asm.Instruction{}, false
		}

		idx++

		return insts[idx-1], true
	}

	for idx < len(insts) {
		major, minor := int64(wildcardNum), int64(wildcardNum)
		rule := DeviceRule{Type: "a", Access: "rwm", Major: &major, Minor: &minor}

		if isJumpImm(insts[idx], asm.JNE, asm.R2) {
			switch insts[idx].Constant {
			case int64(unix.BPF_DEVCG_DEV_CHAR):
				rule.Type = "c"
			case int64(unix.BPF_DEVCG_DEV_BLOCK):
				rule.Type = "b"
			default:
				return nil, fmt.Errorf("%w: device type %d", errRuleBlock, insts[idx].Constant)
			}

			idx++
		}

		if idx < len(insts) && insts[idx].OpCode == asm.Mov.Op32(asm.RegSource) &&
			insts[idx].Dst == asm.R6 {
			idx++

			and, ok := next()
			if !ok || and.OpCode != asm.And.Op32(asm.ImmSource) || and.Dst != asm.R6 {
				return nil, fmt.Errorf("%w: access mask", errRuleBlock)
			}

			rule.Access = bpfAccessString(and.Constant)

			_, ok = next()
			if !ok {
				return nil, fmt.Errorf("%w: access check", errRuleBlock)
			}
		}

		if idx < len(insts) && isJumpImm(insts[idx], asm.JNE, asm.R4) {
			major = insts[idx].Constant
			idx++
		}

		if idx < len(insts) && isJumpImm(insts[idx], asm.JNE, asm.R5) {
			minor = insts[idx].Constant
			idx++
		}

		verdict, ok := next()
		if !ok {
			return nil, fmt.Errorf("%w: missing verdict", errRuleBlock)
		}

		value, isMarker := markerValue(verdict)
		if !isMarker || value > 1 {
			return nil, fmt.Errorf("%w: verdict", errRuleBlock)
		}

		rule.Allow = value == 1

		exit, ok := next()
		if !ok || exit.OpCode != asm.Exit.Op(asm.ImmSource) {
			return nil, fmt.Errorf("%w: missing exit", errRuleBlock)
		}

		rules = append(rules, rule)
	}

	// appendDevice is fed the rules last to first.
	slices.Reverse(rules)

	return rules, nil
}

// bpfAccessString turns a BPF_DEVCG_ACC_* mask into "rwm" letters.
func bpfAccessString(mask int64) string {
	var out []byte

	if mask&unix.BPF_DEVCG_ACC_READ != 0 {
		out = append(out, 'r')
	}

	if mask&unix.BPF_DEVCG_ACC_WRITE != 0 {
		out = append(out, 'w')
	}

	if mask&unix.BPF_DEVCG_ACC_MKNOD != 0 {
		out = append(out, 'm')
	}

	return string(out)
}

// progMeta is what the gate needs to know about a program besides its code.
type progMeta struct {
	// mapIDsKnown reports whether the kernel told us the program's map IDs.
	mapIDsKnown bool
	mapCount    int
}

// checkWrappable is the reloadability gate. A translated dump is only
// guaranteed to load back as the same program when it uses no maps, calls
// or wide loads (whose kernel-side rewrites the dump does not undo), so
// wrapping is limited to the instruction set device filters actually use:
// context loads, simple ALU, conditional and unconditional jumps within the
// program, and exit.
func checkWrappable(insts asm.Instructions, meta progMeta) error {
	if !meta.mapIDsKnown {
		return fmt.Errorf("%w: map IDs unavailable", ErrProgramNotWrappable)
	}

	if meta.mapCount > 0 {
		return fmt.Errorf("%w: program uses %d maps", ErrProgramNotWrappable, meta.mapCount)
	}

	if len(insts) == 0 {
		return fmt.Errorf("%w: empty program", ErrProgramNotWrappable)
	}

	for idx, ins := range insts {
		err := checkInstruction(ins)
		if err != nil {
			return fmt.Errorf("%w: instruction %d (%v): %w", ErrProgramNotWrappable, idx, ins, err)
		}

		if ins.OpCode.Class().IsJump() && ins.OpCode.JumpOp() != asm.Exit {
			target := idx + int(ins.Offset) + 1
			if target < 0 || target >= len(insts) {
				return fmt.Errorf(
					"%w: instruction %d jumps out of the program",
					ErrProgramNotWrappable,
					idx,
				)
			}
		}
	}

	return nil
}

var errOutsideSubset = errors.New("outside the wrappable instruction subset")

func checkInstruction(ins asm.Instruction) error {
	opCode := ins.OpCode

	//nolint:exhaustive // an allow list: every other class is rejected by the default branch
	switch opCode.Class() {
	case asm.LdXClass:
		if opCode.Mode() != asm.MemMode || ins.Src != asm.R1 {
			return errOutsideSubset
		}

		switch ins.Offset {
		case 0, 4, 8:
			return nil
		default:
			return errOutsideSubset
		}

	case asm.ALUClass, asm.ALU64Class:
		//nolint:exhaustive // an allow list: every other ALU op is rejected by the default branch
		switch opCode.ALUOp() {
		case asm.Mov, asm.And, asm.Or, asm.RSh, asm.LSh:
			if ins.Offset != 0 {
				// A non-zero offset turns Mov into a sign-extending move.
				return errOutsideSubset
			}

			return nil
		default:
			return errOutsideSubset
		}

	case asm.JumpClass, asm.Jump32Class:
		return checkJump(opCode)

	default:
		return errOutsideSubset
	}
}

func checkJump(opCode asm.OpCode) error {
	//nolint:exhaustive // an allow list: Call, JCOND (may_goto) and anything unknown are rejected
	switch opCode.JumpOp() {
	case asm.JEq, asm.JNE, asm.JGT, asm.JGE, asm.JLT, asm.JLE, asm.JSet,
		asm.JSGT, asm.JSGE, asm.JSLT, asm.JSLE:
		return nil
	case asm.Exit, asm.Ja:
		// JMP32 Exit is not an instruction, and JMP32 Ja is a long jump with
		// the offset in the immediate.
		if opCode.Class() != asm.JumpClass {
			return errOutsideSubset
		}

		return nil
	default:
		return errOutsideSubset
	}
}
