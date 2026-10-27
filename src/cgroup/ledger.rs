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

//! The record, per cgroup v1 directory, of the device permission bits this daemon itself added
//! to `devices.allow`. `devices.list` cannot tell a runtime exception from one of ours, so
//! without the ledger a later narrowing could neither find what to revoke nor avoid revoking the
//! runtime's own baseline.

use std::collections::{BTreeSet, HashMap};
use std::sync::{Mutex, MutexGuard, PoisonError};

use super::handle::Identity;
use super::v1set::DevBit;

/// The cgroup v1 grants of one processor. It lives as long as the processor that owns it and is
/// shared by every cgroup v1 controller it makes. Entries are keyed by [`Identity`], so a cgroup
/// recreated at the same path starts with nothing owned. The default value is ready to use.
#[derive(Debug, Default)]
pub struct Ledger {
    entries: Mutex<HashMap<Identity, BTreeSet<DevBit>>>,
}

impl Ledger {
    /// Drops the record for `id`. Only call it once the grants it lists are known to be gone
    /// (revoked, or the cgroup no longer exists).
    pub fn forget(&self, id: &Identity) {
        self.lock().remove(id);
    }

    /// A copy of the bits recorded for `id`.
    pub(super) fn owned(&self, id: &Identity) -> BTreeSet<DevBit> {
        self.lock().get(id).cloned().unwrap_or_default()
    }

    pub(super) fn add(&self, id: &Identity, bit: DevBit) {
        self.lock().entry(id.clone()).or_default().insert(bit);
    }

    pub(super) fn remove(&self, id: &Identity, bit: DevBit) {
        let mut entries = self.lock();
        if let Some(bits) = entries.get_mut(id) {
            bits.remove(&bit);
            if bits.is_empty() {
                entries.remove(id);
            }
        }
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<Identity, BTreeSet<DevBit>>> {
        // Every change is one insert or remove, so the map is consistent even after a panic
        // (which aborts outside tests).
        self.entries.lock().unwrap_or_else(PoisonError::into_inner)
    }
}
