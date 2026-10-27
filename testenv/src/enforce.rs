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

//! What the real-enforcement tests decide without Docker, so that a plain `cargo test` checks it:
//! the guard variables and the fail-instead-of-skip rule of the Go suite (`enforceRequested`,
//! `requireOrSkip`, `reloadUnsupported` in `test/integration`), the flag spelling of each daemon
//! build, and the reading of the device programs attached to a container's cgroup.
//!
//! The programs are read by [`dump_device_programs`], which needs root (`BPF_PROG_QUERY` and
//! `BPF_PROG_GET_FD_BY_ID`): the suite runs its own binary through `sudo -n` with [`DUMP_FLAG`],
//! the same non-interactive sudo the reload test runs `systemctl` with, and parses the text it
//! prints ([`ProgramDump`]). Everything after that runs unprivileged and uses the daemon's own
//! classification ([`parse_owned_innermost`], which the cgroup v2 pass classifies with), never a
//! second reading of the wrapper format.

use std::fmt::Write as _;
use std::hash::{DefaultHasher, Hash as _, Hasher as _};
use std::os::fd::AsFd as _;
use std::os::unix::fs::MetadataExt as _;
use std::sync::Arc;

use swarm_device_access::cgroup::{
    CgroupVersion, DeviceRule, KernelOps, V2Ops as _, canonical_bytes, parse_owned,
    parse_owned_innermost,
};
use swarm_device_access::clock::SystemClock;
use swarm_device_access::go_compat::local_zone::local_zone;
use swarm_device_access::logger::{Config, Format, Level, LogHandle, Logger};
use swarm_device_access::processor::pin::{host_cgroup_path, read_proc_cgroup};

use crate::Error;

/// The variable that opts into the tests that change the host, and turns every missing
/// prerequisite into a failure: the Go suite's `envEnforce`.
pub const ENV_ENFORCE: &str = "SDA_IT_ENFORCE";
/// The variable that makes the reload subtest fail, instead of skip, on a host where `systemctl
/// daemon-reload` does not wipe the device programs: the Go suite's `envRequireReload`.
pub const ENV_REQUIRE_RELOAD: &str = "SDA_IT_REQUIRE_RELOAD";
/// The Go daemon image of the cross-language upgrade test and of the identity check's negative
/// control: the last published 1.x release, pinned by digest (`make test-integration` sets it and
/// pulls it). Never the image under test.
pub const ENV_GO_TEST_IMAGE: &str = "SDA_GO_TEST_IMAGE";
/// The argument that makes the suite's binary print [`dump_device_programs`] for a pid and exit,
/// instead of running tests.
pub const DUMP_FLAG: &str = "--dump-device-programs";

/// Whether a guard variable is on: exactly `1`, as the Go suite reads it.
#[must_use]
pub fn flag_set(value: Option<&str>) -> bool {
    value == Some("1")
}

/// The Go image the upgrade test and the identity control run: [`ENV_GO_TEST_IMAGE`]'s value, or
/// `None` when it is unset or empty. There is no default: the tag the integration image is built
/// under names the Rust image now, so a default could only ever name the wrong daemon.
#[must_use]
pub fn go_test_image(value: Option<&str>) -> Option<String> {
    value.filter(|image| !image.is_empty()).map(str::to_owned)
}

/// How a test stops when the host cannot run it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The test fails with this message.
    Fail(String),
    /// The test skips itself with this message.
    Skip(String),
}

/// Go's `requireOrSkip` for a missing prerequisite (`cause`): a failure when `strict` (the run
/// opted into enforcement, which must not go green by skipping it), a skip otherwise.
#[must_use]
pub fn missing_prerequisite(strict: bool, cause: &str) -> Verdict {
    if strict {
        Verdict::Fail(format!(
            "missing prerequisite ({ENV_ENFORCE}=1 makes this a failure): {cause}"
        ))
    } else {
        Verdict::Skip(format!("missing prerequisite: {cause}"))
    }
}

/// Go's `reloadUnsupported`: the reload scenario cannot prove anything on this host (`cause`). A
/// failure when `required`, so that CI cannot pass without reload coverage, a skip otherwise.
#[must_use]
pub fn reload_unsupported(required: bool, cause: &str) -> Verdict {
    if required {
        Verdict::Fail(format!(
            "reload scenario required by {ENV_REQUIRE_RELOAD}=1: {cause}"
        ))
    } else {
        Verdict::Skip(format!(
            "reload scenario not supported on this host (set {ENV_REQUIRE_RELOAD}=1 to fail \
             instead): {cause}"
        ))
    }
}

/// Which daemon a container runs: the two spell their long flags differently (Q1 in
/// `docs/rust-port.md`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Build {
    /// The Go daemon, whose flag package takes `-name=value`.
    Go,
    /// The Rust daemon, which refuses a single-dash long flag and takes `--name=value`.
    Rust,
}

impl Build {
    /// The daemon arguments of the Go suite's `startDaemonContainer` (opt-in policy, JSON debug
    /// logging), then `flags`, each given as `name` or `name=value` and spelled for this build.
    #[must_use]
    pub fn daemon_args(self, flags: &[&str]) -> Vec<String> {
        let dashes = match self {
            Self::Go => "-",
            Self::Rust => "--",
        };
        ["policy-mode=opt-in", "log-format=json", "log-level=debug"]
            .iter()
            .chain(flags)
            .map(|flag| format!("{dashes}{flag}"))
            .collect()
    }
}

impl std::fmt::Display for Build {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::Go => "Go",
            Self::Rust => "Rust",
        })
    }
}

/// The device programs attached to one cgroup, as [`dump_device_programs`] read them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProgramDump {
    /// The cgroup directory on the host.
    pub cgroup: String,
    /// Its inode: a container restarted under the same path has a new one.
    pub inode: u64,
    /// How many programs the kernel reports attached.
    pub total: usize,
    /// How many of them could not be opened.
    pub inaccessible: usize,
    /// The cgroup's attach flags for `BPF_CGROUP_DEVICE`.
    pub attach_flags: u32,
    /// The canonical bytes of each program that could be opened, in the kernel's order.
    pub programs: Vec<Vec<u8>>,
}

impl ProgramDump {
    /// The text [`DUMP_FLAG`] prints: one `key value` line per field, then one `program <hex>`
    /// line per program.
    #[must_use]
    pub fn render(&self) -> String {
        let mut text = format!(
            "cgroup {}\ninode {}\ntotal {}\ninaccessible {}\nattach_flags {}\n",
            self.cgroup, self.inode, self.total, self.inaccessible, self.attach_flags
        );
        for program in &self.programs {
            let _ = writeln!(text, "program {}", hex(program));
        }
        text
    }

    /// Reads what [`ProgramDump::render`] wrote.
    ///
    /// # Errors
    ///
    /// A line that is not one of the rendered ones, a field missing or given twice, or a value
    /// that does not parse.
    pub fn parse(text: &str) -> Result<Self, Error> {
        let mut cgroup = None;
        let mut numbers: [Option<u64>; 4] = [None; 4];
        let mut programs = Vec::new();
        for line in text.lines() {
            let (key, value) = line
                .split_once(' ')
                .ok_or_else(|| Error::new(format!("malformed dump line {line:?}")))?;
            let slot = match key {
                "cgroup" => {
                    if cgroup.replace(value.to_owned()).is_some() {
                        return Err(Error::new("the dump names its cgroup twice"));
                    }
                    continue;
                }
                "program" => {
                    programs.push(unhex(value)?);
                    continue;
                }
                "inode" => 0,
                "total" => 1,
                "inaccessible" => 2,
                "attach_flags" => 3,
                _ => return Err(Error::new(format!("unknown dump line {line:?}"))),
            };
            let number = value
                .parse()
                .map_err(|error| Error::new(format!("dump line {line:?}: {error}")))?;
            if numbers[slot].replace(number).is_some() {
                return Err(Error::new(format!("the dump gives {key} twice")));
            }
        }
        let missing = |name: &str| Error::new(format!("the dump has no {name}"));
        let [inode, total, inaccessible, attach_flags] = numbers;
        Ok(Self {
            cgroup: cgroup.ok_or_else(|| missing("cgroup"))?,
            inode: inode.ok_or_else(|| missing("inode"))?,
            total: to_usize(total.ok_or_else(|| missing("total"))?)?,
            inaccessible: to_usize(inaccessible.ok_or_else(|| missing("inaccessible"))?)?,
            attach_flags: u32::try_from(attach_flags.ok_or_else(|| missing("attach_flags"))?)
                .map_err(|error| Error::new(format!("attach_flags: {error}")))?,
            programs,
        })
    }
}

/// Reads the device programs attached to the cgroup of the host process `pid`, as the daemon
/// finds that cgroup (`read_proc_cgroup` and `host_cgroup_path`, with the host's own root) and
/// reads its programs (`KernelOps`, and the canonical bytes its pass compares). Needs root.
///
/// # Errors
///
/// The process's cgroup cannot be read or is not on cgroup v2, its directory cannot be opened, or
/// a program cannot be queried, opened or dumped.
pub fn dump_device_programs(pid: i64) -> Result<ProgramDump, Error> {
    let resolved = read_proc_cgroup("/", pid)
        .map_err(|error| Error::new(format!("read the cgroup of pid {pid}: {error}")))?;
    match resolved.version {
        CgroupVersion::V2 => {}
        CgroupVersion::V1 => {
            return Err(Error::new(format!(
                "pid {pid} is on cgroup v1: there is no device program to read"
            )));
        }
    }
    let cgroup = host_cgroup_path(
        "/",
        &resolved.mount_point,
        &resolved.mount_prefix,
        &resolved.root,
    );
    let dir = std::fs::File::open(&cgroup)
        .map_err(|error| Error::new(format!("open {cgroup}: {error}")))?;
    let inode = dir
        .metadata()
        .map_err(|error| Error::new(format!("stat {cgroup}: {error}")))?
        .ino();
    let ops = KernelOps::new(stderr_log());
    let attached = ops
        .query(dir.as_fd())
        .map_err(|error| Error::new(format!("query {cgroup}: {error}")))?;
    let mut programs = Vec::with_capacity(attached.progs.len());
    for (index, prog) in attached.progs.iter().enumerate() {
        let (instructions, _) = ops
            .instructions(prog)
            .map_err(|error| Error::new(format!("program {index} of {cgroup}: {error}")))?;
        programs.push(
            canonical_bytes(&instructions)
                .map_err(|error| Error::new(format!("program {index} of {cgroup}: {error}")))?,
        );
    }
    Ok(ProgramDump {
        cgroup,
        inode,
        total: attached.total,
        inaccessible: attached.inaccessible,
        attach_flags: attached.attach_flags,
        programs,
    })
}

/// The daemon's text log at debug to stderr, for what `KernelOps` logs: stdout carries the dump.
fn stderr_log() -> LogHandle {
    LogHandle::new(Logger::new(
        Config {
            format: Format::Text,
            level: Level::DEBUG,
            include_time: true,
        },
        Box::new(std::io::stderr()),
        local_zone(|key| std::env::var(key).ok()),
        Arc::new(SystemClock),
    ))
}

/// One attached program, as the daemon's cgroup v2 pass classifies it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Classified {
    /// Not a validated wrapper: a runtime's program, or an original a restore put back.
    Bare,
    /// A validated wrapper of either daemon.
    Wrapper {
        /// Its lineage nonce.
        nonce: u64,
        /// The rules it grants.
        rules: Vec<DeviceRule>,
        /// The original underneath it, every nested wrapper stripped.
        original: Vec<u8>,
        /// Whether it wraps another wrapper, which a pass re-emits as one.
        nested: bool,
    },
}

/// Classifies the canonical bytes `raw` with [`parse_owned_innermost`].
///
/// # Errors
///
/// The program starts like a wrapper but does not validate: an owned-block conflict, which
/// neither daemon ever leaves behind.
pub fn classify(raw: &[u8]) -> Result<Classified, Error> {
    let conflict = |error: &dyn std::fmt::Display| Error::new(format!("owned block: {error}"));
    let Some(innermost) = parse_owned_innermost(raw).map_err(|error| conflict(&error))? else {
        return Ok(Classified::Bare);
    };
    let immediate = parse_owned(raw).map_err(|error| conflict(&error))?;
    Ok(Classified::Wrapper {
        nested: immediate.is_some_and(|outer| outer.original != innermost.original),
        nonce: innermost.nonce,
        rules: innermost.rules,
        original: innermost.original,
    })
}

/// The runtime originals: every program attached to the container's cgroup before any daemon ran.
///
/// # Errors
///
/// The dump is incomplete, holds no program (nothing for a daemon to wrap) or already holds a
/// wrapper (the cgroup is not the runtime's alone).
pub fn runtime_originals(baseline: &ProgramDump) -> Result<Vec<Vec<u8>>, Error> {
    let mut problems = accounting_problems(baseline);
    if baseline.programs.is_empty() {
        problems
            .push("no device program is attached: the runtime attached none to wrap".to_owned());
    }
    for (index, raw) in baseline.programs.iter().enumerate() {
        match classify(raw) {
            Ok(Classified::Bare) => {}
            Ok(Classified::Wrapper { nonce, .. }) => problems.push(format!(
                "program {index} is already an owned wrapper (nonce {nonce:#x}) before any \
                 daemon ran"
            )),
            Err(error) => problems.push(format!("program {index}: {error}")),
        }
    }
    if problems.is_empty() {
        Ok(baseline.programs.clone())
    } else {
        Err(Error::new(problems.join("; ")))
    }
}

/// What is wrong with `dump` after a grant: every program must be a wrapper, exactly one per
/// runtime original (`originals`), none nested, no two in one lineage, all with the same
/// non-empty rules. A wrapper over something else, a second wrapper of an original or an older
/// rule set is a stale wrapper; an original left bare was not granted.
#[must_use]
pub fn granted_problems(originals: &[Vec<u8>], dump: &ProgramDump) -> Vec<String> {
    let mut problems = accounting_problems(dump);
    let mut wrapped = Vec::new();
    let mut nonces = Vec::new();
    let mut rule_sets: Vec<Vec<DeviceRule>> = Vec::new();
    for (index, raw) in dump.programs.iter().enumerate() {
        match classify(raw) {
            Ok(Classified::Bare) => problems.push(format!(
                "program {index} ({}) is attached bare: every runtime original must be wrapped",
                fingerprint(raw)
            )),
            Ok(Classified::Wrapper {
                nonce,
                rules,
                original,
                nested,
            }) => {
                if nested {
                    problems.push(format!("program {index} wraps another wrapper"));
                }
                if rules.is_empty() {
                    problems.push(format!("program {index} is a wrapper that grants nothing"));
                }
                if nonces.contains(&nonce) {
                    problems.push(format!(
                        "program {index} is a second wrapper of lineage {nonce:#x}: a stale wrapper"
                    ));
                }
                nonces.push(nonce);
                if rule_sets.first().is_some_and(|first| *first != rules) {
                    problems.push(format!(
                        "program {index} grants {rules:?}, another wrapper {:?}: one is stale",
                        rule_sets.first()
                    ));
                }
                rule_sets.push(rules);
                wrapped.push(original);
            }
            Err(error) => problems.push(format!("program {index}: {error}")),
        }
    }
    if !same_multiset(&wrapped, originals) {
        problems.push(format!(
            "wrapped originals {}, want exactly one wrapper per runtime original {}",
            fingerprints(&wrapped),
            fingerprints(originals)
        ));
    }
    problems
}

/// What is wrong with `dump` after a revocation: no wrapper may remain, and the bare programs must
/// be the runtime originals (`originals`), each attached exactly once, as the restore plan
/// (`plan_restore`) leaves them.
#[must_use]
pub fn revoked_problems(originals: &[Vec<u8>], dump: &ProgramDump) -> Vec<String> {
    let mut problems = accounting_problems(dump);
    let mut bares = Vec::new();
    for (index, raw) in dump.programs.iter().enumerate() {
        match classify(raw) {
            Ok(Classified::Bare) => bares.push(raw.clone()),
            Ok(Classified::Wrapper { nonce, .. }) => problems.push(format!(
                "program {index} is an owned wrapper (nonce {nonce:#x}) after the revocation"
            )),
            Err(error) => problems.push(format!("program {index}: {error}")),
        }
    }
    if !same_multiset(&bares, originals) {
        problems.push(format!(
            "bare programs {}, want each runtime original attached bare exactly once {}",
            fingerprints(&bares),
            fingerprints(originals)
        ));
    }
    problems
}

/// What is wrong with `dump` as a reading of the cgroup of `baseline`: another path or inode means
/// the container's cgroup did not survive.
#[must_use]
pub fn cgroup_problem(baseline: &ProgramDump, dump: &ProgramDump) -> Option<String> {
    (baseline.cgroup != dump.cgroup || baseline.inode != dump.inode).then(|| {
        format!(
            "cgroup {} (inode {}), want the surviving {} (inode {})",
            dump.cgroup, dump.inode, baseline.cgroup, baseline.inode
        )
    })
}

/// A program that could not be read hides what it is: every program must be accounted for.
fn accounting_problems(dump: &ProgramDump) -> Vec<String> {
    let mut problems = Vec::new();
    if dump.inaccessible > 0 {
        problems.push(format!(
            "{} attached programs could not be opened",
            dump.inaccessible
        ));
    }
    if dump.total != dump.programs.len() + dump.inaccessible {
        problems.push(format!(
            "the kernel reports {} programs, the dump holds {}",
            dump.total,
            dump.programs.len()
        ));
    }
    problems
}

/// Whether `left` and `right` hold the same programs the same number of times.
fn same_multiset(left: &[Vec<u8>], right: &[Vec<u8>]) -> bool {
    let mut left = left.to_vec();
    let mut right = right.to_vec();
    left.sort();
    right.sort();
    left == right
}

/// A short name for a program in a message: a stable hash of its bytes and its instruction count.
fn fingerprint(raw: &[u8]) -> String {
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    format!("{:016x}/{} insns", hasher.finish(), raw.len() / 8)
}

fn fingerprints(programs: &[Vec<u8>]) -> String {
    let names: Vec<String> = programs.iter().map(|raw| fingerprint(raw)).collect();
    format!("[{}]", names.join(", "))
}

fn to_usize(value: u64) -> Result<usize, Error> {
    usize::try_from(value).map_err(|error| Error::new(format!("{value}: {error}")))
}

/// `raw` in lower-case hex.
fn hex(raw: &[u8]) -> String {
    raw.iter().fold(String::new(), |mut text, byte| {
        let _ = write!(text, "{byte:02x}");
        text
    })
}

/// The bytes `text` spells in hex.
fn unhex(text: &str) -> Result<Vec<u8>, Error> {
    let bad = || Error::new(format!("malformed hex {text:?}"));
    if !text.len().is_multiple_of(2) {
        return Err(bad());
    }
    (0..text.len())
        .step_by(2)
        .map(|index| {
            text.get(index..index + 2)
                .filter(|pair| pair.bytes().all(|digit| digit.is_ascii_hexdigit()))
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(bad)
        })
        .collect()
}

#[cfg(test)]
mod tests;
