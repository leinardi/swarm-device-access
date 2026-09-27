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
	"errors"
	"slices"
	"strconv"
	"testing"

	"golang.org/x/sys/unix"
)

// pass runs one SetDeviceRules pass with the given replace support and
// cache, and clears the recorded calls first so each pass is asserted alone.
func pass(ops *fakeOps, replace int32, cache *FilterCache, rules []DeviceRule) error {
	ops.calls = nil
	c := &cgroupv2{ops: ops, replace: probeIn(replace), cache: cache}

	return c.setDeviceRules(3, testIdentity, rules)
}

// attachedPrograms parses every attached program: wrappers by lineage
// nonce, bare programs by their bytes.
func attachedPrograms(
	t *testing.T,
	ops *fakeOps,
) (nonces []uint64, originals [][]byte, bares [][]byte) {
	t.Helper()

	for _, name := range ops.attached {
		raw := attachedRaw(t, ops, name)

		parsed, owned, err := parseOwned(raw)
		if err != nil {
			t.Fatalf("%s: %v", name, err)
		}

		if owned {
			nonces = append(nonces, parsed.nonce)
			originals = append(originals, parsed.original)
		} else {
			bares = append(bares, raw)
		}
	}

	return nonces, originals, bares
}

func grantRules() []DeviceRule {
	return []DeviceRule{rule("c", 10, 200, "rwm")}
}

// TestConverge_WrapRollbackFailureThenRetry: the first wrap attaches N1 but
// neither the original's detach nor the rollback succeed, leaving O1 and N1
// attached. The retry keeps N1 and wraps O1 as its own pair: two identical
// wrappers (one redundant, not wider), no bare program left, and nothing
// changes on later passes.
func TestConverge_WrapRollbackFailureThenRetry(t *testing.T) {
	ops := ownedOps(t, map[string][]byte{"O1": mustCanonical(t, allowNullOriginal())})
	ops.errs["detach O1"] = errInjected
	ops.errs["detach N1"] = errRollback

	err := pass(ops, replaceUnsupported, nil, grantRules())
	if !errors.Is(err, errRollback) {
		t.Fatalf("err = %v, want the joined rollback failure", err)
	}

	assertAttached(t, ops, "O1", "N1")

	ops.errs = map[string]error{}

	err = pass(ops, replaceUnsupported, nil, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops, "load N2", "attach N2", "detach O1")

	nonces, originals, bares := attachedPrograms(t, ops)
	if len(bares) != 0 || len(nonces) != 2 || nonces[0] == nonces[1] ||
		!bytes.Equal(originals[0], originals[1]) {
		t.Fatalf(
			"want two identical-original wrappers with distinct nonces and no bare, got nonces %v bares %d",
			nonces,
			len(bares),
		)
	}

	for range 2 {
		err = pass(ops, replaceUnsupported, nil, grantRules())
		if err != nil {
			t.Fatal(err)
		}

		assertCalls(t, ops)
	}

	ops.assertAllClosed(t)
}

// TestConverge_RollbackSucceededThenRetryIsOneProgramPerOriginal is the 0.1
// case whose rollback worked: pair 1 replaced, pair 2 back to its original;
// the retry ends with exactly one wrapper per runtime original.
func TestConverge_RollbackSucceededThenRetryIsOneProgramPerOriginal(t *testing.T) {
	origA := mustCanonical(t, allowNullOriginal())
	origB := mustCanonical(t, runtimeOriginal())
	ops := ownedOps(t, map[string][]byte{"O1": origA, "O2": origB})
	ops.errs["detach O2"] = errInjected

	err := pass(ops, replaceUnsupported, nil, grantRules())
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v", err)
	}

	assertAttached(t, ops, "O2", "N1")

	ops.errs = map[string]error{}

	err = pass(ops, replaceUnsupported, nil, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	_, originals, bares := attachedPrograms(t, ops)
	if len(bares) != 0 || len(originals) != 2 || bytes.Equal(originals[0], originals[1]) {
		t.Fatalf(
			"want one wrapper per original, got %d wrappers %d bares",
			len(originals),
			len(bares),
		)
	}

	err = pass(ops, replaceUnsupported, nil, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops)
}

func TestConverge_IdenticalBaresBecomeTwoWrappersEachByItsOwnPair(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	ops := ownedOps(t, map[string][]byte{"O1": orig, "O2": orig})

	err := pass(ops, replaceUnsupported, nil, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	// Each bare program is detached only right after its own wrapper is in.
	assertCalls(t, ops, "load N1", "load N2", "attach N1", "detach O1", "attach N2", "detach O2")

	nonces, _, bares := attachedPrograms(t, ops)
	if len(bares) != 0 || len(nonces) != 2 || nonces[0] == nonces[1] {
		t.Fatalf("want two wrappers with distinct nonces, got %v (bares %d)", nonces, len(bares))
	}
}

func wrapperOf(t *testing.T, rules []DeviceRule, nonce uint64) []byte {
	t.Helper()

	return mustEmit(t, rules, nonce, mustCanonical(t, allowNullOriginal()))
}

func TestConverge_EmptyRestoreDetachFailureThenRetry(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	ops := ownedOps(t, map[string][]byte{"W1": wrapperOf(t, grantRules(), 11)})
	ops.errs["detach W1"] = errInjected

	err := pass(ops, replaceUnsupported, nil, nil)
	if !errors.Is(err, errInjected) {
		t.Fatalf("err = %v", err)
	}

	// The rollback worked: only the wrapper is attached.
	assertAttached(t, ops, "W1")

	ops.errs = map[string]error{}

	err = pass(ops, replaceUnsupported, nil, nil)
	if err != nil {
		t.Fatal(err)
	}

	nonces, _, bares := attachedPrograms(t, ops)
	if len(nonces) != 0 || len(bares) != 1 || !bytes.Equal(bares[0], orig) {
		t.Fatalf("want a single bare original, got %d wrappers %d bares", len(nonces), len(bares))
	}
}

// TestConverge_EmptyRestoreRollbackFailureThenRetry: the bare original is
// attached, the wrapper's detach fails and so does the rollback of the
// original. The retry sees the original already bare and only detaches the
// wrapper (removing a wrapper next to its own original only narrows).
func TestConverge_EmptyRestoreRollbackFailureThenRetry(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	ops := ownedOps(t, map[string][]byte{"W1": wrapperOf(t, grantRules(), 11)})
	ops.errs["detach W1"] = errInjected
	ops.errs["detach N1"] = errRollback

	err := pass(ops, replaceUnsupported, nil, nil)
	if !errors.Is(err, errRollback) {
		t.Fatalf("err = %v", err)
	}

	assertAttached(t, ops, "W1", "N1")

	ops.errs = map[string]error{}

	err = pass(ops, replaceUnsupported, nil, nil)
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops, "detach W1")
	assertAttached(t, ops, "N1")

	if !bytes.Equal(attachedRaw(t, ops, "N1"), orig) {
		t.Error("remaining program is not the bare original")
	}

	err = pass(ops, replaceUnsupported, nil, nil)
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops)
}

// TestConverge_EmptyRestoreKeepsIdenticalRuntimeProgramCount checks that
// one bare program satisfies one lineage: two identical originals, each
// wrapped, are restored as two bare programs.
func TestConverge_EmptyRestoreKeepsIdenticalRuntimeProgramCount(t *testing.T) {
	ops := ownedOps(t, map[string][]byte{
		"W1": wrapperOf(t, grantRules(), 11),
		"W2": wrapperOf(t, grantRules(), 12),
	})

	err := pass(ops, replaceSupported, nil, nil)
	if err != nil {
		t.Fatal(err)
	}

	nonces, _, bares := attachedPrograms(t, ops)
	if len(nonces) != 0 || len(bares) != 2 {
		t.Fatalf("want two bare originals, got %d wrappers %d bares", len(nonces), len(bares))
	}
}

func TestConverge_StaleSameNonceWrappersLeaveOne(t *testing.T) {
	older := []DeviceRule{rule("c", 10, 201, "r")}

	t.Run("none equal to desired", func(t *testing.T) {
		ops := ownedOps(
			t,
			map[string][]byte{"W1": wrapperOf(t, older, 11), "W2": wrapperOf(t, nil, 11)},
		)

		err := pass(ops, replaceUnsupported, nil, grantRules())
		if err != nil {
			t.Fatal(err)
		}

		assertCalls(t, ops, "load N1", "attach N1", "detach W1", "detach W2")
		assertAttached(t, ops, "N1")
	})

	t.Run("one already equal", func(t *testing.T) {
		ops := ownedOps(
			t,
			map[string][]byte{"W1": wrapperOf(t, older, 11), "W2": wrapperOf(t, grantRules(), 11)},
		)

		err := pass(ops, replaceUnsupported, nil, grantRules())
		if err != nil {
			t.Fatal(err)
		}

		assertCalls(t, ops, "detach W1")
		assertAttached(t, ops, "W2")
	})
}

func TestConverge_IdenticalWrappersWithDifferentNoncesBothRemain(t *testing.T) {
	ops := ownedOps(t, map[string][]byte{
		"W1": wrapperOf(t, grantRules(), 11),
		"W2": wrapperOf(t, grantRules(), 12),
	})

	err := pass(ops, replaceUnsupported, nil, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops)
	assertAttached(t, ops, "W1", "W2")
}

func TestConverge_WrapperOverWrapperIsNormalised(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	inner := mustEmit(t, []DeviceRule{rule("c", 10, 201, "r")}, 21, orig)
	ops := ownedOps(t, map[string][]byte{"W1": mustEmit(t, grantRules(), 22, inner)})

	err := pass(ops, replaceSupported, nil, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	if !bytes.Equal(attachedRaw(t, ops, "N1"), mustEmit(t, grantRules(), 22, orig)) {
		t.Error(
			"wrapper over a wrapper was not re-emitted as one wrapper over the innermost original",
		)
	}
}

func TestConverge_WipeAfterWrapRebuildsSameLineage(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	cache := &FilterCache{}
	ops := ownedOps(t, map[string][]byte{"O1": orig})

	err := pass(ops, replaceSupported, cache, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	nonces, _, _ := attachedPrograms(t, ops)

	ops.attached = nil // systemd daemon-reload

	err = pass(ops, replaceSupported, cache, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops, "load N2", "attach N2")

	rebuiltNonces, originals, _ := attachedPrograms(t, ops)
	if len(rebuiltNonces) != 1 || rebuiltNonces[0] != nonces[0] ||
		!bytes.Equal(originals[0], orig) {
		t.Fatalf("rebuilt lineage %v, want nonce %v around the original", rebuiltNonces, nonces)
	}

	raw := attachedRaw(t, ops, "N2")
	if runDeviceFilter(t, raw, charReq(10, 200, unix.BPF_DEVCG_ACC_READ)) != 1 ||
		runDeviceFilter(t, raw, charReq(10, 201, unix.BPF_DEVCG_ACC_READ)) != 0 {
		t.Error("rebuilt wrapper does not carry exactly the desired grant over the original")
	}
}

func TestConverge_WipeWithEmptyRulesRestoresBareOriginal(t *testing.T) {
	orig := mustCanonical(t, allowNullOriginal())
	cache := &FilterCache{}
	ops := ownedOps(t, map[string][]byte{"O1": orig})

	err := pass(ops, replaceSupported, cache, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	ops.attached = nil

	err = pass(ops, replaceSupported, cache, nil)
	if err != nil {
		t.Fatal(err)
	}

	assertCalls(t, ops, "load N2", "attach N2")

	if !bytes.Equal(attachedRaw(t, ops, "N2"), orig) {
		t.Error("wipe with empty rules did not restore the bare runtime original")
	}
}

func TestConverge_ForgottenCacheCannotRebuild(t *testing.T) {
	cache := &FilterCache{}
	ops := ownedOps(t, map[string][]byte{"O1": mustCanonical(t, allowNullOriginal())})

	err := pass(ops, replaceSupported, cache, grantRules())
	if err != nil {
		t.Fatal(err)
	}

	cache.Forget(testIdentity)

	ops.attached = nil

	err = pass(ops, replaceSupported, cache, grantRules())
	if !errors.Is(err, ErrFilterMissing) {
		t.Fatalf("err = %v, want ErrFilterMissing", err)
	}

	assertCalls(t, ops)
}

func TestConverge_CacheIsKeyedByIdentity(t *testing.T) {
	cache := &FilterCache{}
	cache.set(testIdentity, cacheEntry{lineages: []cachedLineage{{original: []byte{1}}}})

	restarted := testIdentity
	restarted.Inode++

	if got := cache.get(restarted).lineages; len(got) != 0 {
		t.Errorf("a new cgroup inode at the same path sees %d cached lineages, want none", len(got))
	}
}

// TestConverge_PartialRebuildResumes: the cgroup had two runtime programs,
// the wipe removed both, and the rebuild attaches the first but fails on the
// second. The retry sees one program attached (not a wiped cgroup) and must
// still reattach the lost lineage instead of forgetting it.
func TestConverge_PartialRebuildResumes(t *testing.T) {
	origA := mustCanonical(t, allowNullOriginal())
	origB := mustCanonical(t, runtimeOriginal())
	cache := &FilterCache{}
	ops := ownedOps(t, map[string][]byte{"O1": origA, "O2": origB})

	for _, rules := range [][]DeviceRule{grantRules(), nil} {
		err := pass(ops, replaceSupported, cache, grantRules())
		if err != nil {
			t.Fatal(err)
		}

		ops.attached = nil
		ops.errs = map[string]error{}
		failing := "attach N" + strconv.Itoa(ops.loads+2)
		ops.errs[failing] = errInjected

		err = pass(ops, replaceSupported, cache, rules)
		if !errors.Is(err, errInjected) {
			t.Fatalf("rules %v: err = %v, want the injected %s failure", rules, err, failing)
		}

		if len(ops.attached) != 1 {
			t.Fatalf("rules %v: attached = %q, want the first lineage only", rules, ops.attached)
		}

		ops.errs = map[string]error{}

		err = pass(ops, replaceSupported, cache, rules)
		if err != nil {
			t.Fatal(err)
		}

		var got [][]byte

		nonces, originals, bares := attachedPrograms(t, ops)
		got = append(append(got, originals...), bares...)

		if len(got) != 2 ||
			!slices.ContainsFunc(got, func(b []byte) bool { return bytes.Equal(b, origA) }) ||
			!slices.ContainsFunc(got, func(b []byte) bool { return bytes.Equal(b, origB) }) {
			t.Fatalf(
				"rules %v: after the retry want both runtime originals, got %d wrappers %d bares",
				rules,
				len(nonces),
				len(bares),
			)
		}

		if cache.get(testIdentity).rebuilding {
			t.Errorf("rules %v: rebuild still pending after a complete pass", rules)
		}

		err = pass(ops, replaceSupported, cache, rules)
		if err != nil {
			t.Fatal(err)
		}

		assertCalls(t, ops)
	}
}
