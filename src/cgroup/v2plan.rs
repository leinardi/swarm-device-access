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

//! The cgroup v2 pass, planned before anything changes: every attached program classified, the
//! wrappers grouped by lineage, the grant or restore plan, the rebuild of lineages a wipe lost
//! from the [`FilterCache`], and the execution that loads every new program before the first
//! attach.

use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::sync::{Mutex, PoisonError};

use super::api::DeviceRule;
use super::error::Error;
use super::handle::Identity;
use super::owned::{
    OWNED_PROGRAM_NAME, ProgMeta, canonical_bytes, check_wrappable, emit_owned, from_canonical,
    new_nonce, parse_owned_innermost,
};
use super::v2::CgroupV2;
use super::v2ops::{F_ALLOW_MULTI, V2Ops};
use crate::logger::{Attr, LogHandle};

/// Remembers, per cgroup identity, the runtime originals (and the wrapper nonce of each lineage)
/// that were attached the last time this daemon looked. systemd's daemon-reload can detach every
/// device filter of a cgroup; the programs themselves then carry nothing to recover from, and this
/// cache is the only way to put the runtime's restrictions back, with or without this daemon's
/// grants. It lives only as long as the process: a daemon that did not see a cgroup before its
/// filter was wiped cannot rebuild it.
///
/// Entries are keyed by [`Identity`], so a container restarted with the same ID (a new cgroup
/// inode) starts empty; [`FilterCache::forget`] drops an entry when its lifecycle ends.
#[derive(Debug, Default)]
pub struct FilterCache {
    entries: Mutex<HashMap<Identity, CacheEntry>>,
}

impl FilterCache {
    /// An empty cache.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Drops what is cached for `id`.
    pub fn forget(&self, id: &Identity) {
        self.lock().remove(id);
    }

    pub(super) fn get(&self, id: &Identity) -> CacheEntry {
        self.lock().get(id).cloned().unwrap_or_default()
    }

    pub(super) fn set(&self, id: &Identity, entry: CacheEntry) {
        self.lock().insert(id.clone(), entry);
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<Identity, CacheEntry>> {
        // A poisoned lock is only reachable in tests (panic = "abort"); every entry is replaced
        // whole, so the map is consistent either way.
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// What is known about one cgroup. `rebuilding` is set while a rebuild has not completed: until
/// it has, lineages absent from the cgroup were lost to the wipe (not removed by the runtime) and
/// are reattached by the next pass instead of being dropped from the entry.
#[derive(Debug, Clone, Default)]
pub(super) struct CacheEntry {
    pub(super) lineages: Vec<CachedLineage>,
    pub(super) rebuilding: bool,
}

/// One runtime original and the nonce of the wrapper lineage around it (0 when it has never been
/// wrapped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct CachedLineage {
    pub(super) nonce: u64,
    pub(super) original: Vec<u8>,
    pub(super) meta: ProgMeta,
}

/// One attached program, classified. `index` is its position in the query's handles.
#[derive(Debug, Clone)]
pub(super) struct AttachedProg {
    pub(super) index: usize,
    pub(super) raw: Vec<u8>,
    pub(super) meta: ProgMeta,
    /// For validated wrappers: the lineage nonce and the innermost original.
    pub(super) owned: bool,
    pub(super) nonce: u64,
    pub(super) original: Vec<u8>,
}

/// The wrappers of one lineage: same validated nonce and byte-identical original. Two identical
/// runtime originals wrapped separately have different nonces and are never merged.
#[derive(Debug)]
pub(super) struct WrapperGroup {
    pub(super) nonce: u64,
    pub(super) original: Vec<u8>,
    pub(super) meta: ProgMeta,
    pub(super) members: Vec<AttachedProg>,
}

/// One mutation. `old` indexes the query's handles, `new` the plan's loads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Step {
    /// Attaches a new program on its own (a rebuild).
    Attach { new: usize },
    /// Detaches an attached program.
    Detach { old: usize },
    /// Puts a new program in place of an attached one.
    Pair { old: usize, new: usize },
}

/// The complete set of changes for one cgroup, built before anything is loaded or attached.
#[derive(Debug, Default)]
pub(super) struct PassPlan {
    pub(super) loads: Vec<Vec<u8>>,
    pub(super) names: Vec<&'static str>,
    pub(super) steps: Vec<Step>,
    pub(super) lineages: Vec<CachedLineage>,
}

impl PassPlan {
    fn load(&mut self, raw: Vec<u8>, name: &'static str) -> usize {
        self.loads.push(raw);
        self.names.push(name);
        self.loads.len() - 1
    }

    fn pair(&mut self, old: usize, raw: Vec<u8>, name: &'static str) {
        let new = self.load(raw, name);
        self.steps.push(Step::Pair { old, new });
    }

    fn detach(&mut self, old: usize) {
        self.steps.push(Step::Detach { old });
    }

    /// Adds, for each cached lineage, an attach of its original: bare for empty rules, wrapped
    /// under the lineage nonce otherwise.
    pub(super) fn add_rebuild(
        &mut self,
        cached: Vec<CachedLineage>,
        rules: &[DeviceRule],
    ) -> Result<(), Error> {
        for mut lineage in cached {
            gate(&lineage.original, lineage.meta)?;
            if rules.is_empty() {
                let new = self.load(lineage.original.clone(), "");
                self.steps.push(Step::Attach { new });
                self.lineages.push(lineage);
                continue;
            }
            if lineage.nonce == 0 {
                lineage.nonce = new_nonce().map_err(Error::Nonce)?;
            }
            let wrapped =
                emit_owned(rules, lineage.nonce, &lineage.original).map_err(Error::Emit)?;
            let new = self.load(wrapped, OWNED_PROGRAM_NAME);
            self.steps.push(Step::Attach { new });
            self.lineages.push(lineage);
        }
        Ok(())
    }
}

impl<O: V2Ops> CgroupV2<O> {
    /// Parses one attached program. Wrappers are stripped to the innermost original, so a
    /// wrapper around a wrapper is re-emitted as one.
    pub(super) fn classify(&self, index: usize, prog: &O::Prog) -> Result<AttachedProg, Error> {
        let (instructions, meta) = self.ops.instructions(prog).map_err(Error::Ops)?;
        let raw = canonical_bytes(&instructions).map_err(Error::Canonical)?;
        let Some(owned) = parse_owned_innermost(&raw).map_err(Error::Owned)? else {
            return Ok(AttachedProg {
                index,
                original: raw.clone(),
                raw,
                meta,
                owned: false,
                nonce: 0,
            });
        };
        gate(&owned.original, meta)?;
        Ok(AttachedProg {
            index,
            raw,
            meta,
            owned: true,
            nonce: owned.nonce,
            original: owned.original,
        })
    }

    /// Loads every new program, then applies the steps in order. It stops at the first failure:
    /// attach always precedes detach, and under `BPF_F_ALLOW_MULTI` every intermediate state is
    /// at most as wide as the desired one, so the next pass continues from wherever this one
    /// stopped. Every loaded program is closed when it returns.
    pub(super) fn execute(
        &self,
        dir: BorrowedFd<'_>,
        attached: &[O::Prog],
        plan: &PassPlan,
    ) -> Result<(), Error> {
        let mut loaded = Vec::with_capacity(plan.loads.len());
        for (raw, name) in plan.loads.iter().zip(&plan.names) {
            loaded.push(self.load_canonical(raw, name)?);
        }
        let steps = plan.steps.len();
        for (index, step) in plan.steps.iter().enumerate() {
            let outcome = match *step {
                Step::Attach { new } => self
                    .ops
                    .attach(&loaded[new], dir, F_ALLOW_MULTI, None)
                    .map_err(Error::Ops),
                Step::Detach { old } => self.ops.detach(&attached[old], dir).map_err(Error::Ops),
                Step::Pair { old, new } => self.swap(dir, &attached[old], &loaded[new]),
            };
            outcome.map_err(|source| Error::Change {
                step: index + 1,
                steps,
                source: Box::new(source),
            })?;
        }
        Ok(())
    }
}

/// Whether `original` may be wrapped: its instructions decode and stay in the wrappable subset.
pub(super) fn gate(original: &[u8], meta: ProgMeta) -> Result<(), Error> {
    let instructions = from_canonical(original).map_err(Error::Decode)?;
    check_wrappable(&instructions, meta).map_err(Error::Gate)
}

/// Splits classified programs into wrapper lineages (in order of first appearance) and bare
/// programs.
pub(super) fn group_wrappers(progs: Vec<AttachedProg>) -> (Vec<WrapperGroup>, Vec<AttachedProg>) {
    let mut groups: Vec<WrapperGroup> = Vec::new();
    let mut bares = Vec::new();
    for prog in progs {
        if !prog.owned {
            bares.push(prog);
            continue;
        }
        let position = groups
            .iter()
            .position(|group| group.nonce == prog.nonce && group.original == prog.original);
        let index = position.unwrap_or_else(|| {
            groups.push(WrapperGroup {
                nonce: prog.nonce,
                original: prog.original.clone(),
                meta: prog.meta,
                members: Vec::new(),
            });
            groups.len() - 1
        });
        groups[index].members.push(prog);
    }
    (groups, bares)
}

/// Plans a pass for non-empty rules. Each wrapper lineage ends as exactly one wrapper equal to
/// the desired one (an existing equal member is kept, otherwise the first member is replaced by
/// it), and the other members of the lineage are detached after it is in place. Each bare
/// program is wrapped as its own pair under a new nonce, even when an identical original is
/// already wrapped: without IDs a second runtime program and the leftover of a failed rollback
/// look the same, and wrapping both only costs one redundant, identical wrapper.
pub(super) fn plan_grant(
    groups: &[WrapperGroup],
    bares: &[AttachedProg],
    rules: &[DeviceRule],
    cgroup_path: &str,
    log: &LogHandle,
) -> Result<PassPlan, Error> {
    let mut plan = PassPlan::default();
    for group in groups {
        let desired = emit_owned(rules, group.nonce, &group.original).map_err(Error::Emit)?;
        let existing = group
            .members
            .iter()
            .position(|member| member.raw == desired);
        let keep = existing.unwrap_or_else(|| {
            plan.pair(group.members[0].index, desired, OWNED_PROGRAM_NAME);
            0
        });
        for (position, member) in group.members.iter().enumerate() {
            if position != keep {
                plan.detach(member.index);
            }
        }
        plan.lineages.push(CachedLineage {
            nonce: group.nonce,
            original: group.original.clone(),
            meta: group.meta,
        });
    }
    for bare in bares {
        gate(&bare.raw, bare.meta)?;
        let nonce = new_nonce().map_err(Error::Nonce)?;
        let wrapped = emit_owned(rules, nonce, &bare.raw).map_err(Error::Emit)?;
        log.info(
            "wrapping device filter with no prior owned block; pre-upgrade grants, if any, are \
             not managed",
            &[Attr::string("cgroup", cgroup_path)],
        );
        plan.pair(bare.index, wrapped, OWNED_PROGRAM_NAME);
        plan.lineages.push(CachedLineage {
            nonce,
            original: bare.raw.clone(),
            meta: bare.meta,
        });
    }
    Ok(plan)
}

/// Plans a pass for empty rules: every wrapper lineage ends as its bare original. A lineage whose
/// original is already attached bare (from before this pass; one bare satisfies one lineage, so
/// identical runtime programs keep their count) only has its wrappers detached: that is how a
/// restore whose detach failed converges on the next pass. Any other lineage gets its original
/// attached in place of its first wrapper, then the rest detached. Bare programs are left alone.
pub(super) fn plan_restore(groups: &[WrapperGroup], bares: &[AttachedProg]) -> PassPlan {
    let mut plan = PassPlan::default();
    let mut available = vec![true; bares.len()];
    for group in groups {
        let matched = bares
            .iter()
            .zip(&available)
            .position(|(bare, &free)| free && bare.raw == group.original);
        let mut members = group.members.as_slice();
        if let Some(matched) = matched {
            available[matched] = false;
        } else if let Some((first, rest)) = members.split_first() {
            plan.pair(first.index, group.original.clone(), "");
            members = rest;
        }
        for member in members {
            plan.detach(member.index);
        }
        plan.lineages.push(CachedLineage {
            nonce: group.nonce,
            original: group.original.clone(),
            meta: group.meta,
        });
    }
    for (bare, available) in bares.iter().zip(available) {
        if available {
            plan.lineages.push(CachedLineage {
                nonce: 0,
                original: bare.raw.clone(),
                meta: bare.meta,
            });
        }
    }
    plan
}

/// The cached lineages the cgroup does not show: no wrapper of the same nonce and original, and
/// no bare copy of the original left over for it (each attached program accounts for one
/// lineage).
pub(super) fn missing_lineages(
    cached: Vec<CachedLineage>,
    groups: &[WrapperGroup],
    bares: &[AttachedProg],
) -> Vec<CachedLineage> {
    let mut group_used = vec![false; groups.len()];
    let mut bare_used = vec![false; bares.len()];
    let mut missing = Vec::new();
    for lineage in cached {
        let group = groups.iter().zip(&group_used).position(|(group, &used)| {
            !used && group.nonce == lineage.nonce && group.original == lineage.original
        });
        if let Some(group) = group {
            group_used[group] = true;
            continue;
        }
        let bare = bares
            .iter()
            .zip(&bare_used)
            .position(|(bare, &used)| !used && bare.raw == lineage.original);
        if let Some(bare) = bare {
            bare_used[bare] = true;
            continue;
        }
        missing.push(lineage);
    }
    missing
}

#[cfg(test)]
mod tests;
