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

//! The per-container history of runs and the cgroups they were verified in. Go's `lifecycle.go`.

use std::collections::BTreeMap;
use std::fmt;
use std::sync::{Arc, Mutex, PoisonError};

use jiff::Timestamp;

use crate::cgroup::{CgroupVersion, Identity};
use crate::clock::Clock;
use crate::go_compat::time_layout::{go_zero_time, parse_rfc3339_nano};

/// How much is known about one run of a container.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LifecycleState {
    /// An inspect failed; only the ID and the time of the observation are known.
    Provisional,
    /// The start time and the cgroup identity were verified against the pinned process.
    Verified,
    /// The run ended (die, destroy, or seen not running).
    Terminal,
}

impl fmt::Display for LifecycleState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Provisional => "provisional",
            Self::Verified => "verified",
            Self::Terminal => "terminal",
        })
    }
}

/// Bounds one container's history; the oldest records without an identity go first.
const MAX_RECORDS_PER_CONTAINER: usize = 8;

/// The cgroup a run was verified in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunCgroup {
    pub identity: Identity,
    pub version: CgroupVersion,
    pub privileged: bool,
}

/// One run of a container.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LifecycleRecord {
    pub container_id: String,
    /// Docker's `State.StartedAt`, empty while unknown.
    pub started_at: String,
    /// The cgroup the run was verified in; `None` while unknown (Go's zero `Identity`).
    pub cgroup: Option<RunCgroup>,
    pub state: LifecycleState,
    pub first_seen: Timestamp,
    /// The last time an inspect failure refreshed the record.
    pub observed: Timestamp,
    /// Set once the empty set was applied at the cgroup after the run ended; the record is kept
    /// until the cgroup is verified gone.
    pub revoked: bool,
}

impl LifecycleRecord {
    /// Whether the record names a cgroup.
    #[must_use]
    pub const fn has_identity(&self) -> bool {
        self.cgroup.is_some()
    }

    /// The cgroup's identity, if the record names one.
    #[must_use]
    pub fn identity(&self) -> Option<&Identity> {
        self.cgroup.as_ref().map(|cgroup| &cgroup.identity)
    }

    /// `StartedAt` parsed; `None` when it is unknown or not RFC 3339.
    fn started(&self) -> Option<Timestamp> {
        if self.started_at.is_empty() {
            return None;
        }
        parse_rfc3339_nano(&self.started_at)
    }
}

/// The per-container history of runs. A restart under the same ID adds a record rather than
/// replacing one, so cleanup that arrives late for an old run never reaches the new one. Records
/// are released only once their cgroup is verified gone; an inspect that cannot find the
/// container never removes one.
pub struct Lifecycles {
    by_id: Mutex<BTreeMap<String, Vec<LifecycleRecord>>>,
    clock: Arc<dyn Clock>,
}

impl fmt::Debug for Lifecycles {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Lifecycles")
            .field("by_id", &self.by_id)
            .finish_non_exhaustive()
    }
}

impl Lifecycles {
    /// An empty history whose times come from `clock`.
    #[must_use]
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            by_id: Mutex::new(BTreeMap::new()),
            clock,
        }
    }

    /// Copies of the container's records, oldest first.
    #[must_use]
    pub fn records(&self, container_id: &str) -> Vec<LifecycleRecord> {
        self.locked().get(container_id).cloned().unwrap_or_default()
    }

    /// The current time of the history's clock.
    pub(super) fn now(&self) -> Timestamp {
        self.clock.now()
    }

    /// Copies of every record.
    pub(super) fn all(&self) -> Vec<LifecycleRecord> {
        self.locked().values().flatten().cloned().collect()
    }

    /// Creates or refreshes the provisional record of a container an inspect could not report
    /// on.
    pub(super) fn provisional(&self, container_id: &str) {
        let now = self.now();
        let mut by_id = self.locked();
        if let Some(record) = by_id.get_mut(container_id).and_then(|records| {
            records
                .iter_mut()
                .find(|record| record.state == LifecycleState::Provisional)
        }) {
            record.observed = now;
            return;
        }
        append(
            &mut by_id,
            LifecycleRecord {
                container_id: container_id.to_owned(),
                started_at: String::new(),
                cgroup: None,
                state: LifecycleState::Provisional,
                first_seen: now,
                observed: now,
                revoked: false,
            },
        );
    }

    /// Records the cgroup a run was verified in. It upgrades the record of the same run, else a
    /// provisional record, else adds a record.
    pub(super) fn verify(&self, container_id: &str, started_at: &str, cgroup: RunCgroup) {
        let now = self.now();
        let mut by_id = self.locked();
        let records = by_id.get_mut(container_id);
        let found = records.and_then(|records| {
            records
                .iter()
                .position(|record| record.started_at == started_at)
                .or_else(|| {
                    records
                        .iter()
                        .position(|record| record.state == LifecycleState::Provisional)
                })
                .map(|index| &mut records[index])
        });
        let Some(record) = found else {
            append(
                &mut by_id,
                LifecycleRecord {
                    container_id: container_id.to_owned(),
                    started_at: started_at.to_owned(),
                    cgroup: Some(cgroup),
                    state: LifecycleState::Verified,
                    first_seen: now,
                    observed: go_zero_time(),
                    revoked: false,
                },
            );
            return;
        };
        started_at.clone_into(&mut record.started_at);
        record.cgroup = Some(cgroup);
        record.revoked = false;
        if record.state != LifecycleState::Terminal {
            record.state = LifecycleState::Verified;
        }
    }

    /// The record of one run.
    pub(super) fn lookup(&self, container_id: &str, started_at: &str) -> Option<LifecycleRecord> {
        self.locked()
            .get(container_id)?
            .iter()
            .find(|record| record.started_at == started_at)
            .cloned()
    }

    /// Resolves an event that ended a run at `event_time` (die, destroy). It marks terminal the
    /// verified record with the latest start time not after `event_time` and returns it; with no
    /// such record, every provisional record becomes terminal and is returned instead.
    pub(super) fn terminate(
        &self,
        container_id: &str,
        event_time: Timestamp,
    ) -> Vec<LifecycleRecord> {
        let mut by_id = self.locked();
        let Some(records) = by_id.get_mut(container_id) else {
            return Vec::new();
        };
        let mut matched: Option<(usize, Timestamp)> = None;
        for (index, record) in records.iter().enumerate() {
            if record.state == LifecycleState::Provisional {
                continue;
            }
            let Some(started) = record.started() else {
                continue;
            };
            if !record.has_identity() || started > event_time {
                continue;
            }
            if matched.is_none_or(|(_, start)| started > start) {
                matched = Some((index, started));
            }
        }
        if let Some((index, _)) = matched {
            records[index].state = LifecycleState::Terminal;
            return vec![records[index].clone()];
        }
        records
            .iter_mut()
            .filter(|record| record.state == LifecycleState::Provisional)
            .map(|record| {
                record.state = LifecycleState::Terminal;
                record.clone()
            })
            .collect()
    }

    /// Marks one run ended.
    pub(super) fn mark_terminal(&self, container_id: &str, started_at: &str) {
        self.update(container_id, started_at, |record| {
            record.state = LifecycleState::Terminal;
        });
    }

    /// Records that the empty set was applied for an ended run.
    pub(super) fn mark_revoked(&self, container_id: &str, started_at: &str) {
        self.update(container_id, started_at, |record| {
            if record.state == LifecycleState::Terminal {
                record.revoked = true;
            }
        });
    }

    fn update(&self, container_id: &str, started_at: &str, change: impl Fn(&mut LifecycleRecord)) {
        if let Some(records) = self.locked().get_mut(container_id) {
            records
                .iter_mut()
                .filter(|record| record.started_at == started_at)
                .for_each(change);
        }
    }

    /// Removes the record `released` is a copy of. Records are told apart by start time and
    /// creation time: records that never learned their start time all share an empty
    /// `started_at`.
    pub(super) fn release(&self, released: &LifecycleRecord) {
        let mut by_id = self.locked();
        let Some(records) = by_id.get_mut(&released.container_id) else {
            return;
        };
        records.retain(|record| {
            record.started_at != released.started_at || record.first_seen != released.first_seen
        });
        if records.is_empty() {
            by_id.remove(&released.container_id);
        }
    }

    fn locked(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Vec<LifecycleRecord>>> {
        // Under panic = "abort" a poisoned lock is only reachable in tests; every change leaves
        // the history consistent.
        self.by_id.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// Appends `record` to its container's history, then, over the bound, drops the oldest records
/// that hold no identity: they name no cgroup, so nothing can be left to revoke through them.
fn append(by_id: &mut BTreeMap<String, Vec<LifecycleRecord>>, record: LifecycleRecord) {
    let records = by_id.entry(record.container_id.clone()).or_default();
    records.push(record);
    while records.len() > MAX_RECORDS_PER_CONTAINER {
        let Some(index) = records.iter().position(|record| !record.has_identity()) else {
            break;
        };
        records.remove(index);
    }
}
