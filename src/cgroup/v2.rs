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

// Modified by Roberto Leinardi in 2026: ported to Rust from internal/cgroup/v2.go.

//! cgroup v2: where the unified hierarchy is mounted and the process's cgroup in it, and the
//! controller ([`CgroupV2`]) that keeps the daemon's grants in the cgroup's device filters, with
//! the planning in `v2plan.rs` and the kernel operations in `v2ops.rs`.

use std::os::fd::BorrowedFd;
use std::sync::Arc;

use super::api::{DeviceRule, Interface, ParseError, scan_lines};
use super::error::Error;
use super::handle::{CgroupHandle, Identity};
use super::owned::from_canonical;
use super::v2ops::{F_ALLOW_MULTI, KernelOps, PROCESS_REPLACE_PROBE, ReplaceProbe, V2Ops};
use super::v2plan::{
    CacheEntry, FilterCache, PassPlan, group_wrappers, missing_lineages, plan_grant, plan_restore,
};
use crate::logger::{Attr, LogHandle};

/// The cgroup v2 device controller: the daemon's grants as a wrapper around each runtime device
/// filter, changed through `ops`.
#[derive(Debug)]
pub struct CgroupV2<O> {
    pub(super) ops: O,
    pub(super) replace: &'static ReplaceProbe,
    pub(super) cache: Arc<FilterCache>,
    pub(super) log: LogHandle,
}

impl CgroupV2<KernelOps> {
    /// The controller on the kernel's `bpf(2)` calls, with the process's replace probe and `cache`,
    /// which must outlive it (one per processor), as Go's `New(2, ledger, cache)` makes it.
    #[must_use]
    pub fn new(cache: Arc<FilterCache>, log: LogHandle) -> Self {
        Self {
            ops: KernelOps::new(log.clone()),
            replace: &PROCESS_REPLACE_PROBE,
            cache,
            log,
        }
    }
}

impl<O: V2Ops> Interface for CgroupV2<O> {
    /// Makes this daemon's grants in the device filters of the cgroup behind the handle equal
    /// exactly the rules.
    ///
    /// Each attached program is either one of this daemon's wrappers (see `owned.rs`), stripped
    /// back to the runtime original it wraps, or an unmarked program, which is a runtime original
    /// itself. Non-empty rules leave one wrapper per lineage around its original; empty rules put
    /// the bare originals back and leave unmarked programs alone, so a cgroup this daemon never
    /// touched is never touched. The programs carry all the state this needs, so it works the same
    /// after a daemon restart; the one exception is a cgroup whose filters were all detached
    /// (systemd daemon-reload), which is rebuilt from the [`FilterCache`] when this process saw it
    /// before.
    ///
    /// Unmarked programs are never interpreted: a wrapper written by a version that predates the
    /// ownership markers looks exactly like a runtime program, so its grants are kept until the
    /// container restarts. Kernel program IDs are reused and are never used as identity; every
    /// equality decision compares full canonical instruction bytes.
    ///
    /// Unlike NVIDIA's upstream code, which detaches every program before attaching the
    /// replacements because it runs strictly before the container starts, this daemon mutates live
    /// containers: detaching first would leave the container unfiltered in between, and permanently
    /// so if the attach failed or the daemon died. Every new program is therefore attached (or
    /// atomically swapped in) before anything it replaces is detached.
    fn set_device_rules(&self, handle: &CgroupHandle, rules: &[DeviceRule]) -> Result<(), Error> {
        self.set_rules(handle.dir(), handle.identity(), rules)
    }
}

impl<O: V2Ops> CgroupV2<O> {
    pub(super) fn set_rules(
        &self,
        dir: BorrowedFd<'_>,
        id: &Identity,
        rules: &[DeviceRule],
    ) -> Result<(), Error> {
        // Every handle the query opened is closed when `attached` goes out of scope.
        let attached = self.ops.query(dir).map_err(Error::Query)?;
        if attached.inaccessible > 0 {
            return Err(Error::FiltersInaccessible {
                inaccessible: attached.inaccessible,
                total: attached.total,
            });
        }
        if attached.total == 0 {
            return self.rebuild(dir, id, rules);
        }
        if attached.attach_flags & F_ALLOW_MULTI == 0 {
            return Err(Error::UnsupportedAttachMode(attached.attach_flags));
        }

        // Classify and plan everything before touching the cgroup: a conflict, an unwrappable
        // program or a load failure anywhere mutates nothing.
        let classified = attached
            .progs
            .iter()
            .enumerate()
            .map(|(index, prog)| self.classify(index, prog))
            .collect::<Result<Vec<_>, _>>()?;
        let (groups, bares) = group_wrappers(classified);
        let mut plan = if rules.is_empty() {
            plan_restore(&groups, &bares)
        } else {
            plan_grant(&groups, &bares, rules, &id.path, &self.log)?
        };

        // A rebuild that stopped halfway left some lineages attached and others not; the missing
        // ones were lost to the wipe, so they are attached again rather than dropped from the
        // cache.
        let entry = self.cache.get(id);
        if entry.rebuilding {
            plan.add_rebuild(missing_lineages(entry.lineages, &groups, &bares), rules)?;
        }
        self.commit(dir, id, &attached.progs, &plan, entry.rebuilding)
    }

    /// Records the plan's lineages and executes it. `rebuilding` stays set in the cache until a
    /// pass that reattaches every lineage has succeeded.
    fn commit(
        &self,
        dir: BorrowedFd<'_>,
        id: &Identity,
        attached: &[O::Prog],
        plan: &PassPlan,
        rebuilding: bool,
    ) -> Result<(), Error> {
        self.cache.set(
            id,
            CacheEntry {
                lineages: plan.lineages.clone(),
                rebuilding,
            },
        );
        self.execute(dir, attached, plan)?;
        if rebuilding {
            self.cache.set(
                id,
                CacheEntry {
                    lineages: plan.lineages.clone(),
                    rebuilding: false,
                },
            );
        }
        Ok(())
    }

    /// Handles a cgroup with no device filter at all. That is what systemd's daemon-reload leaves
    /// behind, so it is not a no-op for a container that had one: its runtime originals are
    /// attached again from the [`FilterCache`]. Without a cached entry nothing can be rebuilt,
    /// and [`Error::FilterMissing`] lets the caller decide (a privileged container never had a
    /// filter; anything else must not be reported as done).
    fn rebuild(
        &self,
        dir: BorrowedFd<'_>,
        id: &Identity,
        rules: &[DeviceRule],
    ) -> Result<(), Error> {
        let entry = self.cache.get(id);
        if entry.lineages.is_empty() {
            return Err(Error::FilterMissing);
        }
        let programs = entry.lineages.len();
        let mut plan = PassPlan::default();
        plan.add_rebuild(entry.lineages, rules)?;
        self.log.info(
            "device filter missing; reattaching the cached runtime originals",
            &[
                Attr::string("cgroup", id.path.as_str()),
                Attr::int("programs", i64::try_from(programs).unwrap_or(i64::MAX)),
                Attr::bool("with_grants", !rules.is_empty()),
            ],
        );
        self.commit(dir, id, &[], &plan, true)
    }

    /// Loads a program from canonical bytes. A verifier rejection is returned as is, before
    /// anything is attached.
    pub(super) fn load_canonical(&self, raw: &[u8], name: &str) -> Result<O::Prog, Error> {
        let instructions = from_canonical(raw).map_err(Error::Decode)?;
        self.ops.load(name, &instructions).map_err(Error::Ops)
    }

    /// Replaces `old` with `new` on `dir`: atomically when the kernel supports `BPF_F_REPLACE`,
    /// otherwise by attaching `new` before detaching `old`. If that detach fails, `new` is
    /// detached again so the pair is left as it was (both attached would make a later pass stack
    /// another copy).
    pub(super) fn swap(
        &self,
        dir: BorrowedFd<'_>,
        old: &O::Prog,
        new: &O::Prog,
    ) -> Result<(), Error> {
        if self.replace.usable() {
            let outcome = self.ops.attach(new, dir, F_ALLOW_MULTI, Some(old));
            self.replace.record(outcome.as_ref().copied());
            match outcome {
                Ok(()) => return Ok(()),
                Err(error) if self.replace.usable() => return Err(Error::Replace(error)),
                Err(_) => {}
            }
        }
        self.ops
            .attach(new, dir, F_ALLOW_MULTI, None)
            .map_err(Error::AttachNew)?;
        let Err(detach_error) = self.ops.detach(old, dir) else {
            return Ok(());
        };
        let detach_error = Error::DetachOriginal(detach_error);
        match self.ops.detach(new, dir) {
            Ok(()) => Err(detach_error),
            Err(rollback_error) => Err(Error::Joined(vec![
                detach_error,
                Error::Rollback(rollback_error),
            ])),
        }
    }
}

/// The mountinfo entry of the unified hierarchy: the mount's root (prefix) and its mount point.
/// The filesystem type is the third field from the end.
pub(super) fn scan_mount_info_v2(
    data: &[u8],
    file: &'static str,
) -> Result<(String, String), ParseError> {
    for line in scan_lines(data, file) {
        let line = line?;
        // Split on single spaces, as Go's strings.Split: empty fields count.
        let parts: Vec<&str> = line.split(' ').collect();
        if parts.len() < 5 {
            return Err(ParseError::MalformedMountinfoEntry(line));
        }
        if parts[parts.len() - 3] != "cgroup2" {
            continue;
        }
        // The mount prefix must not be a relative path.
        if parts[3].starts_with("/..") {
            return Err(ParseError::RelativeMountPrefix(parts[3].to_owned()));
        }
        return Ok((parts[3].to_owned(), parts[4].to_owned()));
    }
    Err(ParseError::NoCgroup2Fs)
}

/// The process's cgroup in the unified hierarchy, with `prefix` stripped unless it is `/`.
pub(super) fn scan_proc_cgroup_v2(
    data: &[u8],
    file: &'static str,
    prefix: &str,
) -> Result<String, ParseError> {
    for line in scan_lines(data, file) {
        let line = line?;
        let parts: Vec<&str> = line.splitn(3, ':').collect();
        let [_, controllers, root] = parts.as_slice() else {
            return Err(ParseError::MalformedCgroupEntry(line));
        };
        if !controllers.is_empty() {
            continue;
        }
        if prefix == "/" {
            return Ok((*root).to_owned());
        }
        return Ok(root.strip_prefix(prefix).unwrap_or(root).to_owned());
    }
    Err(ParseError::NoCgroupV2Entry)
}

#[cfg(test)]
mod tests;
