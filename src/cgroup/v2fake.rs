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

//! A recording fake of the cgroup v2 kernel operations, for the controller's tests: one cgroup's
//! `BPF_CGROUP_DEVICE` attachment list, every call recorded as the Go fake recorded it, and a
//! check that every handle it opened was closed.
//!
//! Go: `fakeOps`, `fakeProg` and the helpers of `v2swap_test.go`, `converge_test.go` and
//! `owned_test.go`. Go's fake returned an arbitrary error for an injected failure; this one
//! returns the error the kernel operation would, with the injected errno.

use std::collections::HashMap;
use std::os::fd::BorrowedFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use rustix::io::Errno;

use super::api::DeviceRule;
use super::asm::Instruction;
use super::error::Error;
use super::handle::Identity;
use super::owned::{ProgMeta, from_canonical};
use super::testing::{canonical, rule, runtime_original};
use super::v2::CgroupV2;
use super::v2ops::{Attached, F_ALLOW_MULTI, LoadError, OpsError, ReplaceProbe, V2Ops};
use super::v2plan::FilterCache;
use crate::logger::{Level, MemorySink};
use crate::test_support::capture_logs;

/// Go's `errInjected`.
pub(super) const INJECTED: Errno = Errno::IO;
/// Go's `errRollback`.
pub(super) const ROLLBACK: Errno = Errno::BUSY;
/// Go's `errLoadFails`.
pub(super) const LOAD_FAILS: Errno = Errno::NOMEM;

/// The cgroup the fake stands for. Go: `testIdentity`.
pub(super) fn test_identity() -> Identity {
    Identity {
        path: "/sys/fs/cgroup/test".to_owned(),
        inode: 4242,
    }
}

/// What the replace probe knows when a pass starts. Go: `replaceUnknown`, `replaceSupported`
/// and `replaceUnsupported`.
#[derive(Debug, Clone, Copy)]
pub(super) enum Replace {
    Unknown,
    Supported,
    Unsupported,
}

/// A probe in `state`, for one test. Go: `probeIn`.
pub(super) fn probe_in(state: Replace) -> &'static ReplaceProbe {
    Box::leak(Box::new(match state {
        Replace::Unknown => ReplaceProbe::new(),
        Replace::Supported => ReplaceProbe::known(true),
        Replace::Unsupported => ReplaceProbe::known(false),
    }))
}

/// The rules the swap tests grant. Go: `testRules` and `grantRules`.
pub(super) fn test_rules() -> Vec<DeviceRule> {
    vec![rule("c", 10, 200, "rwm")]
}

/// The controller on `ops`, and the logs it writes.
pub(super) fn controller<'fake>(
    ops: &'fake FakeOps,
    replace: &'static ReplaceProbe,
    cache: Arc<FilterCache>,
) -> (CgroupV2<&'fake FakeOps>, MemorySink) {
    let (log, sink) = capture_logs(Level::DEBUG);
    (
        CgroupV2 {
            ops,
            replace,
            cache,
            log,
        },
        sink,
    )
}

/// One pass with `rules`, `replace` and `cache`; no cache is Go's nil one, which keeps nothing
/// between passes. Go: `run` and `setRules`.
pub(super) fn set_rules(
    ops: &FakeOps,
    replace: Replace,
    cache: Option<&Arc<FilterCache>>,
    rules: &[DeviceRule],
) -> Result<(), Error> {
    let cache = cache.map_or_else(|| Arc::new(FilterCache::new()), Arc::clone);
    let (controller, _) = controller(ops, probe_in(replace), cache);
    controller.set_rules(rustix::fs::CWD, &test_identity(), rules)
}

/// [`set_rules`] with the recorded calls cleared first, so each pass is asserted alone. Go:
/// `pass`.
pub(super) fn pass(
    ops: &FakeOps,
    replace: Replace,
    cache: Option<&Arc<FilterCache>>,
    rules: &[DeviceRule],
) -> Result<(), Error> {
    ops.state().calls.clear();
    set_rules(ops, replace, cache, rules)
}

/// A handle naming the program it refers to. Several handles may refer to the same program
/// (every query opens fresh ones).
#[derive(Debug)]
pub(super) struct FakeProg {
    name: String,
    closed: Arc<AtomicBool>,
}

impl Drop for FakeProg {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::SeqCst);
    }
}

/// What the fake models, and what it recorded.
#[derive(Debug)]
pub(super) struct FakeState {
    pub(super) attached: Vec<String>,
    pub(super) flags: u32,
    pub(super) inaccessible: usize,
    /// The errno a call ("attach N1", "replace O1->N1", "detach O1", "load N2") fails with; a
    /// failing call changes nothing.
    pub(super) errs: HashMap<String, Errno>,
    pub(super) calls: Vec<String>,
    pub(super) loads: usize,
    /// Each program's instructions; a name without an entry is a runtime original
    /// ([`runtime_original`]).
    pub(super) insts: HashMap<String, Vec<Instruction>>,
    /// The name each loaded program was given.
    pub(super) names: HashMap<String, String>,
    pub(super) meta: ProgMeta,
    opened: Vec<(String, Arc<AtomicBool>)>,
}

/// The fake. The controller takes it by reference, so a test keeps it across passes.
#[derive(Debug)]
pub(super) struct FakeOps(Mutex<FakeState>);

impl FakeOps {
    /// A cgroup with `attached` programs, attached with `BPF_F_ALLOW_MULTI`. Go: `newFakeOps`.
    pub(super) fn new(attached: &[&str]) -> Self {
        Self(Mutex::new(FakeState {
            attached: attached.iter().map(|&name| name.to_owned()).collect(),
            flags: F_ALLOW_MULTI,
            inaccessible: 0,
            errs: HashMap::new(),
            calls: Vec::new(),
            loads: 0,
            insts: HashMap::new(),
            names: HashMap::new(),
            meta: ProgMeta {
                map_ids_known: true,
                map_count: 0,
            },
            opened: Vec::new(),
        }))
    }

    /// A cgroup whose attached programs carry the given code, in name order. Go: `ownedOps`.
    pub(super) fn owned(programs: &[(&str, &[u8])]) -> Self {
        let mut names: Vec<&str> = programs.iter().map(|&(name, _)| name).collect();
        names.sort_unstable();
        let ops = Self::new(&names);
        for &(name, raw) in programs {
            ops.state()
                .insts
                .insert(name.to_owned(), from_canonical(raw).unwrap());
        }
        ops
    }

    pub(super) fn state(&self) -> MutexGuard<'_, FakeState> {
        self.0.lock().unwrap()
    }

    /// Makes `call` fail with `errno`.
    pub(super) fn fail(&self, call: &str, errno: Errno) {
        self.state().errs.insert(call.to_owned(), errno);
    }

    /// The canonical bytes of the program `name`. Go: `attachedRaw`.
    pub(super) fn raw(&self, name: &str) -> Vec<u8> {
        canonical(&self.instructions_of(name))
    }

    /// The names of the attached programs.
    pub(super) fn attached(&self) -> Vec<String> {
        self.state().attached.clone()
    }

    /// Go: `assertCalls`.
    #[track_caller]
    pub(super) fn assert_calls(&self, want: &[&str]) {
        assert_eq!(self.state().calls, want, "calls");
    }

    /// Go: `assertAttached`.
    #[track_caller]
    pub(super) fn assert_attached(&self, want: &[&str]) {
        assert_eq!(self.state().attached, want, "attached");
    }

    /// Go: `assertAllClosed`.
    #[track_caller]
    pub(super) fn assert_all_closed(&self) {
        for (name, closed) in &self.state().opened {
            assert!(closed.load(Ordering::SeqCst), "handle {name} left open");
        }
    }

    fn instructions_of(&self, name: &str) -> Vec<Instruction> {
        self.state()
            .insts
            .get(name)
            .cloned()
            .unwrap_or_else(runtime_original)
    }

    fn open(state: &mut FakeState, name: &str) -> FakeProg {
        let closed = Arc::new(AtomicBool::new(false));
        state.opened.push((name.to_owned(), Arc::clone(&closed)));
        FakeProg {
            name: name.to_owned(),
            closed,
        }
    }

    /// Records `call` and returns the errno it is to fail with.
    fn record(state: &mut FakeState, call: String) -> Option<Errno> {
        let errno = state.errs.get(&call).copied();
        state.calls.push(call);
        errno
    }
}

impl V2Ops for &FakeOps {
    type Prog = FakeProg;

    fn query(&self, _dir: BorrowedFd<'_>) -> Result<Attached<FakeProg>, OpsError> {
        let mut state = self.state();
        let names = state.attached.clone();
        let progs = names
            .iter()
            .map(|name| FakeOps::open(&mut state, name))
            .collect();
        Ok(Attached {
            progs,
            total: names.len() + state.inaccessible,
            inaccessible: state.inaccessible,
            attach_flags: state.flags,
        })
    }

    fn load(&self, name: &str, instructions: &[Instruction]) -> Result<FakeProg, OpsError> {
        let mut state = self.state();
        state.loads += 1;
        let prog = format!("N{}", state.loads);
        if let Some(errno) = FakeOps::record(&mut state, format!("load {prog}")) {
            return Err(OpsError::Load(LoadError::Verifier {
                errno,
                cause: None,
                log: Vec::new(),
            }));
        }
        state.insts.insert(prog.clone(), instructions.to_vec());
        state.names.insert(prog.clone(), name.to_owned());
        Ok(FakeOps::open(&mut state, &prog))
    }

    fn attach(
        &self,
        prog: &FakeProg,
        _dir: BorrowedFd<'_>,
        flags: u32,
        replace: Option<&FakeProg>,
    ) -> Result<(), OpsError> {
        assert_eq!(
            flags, F_ALLOW_MULTI,
            "attach flags {flags:#x}, want BPF_F_ALLOW_MULTI only"
        );
        let mut state = self.state();
        let call = match replace {
            Some(old) => format!("replace {}->{}", old.name, prog.name),
            None => format!("attach {}", prog.name),
        };
        if let Some(errno) = FakeOps::record(&mut state, call) {
            return Err(OpsError::Attach { flags, errno });
        }
        let Some(old) = replace else {
            state.attached.push(prog.name.clone());
            return Ok(());
        };
        let Some(index) = state.attached.iter().position(|name| *name == old.name) else {
            return Err(OpsError::Attach {
                flags,
                errno: Errno::NOENT,
            });
        };
        state.attached[index].clone_from(&prog.name);
        Ok(())
    }

    fn detach(&self, prog: &FakeProg, _dir: BorrowedFd<'_>) -> Result<(), OpsError> {
        let mut state = self.state();
        if let Some(errno) = FakeOps::record(&mut state, format!("detach {}", prog.name)) {
            return Err(OpsError::Detach(errno));
        }
        state.attached.retain(|name| *name != prog.name);
        Ok(())
    }

    fn instructions(&self, prog: &FakeProg) -> Result<(Vec<Instruction>, ProgMeta), OpsError> {
        let meta = self.state().meta;
        Ok((self.instructions_of(&prog.name), meta))
    }
}
