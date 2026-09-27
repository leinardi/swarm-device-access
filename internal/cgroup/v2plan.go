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
	"fmt"
	"slices"
	"sync"

	"github.com/leinardi/swarm-device-access/internal/logger"
)

// FilterCache remembers, per cgroup identity, the runtime originals (and
// the wrapper nonce of each lineage) that were attached the last time this
// daemon looked. systemd's daemon-reload can detach every device filter of
// a cgroup; the programs themselves then carry nothing to recover from, and
// this cache is the only way to put the runtime's restrictions back, with
// or without this daemon's grants. It lives only as long as the process: a
// daemon that did not see a cgroup before its filter was wiped cannot
// rebuild it.
//
// Entries are keyed by Identity, so a container restarted with the same ID
// (a new cgroup inode) starts empty; Forget drops an entry when its
// lifecycle ends. The zero value is ready to use.
type FilterCache struct {
	mu      sync.Mutex
	entries map[Identity]cacheEntry
}

// cacheEntry is what is known about one cgroup. rebuilding is set while a
// rebuild has not completed: until it has, lineages absent from the cgroup
// were lost to the wipe (not removed by the runtime) and are reattached by
// the next pass instead of being dropped from the entry.
type cacheEntry struct {
	lineages   []cachedLineage
	rebuilding bool
}

// cachedLineage is one runtime original and the nonce of the wrapper
// lineage around it (0 when it has never been wrapped).
type cachedLineage struct {
	nonce    uint64
	original []byte
	meta     progMeta
}

// Forget drops what is cached for id.
func (f *FilterCache) Forget(id Identity) {
	if f == nil {
		return
	}

	f.mu.Lock()
	defer f.mu.Unlock()

	delete(f.entries, id)
}

func (f *FilterCache) get(id Identity) cacheEntry {
	if f == nil {
		return cacheEntry{}
	}

	f.mu.Lock()
	defer f.mu.Unlock()

	entry := f.entries[id]
	entry.lineages = slices.Clone(entry.lineages)

	return entry
}

func (f *FilterCache) set(id Identity, entry cacheEntry) {
	if f == nil {
		return
	}

	f.mu.Lock()
	defer f.mu.Unlock()

	if f.entries == nil {
		f.entries = make(map[Identity]cacheEntry)
	}

	f.entries[id] = entry
}

// attachedProg is one attached program, classified.
type attachedProg struct {
	handle progHandle
	raw    []byte
	meta   progMeta

	// For validated wrappers: the lineage nonce and the innermost original.
	owned    bool
	nonce    uint64
	original []byte
}

// wrapperGroup holds the wrappers of one lineage: same validated nonce and
// byte-identical original. Two identical runtime originals wrapped
// separately have different nonces and are never merged.
type wrapperGroup struct {
	nonce    uint64
	original []byte
	meta     progMeta
	members  []attachedProg
}

// step is one mutation. old nil attaches newIdx on its own (rebuild); newIdx
// -1 detaches old; otherwise newIdx replaces old as a pair.
type step struct {
	old    progHandle
	newIdx int
}

// passPlan is the complete set of changes for one cgroup, built before
// anything is loaded or attached.
type passPlan struct {
	loads    [][]byte
	names    []string
	steps    []step
	lineages []cachedLineage
}

func (p *passPlan) load(raw []byte, name string) int {
	p.loads = append(p.loads, raw)
	p.names = append(p.names, name)

	return len(p.loads) - 1
}

func (p *passPlan) pair(old progHandle, raw []byte, name string) {
	p.steps = append(p.steps, step{old: old, newIdx: p.load(raw, name)})
}

func (p *passPlan) detach(old progHandle) {
	p.steps = append(p.steps, step{old: old, newIdx: -1})
}

// classify parses one attached program. Wrappers are stripped to the
// innermost original, so a wrapper around a wrapper is re-emitted as one.
func (c *cgroupv2) classify(prog progHandle) (attachedProg, error) {
	insts, meta, err := c.ops.instructions(prog)
	if err != nil {
		return attachedProg{}, err
	}

	raw, err := canonicalBytes(insts)
	if err != nil {
		return attachedProg{}, err
	}

	owned, isOwned, err := parseOwned(raw)
	if err != nil {
		return attachedProg{}, err
	}

	if !isOwned {
		return attachedProg{handle: prog, raw: raw, meta: meta, original: raw}, nil
	}

	original := owned.original

	for {
		inner, innerOwned, innerErr := parseOwned(original)
		if innerErr != nil {
			return attachedProg{}, innerErr
		}

		if !innerOwned {
			break
		}

		original = inner.original
	}

	err = gate(original, meta)
	if err != nil {
		return attachedProg{}, err
	}

	return attachedProg{
		handle: prog, raw: raw, meta: meta,
		owned: true, nonce: owned.nonce, original: original,
	}, nil
}

func gate(original []byte, meta progMeta) error {
	insts, err := fromCanonical(original)
	if err != nil {
		return err
	}

	return checkWrappable(insts, meta)
}

// groupWrappers splits classified programs into wrapper lineages (in order
// of first appearance) and bare programs.
func groupWrappers(progs []attachedProg) ([]wrapperGroup, []attachedProg) {
	var (
		groups []wrapperGroup
		bares  []attachedProg
	)

	for _, prog := range progs {
		if !prog.owned {
			bares = append(bares, prog)

			continue
		}

		idx := slices.IndexFunc(groups, func(g wrapperGroup) bool {
			return g.nonce == prog.nonce && bytes.Equal(g.original, prog.original)
		})
		if idx < 0 {
			groups = append(
				groups,
				wrapperGroup{nonce: prog.nonce, original: prog.original, meta: prog.meta},
			)
			idx = len(groups) - 1
		}

		groups[idx].members = append(groups[idx].members, prog)
	}

	return groups, bares
}

// planGrant plans a pass for non-empty rules. Each wrapper lineage ends as
// exactly one wrapper equal to the desired one (an existing equal member is
// kept, otherwise the first member is replaced by it), and the other
// members of the lineage are detached after it is in place. Each bare
// program is wrapped as its own pair under a new nonce, even when an
// identical original is already wrapped: without IDs a second runtime
// program and the leftover of a failed rollback look the same, and wrapping
// both only costs one redundant, identical wrapper.
func planGrant(
	groups []wrapperGroup,
	bares []attachedProg,
	rules []DeviceRule,
	cgroupPath string,
) (passPlan, error) {
	var plan passPlan

	for _, group := range groups {
		desired, err := emitOwned(rules, group.nonce, group.original)
		if err != nil {
			return passPlan{}, err
		}

		keep := slices.IndexFunc(
			group.members,
			func(m attachedProg) bool { return bytes.Equal(m.raw, desired) },
		)
		if keep < 0 {
			plan.pair(group.members[0].handle, desired, ownedProgramName)

			keep = 0
		}

		for idx, member := range group.members {
			if idx != keep {
				plan.detach(member.handle)
			}
		}

		plan.lineages = append(
			plan.lineages,
			cachedLineage{nonce: group.nonce, original: group.original, meta: group.meta},
		)
	}

	for _, bare := range bares {
		err := gate(bare.raw, bare.meta)
		if err != nil {
			return passPlan{}, err
		}

		nonce, err := newNonce()
		if err != nil {
			return passPlan{}, err
		}

		wrapped, err := emitOwned(rules, nonce, bare.raw)
		if err != nil {
			return passPlan{}, err
		}

		logger.L().
			Info("wrapping device filter with no prior owned block; pre-upgrade grants, if any, are not managed",
				"cgroup", cgroupPath)

		plan.pair(bare.handle, wrapped, ownedProgramName)
		plan.lineages = append(
			plan.lineages,
			cachedLineage{nonce: nonce, original: bare.raw, meta: bare.meta},
		)
	}

	return plan, nil
}

// planRestore plans a pass for empty rules: every wrapper lineage ends as
// its bare original. A lineage whose original is already attached bare
// (from before this pass; one bare satisfies one lineage, so identical
// runtime programs keep their count) only has its wrappers detached: that
// is how a restore whose detach failed converges on the next pass. Any
// other lineage gets its original attached in place of its first wrapper,
// then the rest detached. Bare programs are left alone.
func planRestore(groups []wrapperGroup, bares []attachedProg) passPlan {
	var plan passPlan

	available := make([]bool, len(bares))
	for idx := range available {
		available[idx] = true
	}

	for _, group := range groups {
		match := -1

		for idx, bare := range bares {
			if available[idx] && bytes.Equal(bare.raw, group.original) {
				match = idx

				break
			}
		}

		members := group.members

		if match >= 0 {
			available[match] = false
		} else {
			plan.pair(members[0].handle, group.original, "")
			members = members[1:]
		}

		for _, member := range members {
			plan.detach(member.handle)
		}

		plan.lineages = append(
			plan.lineages,
			cachedLineage{nonce: group.nonce, original: group.original, meta: group.meta},
		)
	}

	for idx, bare := range bares {
		if available[idx] {
			plan.lineages = append(
				plan.lineages,
				cachedLineage{original: bare.raw, meta: bare.meta},
			)
		}
	}

	return plan
}

// addRebuild adds, for each cached lineage, an attach of its original:
// bare for empty rules, wrapped under the lineage nonce otherwise.
func (p *passPlan) addRebuild(cached []cachedLineage, rules []DeviceRule) error {
	for _, lineage := range cached {
		err := gate(lineage.original, lineage.meta)
		if err != nil {
			return err
		}

		if len(rules) == 0 {
			p.steps = append(p.steps, step{newIdx: p.load(lineage.original, "")})
			p.lineages = append(p.lineages, lineage)

			continue
		}

		if lineage.nonce == 0 {
			lineage.nonce, err = newNonce()
			if err != nil {
				return err
			}
		}

		wrapped, err := emitOwned(rules, lineage.nonce, lineage.original)
		if err != nil {
			return err
		}

		p.steps = append(p.steps, step{newIdx: p.load(wrapped, ownedProgramName)})
		p.lineages = append(p.lineages, lineage)
	}

	return nil
}

// missingLineages returns the cached lineages the cgroup does not show: no
// wrapper of the same nonce and original, and no bare copy of the original
// left over for it (each attached program accounts for one lineage).
func missingLineages(
	cached []cachedLineage,
	groups []wrapperGroup,
	bares []attachedProg,
) []cachedLineage {
	groupUsed := make([]bool, len(groups))
	bareUsed := make([]bool, len(bares))

	var missing []cachedLineage

	for _, lineage := range cached {
		found := false

		for idx, group := range groups {
			if !groupUsed[idx] && group.nonce == lineage.nonce &&
				bytes.Equal(group.original, lineage.original) {
				groupUsed[idx], found = true, true

				break
			}
		}

		for idx, bare := range bares {
			if found {
				break
			}

			if !bareUsed[idx] && bytes.Equal(bare.raw, lineage.original) {
				bareUsed[idx], found = true, true
			}
		}

		if !found {
			missing = append(missing, lineage)
		}
	}

	return missing
}

// execute loads every new program, then applies the steps in order. It
// stops at the first failure: attach always precedes detach, and under
// BPF_F_ALLOW_MULTI every intermediate state is at most as wide as the
// desired one, so the next pass continues from wherever this one stopped.
func (c *cgroupv2) execute(dirFD int, plan passPlan) error {
	loaded := make([]progHandle, 0, len(plan.loads))
	defer func() { closeAll(loaded) }()

	for idx, raw := range plan.loads {
		prog, err := c.loadCanonical(raw, plan.names[idx])
		if err != nil {
			return err
		}

		loaded = append(loaded, prog)
	}

	for idx, st := range plan.steps {
		var err error

		switch {
		case st.old == nil:
			err = c.ops.attach(loaded[st.newIdx], dirFD, multiFlag, nil)
		case st.newIdx < 0:
			err = c.ops.detach(st.old, dirFD)
		default:
			err = c.swap(dirFD, st.old, loaded[st.newIdx])
		}

		if err != nil {
			return fmt.Errorf("device filter change %d of %d: %w", idx+1, len(plan.steps), err)
		}
	}

	return nil
}
