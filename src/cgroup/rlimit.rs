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

//! cilium/ebpf v0.22.0's `rlimit.RemoveMemlock`, which the Go daemon called once at startup so
//! that loading a device filter does not fail on a kernel that still charges BPF memory to
//! `RLIMIT_MEMLOCK` (before Linux 5.11, where memory cgroup accounting replaced it).
//!
//! Like cilium, it first probes the kernel: with the soft limit lowered to 0, it creates a
//! one-entry array map and restores the limit. A map created anyway means memcg accounting, and
//! the limit is left as it is; `EPERM` means the limit applies, and it is raised to infinity;
//! any other answer is an error. cilium probes once at process start; here the probe runs when
//! the daemon calls [`remove_memlock`], which only the daemon does, so the launcher, which loads
//! no program, never touches the limit.

use std::fmt;

use rustix::io::Errno;
use rustix::process::{Resource, Rlimit, getrlimit, prlimit};

use super::bpf_sys;
use crate::go_compat::errno::errno_string;

/// Why the limit could not be removed, with cilium's text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemlockError {
    /// The probe could not lower the soft limit.
    Lower(Errno),
    /// The probe could not restore the limit it lowered.
    Restore(Errno),
    /// The probe's map creation failed other than with `EPERM`.
    Detect(Errno),
    /// The limit could not be raised.
    Set(Errno),
}

impl fmt::Display for MemlockError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (prefix, errno) = match self {
            Self::Lower(errno) => ("lowering memlock rlimit", errno),
            Self::Restore(errno) => ("restoring old memlock rlimit", errno),
            Self::Detect(errno) => ("unexpected error detecting memory cgroup accounting", errno),
            Self::Set(errno) => ("failed to set memlock rlimit", errno),
        };
        write!(
            formatter,
            "{prefix}: {}",
            errno_string(errno.raw_os_error())
        )
    }
}

impl std::error::Error for MemlockError {}

/// cilium's `detectMemcgAccounting`: whether the kernel charges BPF memory to the memory cgroup
/// rather than to `RLIMIT_MEMLOCK`.
fn memcg_accounting() -> Result<bool, MemlockError> {
    let old = getrlimit(Resource::Memlock);
    let zero = Rlimit {
        current: Some(0),
        maximum: old.maximum,
    };
    prlimit(None, Resource::Memlock, zero).map_err(MemlockError::Lower)?;
    let probe = bpf_sys::map_create_probe();
    prlimit(None, Resource::Memlock, old).map_err(MemlockError::Restore)?;
    match probe {
        Ok(_map) => Ok(true),
        Err(Errno::PERM) => Ok(false),
        Err(errno) => Err(MemlockError::Detect(errno)),
    }
}

/// cilium's `RemoveMemlock`: lifts `RLIMIT_MEMLOCK` to infinity for the process, unless the
/// kernel accounts BPF memory to the memory cgroup. The limit is not inherited by containers.
///
/// # Errors
///
/// [`MemlockError`] when the probe fails or the limit cannot be raised.
pub fn remove_memlock() -> Result<(), MemlockError> {
    if memcg_accounting()? {
        return Ok(());
    }
    let unlimited = Rlimit {
        current: None,
        maximum: None,
    };
    prlimit(None, Resource::Memlock, unlimited)
        .map(drop)
        .map_err(MemlockError::Set)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// cilium's texts, the errno in Go's words.
    #[test]
    fn errors_read_as_cilium_s() {
        assert_eq!(
            MemlockError::Set(Errno::PERM).to_string(),
            "failed to set memlock rlimit: operation not permitted"
        );
        assert_eq!(
            MemlockError::Detect(Errno::INVAL).to_string(),
            "unexpected error detecting memory cgroup accounting: invalid argument"
        );
        assert_eq!(
            MemlockError::Lower(Errno::PERM).to_string(),
            "lowering memlock rlimit: operation not permitted"
        );
        assert_eq!(
            MemlockError::Restore(Errno::PERM).to_string(),
            "restoring old memlock rlimit: operation not permitted"
        );
    }

    /// Go: `TestRemoveMemlock` (cilium). The limit is unchanged where the kernel accounts BPF
    /// memory to the memory cgroup, and infinite where it does not; without the privilege to
    /// raise it, the error is cilium's and the limit is unchanged.
    #[test]
    fn remove_memlock_leaves_or_lifts_the_limit() {
        let before = getrlimit(Resource::Memlock);
        let memcg = memcg_accounting();
        assert_eq!(
            getrlimit(Resource::Memlock),
            before,
            "the probe restores the limit"
        );
        let removed = remove_memlock();
        let after = getrlimit(Resource::Memlock);
        match (memcg, removed) {
            (Ok(true), Ok(())) | (Ok(false), Err(MemlockError::Set(_))) => {
                assert_eq!(after, before);
            }
            (Ok(false), Ok(())) => assert_eq!(after.current, None),
            (memcg, removed) => panic!("probe {memcg:?}, removal {removed:?}"),
        }
    }
}
