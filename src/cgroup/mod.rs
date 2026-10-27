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

//! The device cgroup of a container: where it lives (parsed from the process's `/proc` files), a
//! handle on its directory that every later read and write goes through, and the controllers
//! that make the daemon-owned device grants of that cgroup equal a desired set: cgroup v1's
//! `devices.allow` and `devices.deny`, with the [`Ledger`] of what the daemon itself added, and
//! for cgroup v2 the device filter program: its instructions (`asm.rs`), the rule blocks
//! (`ebpf.rs`), and the daemon's own wrapper around a runtime's program with the gate that
//! decides which programs may be wrapped (`owned.rs`), the kernel operations (`v2ops.rs`), and the
//! controller (`v2.rs`) whose pass is planned in full (`v2plan.rs`) before anything changes.
//!
//! `api.rs`, `v1.rs` and `v2.rs` port NVIDIA's files and keep NVIDIA's header. The Go
//! `Interface` methods that read `/proc/<pid>` by path (`GetDeviceCGroupMountPath`,
//! `GetDeviceCGroupRootPath`) and `GetDeviceCGroupVersion` are not ported: the daemon only parses
//! the contents it read through a pinned `/proc/<pid>` descriptor ([`parse_proc_cgroup`]).
//!
//! The `/proc` files and `devices.list` are the kernel's ASCII output. A byte that is not UTF-8
//! is replaced before parsing, where Go keeps it; that can only change a path that then opens
//! nothing, or the text of an error about a malformed line.

mod api;
mod asm;
mod bpf_sys;
#[cfg(test)]
mod bpfinterp;
mod ebpf;
mod error;
mod handle;
mod ledger;
mod owned;
#[cfg(test)]
mod testing;
mod v1;
mod v1set;
mod v2;
#[cfg(test)]
mod v2fake;
mod v2ops;
mod v2plan;

pub use api::{
    CgroupVersion, DeviceRule, Interface, ParseError, ProcCgroup, new, parse_proc_cgroup,
};
pub use asm::{DecodeCause, DecodeError, EncodeCause, Instruction, OpCode};
pub use ebpf::{Program, RuleError, prepend_device_filter};
pub use error::{Error, FileError, ListEntryCause};
pub use handle::{CgroupHandle, HandleError, Identity};
pub use ledger::Ledger;
pub use owned::{
    CanonicalError, Conflict, EmitError, GateError, NonceError, OWNED_PROGRAM_NAME, OwnedError,
    OwnedProgram, ProgMeta, RuleBlock, canonical_bytes, check_wrappable, emit_owned,
    from_canonical, new_nonce, parse_owned,
};
pub use v1::CgroupV1;
pub use v2::CgroupV2;
pub use v2ops::{
    Attached, F_ALLOW_MULTI, F_ALLOW_OVERRIDE, FetchError, InfoError, InstructionsError, KernelOps,
    KernelProg, LoadError, OpsError, PROCESS_REPLACE_PROBE, ReplaceProbe, V2Ops,
    describe_attach_flags, kernel_at_least,
};
pub use v2plan::FilterCache;
