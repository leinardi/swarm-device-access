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

//! The device rules of a container's `/dev` bind mounts: each name (the mount source, each
//! walked entry, each symlink) resolved on descriptors beneath `/dev`, identified by `fstat` and
//! sysfs `DEVNAME`, judged by policy under its alias, resolved and canonical names, and the
//! candidates aggregated per device across all mounts. Anything that leaves the set unknown is a
//! [`DeviceError`], and the caller then applies the empty desired set. Go's `rules.go` and the
//! collection half of `processor.go`.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use rustix::io::Errno;

use super::devfs::{DEV_ROOT, DevFs, DevNode, DeviceId, NodeKind, SysError, WalkControl};
use crate::cgroup::DeviceRule;
use crate::docker::types::MountPoint;
use crate::go_compat::filepath;
use crate::go_compat::quote::quote;
use crate::logger::{Attr, LogHandle};
use crate::metrics::Recorder;
use crate::policy::{Container, Global};

/// Bounds the names enumerated under one directory mount. A walk that cannot see every name
/// cannot know that no alias denies a device it did see, so overflow is an error, not a cutoff.
pub const MAX_MOUNT_ENTRIES: usize = 4096;
const MAX_LOG_CHILDREN: usize = 32;
/// Bounds the absolute symlinks followed by hand (see [`evaluate_candidate`]), like the kernel's
/// own symlink limit.
const MAX_LINK_HOPS: usize = 40;

const DEVICE_ACCESS_ALL: &str = "rwm";
const BIND_MOUNT: &str = "bind";

/// Candidate skip reasons, also the `sda_device_candidates_skipped_total` reason label.
pub const SKIP_OUTSIDE_DEV: &str = "outside_dev";
pub const SKIP_DANGLING: &str = "dangling";
pub const SKIP_NOT_DEVICE: &str = "not_device";

/// The policy outcome for one candidate.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Decision {
    /// The name that matched a deny glob, if one did.
    pub denied_by: Option<String>,
    /// Set when the canonical and the resolved name both pass the allow lists.
    pub authorized: bool,
}

impl Decision {
    fn granted(&self) -> bool {
        self.denied_by.is_none() && self.authorized
    }
}

/// One name found under a `/dev` mount (the mount source itself, or an entry of a directory
/// mount) and what it resolved to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Candidate {
    /// The name as found.
    pub alias: String,
    /// The `/dev` path its descriptor refers to.
    pub resolved: String,
    /// `/dev/` and the device's sysfs `DEVNAME`.
    pub canonical: String,
    pub kind: NodeKind,
    pub decision: Decision,
}

/// The outcome of collecting the candidates of one mount source. `skipped` counts entries that
/// name no device (dangling or outside-`/dev` symlinks, non-device entries); `errs` holds
/// failures that leave the container's device set unknown.
#[derive(Debug, Default)]
pub struct MountResult {
    pub candidates: Vec<Candidate>,
    pub skipped: usize,
    pub errs: Vec<DeviceError>,
    /// The skipped entries per skip reason.
    pub outcomes: BTreeMap<&'static str, usize>,
}

impl MountResult {
    fn add_skip(&mut self, reason: &'static str) {
        self.skipped += 1;
        *self.outcomes.entry(reason).or_default() += 1;
    }
}

/// Whether `path` is `/dev` or a path under `/dev/`.
#[must_use]
pub fn is_mount_source(path: &str) -> bool {
    path == DEV_ROOT || path.starts_with("/dev/")
}

/// The lexical gate on a name: already clean (no `..`, `//` or trailing `/`) and `/dev` or below.
fn under_dev(path: &str) -> bool {
    filepath::clean(path) == path && is_mount_source(path)
}

/// `path` relative to `/dev` (`.` for `/dev` itself).
fn rel_to_dev(path: &str) -> &str {
    if path == DEV_ROOT {
        return ".";
    }
    path.strip_prefix("/dev/").unwrap_or(path)
}

/// The policy a collection judges by, and where it logs.
#[derive(Clone, Copy)]
pub struct Judge<'policy> {
    pub global: &'policy Global,
    pub container: &'policy Container,
    pub log: &'policy LogHandle,
}

/// Evaluates the mount source and, for a directory, every entry below it. Each name goes through
/// [`evaluate_candidate`] on descriptors; the walk only enumerates names.
#[must_use]
pub fn collect_mount_rules(dev: &dyn DevFs, mount_path: &str, judge: Judge<'_>) -> MountResult {
    let mut result = MountResult::default();
    let source = evaluate_candidate(dev, mount_path, judge);
    if let Some(error) = source.err {
        result.errs.push(error);
        return result;
    }
    match (source.skip, source.candidate.kind) {
        (Some(SKIP_DANGLING), _) => {
            // Docker mounted it, so it existed; a device that went away leaves the set unknown
            // until it is back or the container ends.
            result
                .errs
                .push(DeviceError::MountSourceGone(mount_path.to_owned()));
        }
        (Some(reason), _) => result.add_skip(reason),
        (None, NodeKind::Dir) => {
            let dir = source.candidate.resolved;
            return walk_mount(dev, mount_path, &dir, judge, result);
        }
        (None, NodeKind::Other) => {
            // A single file mounted from /dev must be a device.
            result
                .errs
                .push(DeviceError::NotDevice(mount_path.to_owned()));
        }
        (None, NodeKind::Device(_)) => result.candidates.push(source.candidate),
    }
    result
}

/// Enumerates a directory mount. `dir` is the resolved directory; the candidates are named after
/// the mount source (their alias).
fn walk_mount(
    dev: &dyn DevFs,
    source: &str,
    dir: &str,
    judge: Judge<'_>,
    result: MountResult,
) -> MountResult {
    let mut state = MountWalkState {
        source,
        base: filepath::join(&[dev.root(), rel_to_dev(dir)]),
        dev,
        judge,
        result,
        policy_excluded: 0,
        children_seen: Vec::new(),
        entries: 0,
    };
    let base = state.base.clone();
    // Go's WalkDir returns only what the visit returned, and the visit ends a walk only with
    // SkipAll, which WalkDir does not return: the walk itself never fails.
    dev.walk(&base, &mut |path, is_dir, error| {
        state.visit_entry(path, is_dir, error)
    });

    let granted = state
        .result
        .candidates
        .iter()
        .filter(|candidate| candidate.decision.granted())
        .count();
    if granted == 0 && state.policy_excluded > 0 && state.result.errs.is_empty() {
        let globs = |global: Option<&[String]>, container: Option<&[String]>, key: &str| {
            let joined: Vec<&String> = global.into_iter().chain(container).flatten().collect();
            // Go's slices.Concat of nothing is nil.
            if joined.is_empty() {
                Attr::nil_strings(key)
            } else {
                Attr::strings(key, joined)
            }
        };
        judge.log.warn(
            "mount excluded: no children matched allow/deny policy",
            &[
                Attr::string("path", source),
                Attr::strings("children_seen", &state.children_seen),
                globs(
                    judge.global.device_allow(),
                    judge.container.device_allow(),
                    "allow_globs",
                ),
                globs(
                    judge.global.device_deny(),
                    judge.container.device_deny(),
                    "deny_globs",
                ),
            ],
        );
    }
    state.result
}

struct MountWalkState<'walk> {
    source: &'walk str,
    base: String,
    dev: &'walk dyn DevFs,
    judge: Judge<'walk>,
    result: MountResult,
    /// The device candidates the policy did not grant.
    policy_excluded: usize,
    children_seen: Vec<String>,
    /// Every name enumerated, directories included.
    entries: usize,
}

impl MountWalkState<'_> {
    fn visit_entry(&mut self, walked: &str, is_dir: bool, error: Option<SysError>) -> WalkControl {
        let alias = format!(
            "{}{}",
            self.source,
            walked.strip_prefix(self.base.as_str()).unwrap_or(walked)
        );

        // The root failing to open, a directory that cannot be read, any walk error: some name
        // was not seen, so the set is unknown.
        if let Some(error) = error {
            self.result.errs.push(DeviceError::IncompleteWalk {
                name: alias,
                cause: error,
            });
            return WalkControl::Continue;
        }
        if walked == self.base {
            return WalkControl::Continue;
        }
        self.entries += 1;
        if self.entries > MAX_MOUNT_ENTRIES {
            self.result
                .errs
                .push(DeviceError::MountTooLarge(self.source.to_owned()));
            return WalkControl::SkipAll;
        }
        if is_dir {
            return WalkControl::Continue;
        }
        if self.children_seen.len() < MAX_LOG_CHILDREN {
            self.children_seen.push(filepath::base(walked).to_owned());
        }

        let found = evaluate_candidate(self.dev, &alias, self.judge);
        let log = self.judge.log;
        if let Some(error) = found.err {
            self.result.errs.push(error);
            return WalkControl::Continue;
        }
        if let Some(reason) = found.skip {
            if reason == SKIP_DANGLING {
                skip_unresolvable(&alias, self.judge);
            }
            self.result.add_skip(reason);
            return WalkControl::Continue;
        }
        match found.candidate.kind {
            NodeKind::Dir => {
                log.debug(
                    "symlink to directory skipped",
                    &[
                        Attr::string("path", alias.as_str()),
                        Attr::string("target", found.candidate.resolved.as_str()),
                    ],
                );
                self.result.add_skip(SKIP_NOT_DEVICE);
            }
            NodeKind::Other => {
                log.debug(
                    "non-device entry skipped",
                    &[Attr::string("path", alias.as_str())],
                );
                self.result.add_skip(SKIP_NOT_DEVICE);
            }
            NodeKind::Device(_) => {
                if !found.candidate.decision.granted() {
                    self.policy_excluded += 1;
                }
                self.result.candidates.push(found.candidate);
            }
        }
        WalkControl::Continue
    }
}

/// A candidate, or why it is not one.
#[derive(Debug)]
pub struct Evaluated {
    pub candidate: Candidate,
    /// Set when the name is not a device candidate: one of the skip reasons.
    pub skip: Option<&'static str>,
    /// Set when the name exists but its identity could not be established.
    pub err: Option<DeviceError>,
}

/// Establishes what `alias` names, on descriptors rather than re-walked paths, so an entry
/// swapped between enumeration and use cannot lend its name to another device:
///
/// 1. `alias` must be clean and under `/dev`;
/// 2. it is opened beneath `/dev` (`openat2` `RESOLVE_BENEATH`): symlinks are followed but
///    nothing outside `/dev` is reached. A planted absolute link into `/dev` (`-> /dev/sda`) is
///    followed by hand, bounded; any other escape is skipped as `outside_dev`, a dangling name
///    as `dangling`;
/// 3. `fstat` on the descriptor gives the type and major:minor;
/// 4. sysfs gives the canonical name (`DEVNAME`);
/// 5. policy is evaluated on the alias, the resolved and the canonical name
///    ([`evaluate_identity`]).
///
/// A name that exists but whose identity cannot be established returns an error: the caller
/// must then treat the container's whole device set as unknown.
#[must_use]
pub fn evaluate_candidate(dev: &dyn DevFs, alias: &str, judge: Judge<'_>) -> Evaluated {
    let mut found = Evaluated {
        candidate: Candidate {
            alias: alias.to_owned(),
            resolved: String::new(),
            canonical: String::new(),
            kind: NodeKind::Other,
            decision: Decision::default(),
        },
        skip: None,
        err: None,
    };
    if !under_dev(alias) {
        judge.log.warn(
            "device path resolves outside /dev",
            &[
                Attr::string("path", alias),
                Attr::string("reason", SKIP_OUTSIDE_DEV),
            ],
        );
        found.skip = Some(SKIP_OUTSIDE_DEV);
        return found;
    }
    let node = match open_candidate(dev, alias, judge) {
        Ok(Opened::Node(node)) => node,
        Ok(Opened::Skip(reason)) => {
            found.skip = Some(reason);
            return found;
        }
        Err(error) => {
            found.err = Some(error);
            return found;
        }
    };
    found.candidate.kind = match node.stat() {
        Ok(kind) => kind,
        Err(error) => {
            found.err = Some(DeviceError::unresolved(alias, None, Cause::Sys(error)));
            return found;
        }
    };
    found.candidate.resolved = node.resolved();
    if found.candidate.resolved.is_empty() {
        alias.clone_into(&mut found.candidate.resolved);
    }
    let NodeKind::Device(id) = found.candidate.kind else {
        return found;
    };
    let candidate = &mut found.candidate;
    match canonical_name(dev, id) {
        Ok(canonical) => {
            candidate.canonical = canonical;
            candidate.decision = evaluate_identity(
                alias,
                &candidate.resolved,
                &candidate.canonical,
                judge.global,
                judge.container,
            );
        }
        Err(cause) => {
            // Without a canonical name the device can never be authorized, so the candidate only
            // matters if its other names would grant it: then the set is unknown. Excluded by
            // policy under those names, it is simply not granted.
            candidate.decision = evaluate_identity(
                alias,
                &candidate.resolved,
                &candidate.resolved,
                judge.global,
                judge.container,
            );
            if candidate.decision.granted() {
                found.err = Some(DeviceError::unresolved(
                    alias,
                    Some((candidate.resolved.clone(), id)),
                    cause,
                ));
            }
            candidate.decision.authorized = false;
        }
    }
    found
}

/// What opening a candidate gave.
enum Opened {
    Node(Box<dyn DevNode>),
    /// The name names nothing under `/dev`: a skip reason.
    Skip(&'static str),
}

/// Opens `alias` beneath `/dev`, following a planted absolute symlink into `/dev` by hand (up to
/// the hop limit).
fn open_candidate(dev: &dyn DevFs, alias: &str, judge: Judge<'_>) -> Result<Opened, DeviceError> {
    let mut rel = rel_to_dev(alias).to_owned();
    for _ in 0..MAX_LINK_HOPS {
        let error = match dev.open_beneath(&rel) {
            Ok(node) => return Ok(Opened::Node(node)),
            Err(error) if error.errno() == Errno::NOENT => {
                return Ok(Opened::Skip(SKIP_DANGLING));
            }
            Err(error) => error,
        };
        if error.errno() != Errno::XDEV {
            return Err(DeviceError::unresolved(alias, None, Cause::Sys(error)));
        }
        match escape_hop(dev, &rel) {
            Err(error) => return Err(DeviceError::unresolved(alias, None, Cause::Sys(error))),
            Ok(Hop::Outside(target)) => {
                skip_outside(alias, &target, judge);
                return Ok(Opened::Skip(SKIP_OUTSIDE_DEV));
            }
            Ok(Hop::Retry(next)) => rel = next,
        }
    }
    Err(DeviceError::unresolved(alias, None, Cause::TooManyLinks))
}

/// The outcome of an escape hop.
enum Hop {
    /// The path to retry.
    Retry(String),
    /// The escape really leaves `/dev`: what it pointed at.
    Outside(String),
}

/// Handles a lookup of `rel` that left `/dev` (`EXDEV`). The kernel refuses an absolute symlink
/// even when it points back into `/dev`, so the first component that escapes is found and its
/// link read: an absolute target inside `/dev` replaces that prefix, and the result is the path
/// to retry.
fn escape_hop(dev: &dyn DevFs, rel: &str) -> Result<Hop, SysError> {
    let parts: Vec<&str> = rel.split('/').collect();
    for index in 0..parts.len() {
        let prefix = parts[..=index].join("/");
        let Err(error) = dev.open_beneath(&prefix) else {
            continue;
        };
        if error.errno() != Errno::XDEV {
            return Err(error);
        }
        let target = dev.read_link(&prefix)?;
        let cleaned = filepath::clean(&target);
        if !target.starts_with('/') || !under_dev(&cleaned) {
            return Ok(Hop::Outside(target));
        }
        let mut next = vec![rel_to_dev(&cleaned)];
        next.extend_from_slice(&parts[index + 1..]);
        return Ok(Hop::Retry(filepath::join(&next)));
    }
    // Every prefix opens on its own: the whole name no longer escapes.
    Ok(Hop::Retry(rel.to_owned()))
}

/// The device's `DEVNAME` from sysfs. The uevent must hold exactly one `DEVNAME` line whose value
/// is a clean relative path without `..`.
fn canonical_name(dev: &dyn DevFs, id: DeviceId) -> Result<String, Cause> {
    let data = dev.read_uevent(id).map_err(Cause::Sys)?;
    let names: Vec<String> = crate::go_compat::scanner::lines(&data)
        .map_while(Result::ok)
        .filter_map(|line| {
            String::from_utf8_lossy(line)
                .strip_prefix("DEVNAME=")
                .map(ToOwned::to_owned)
        })
        .collect();
    let [name] = names.as_slice() else {
        return Err(Cause::BadDevname(BadDevname::Count(names.len())));
    };
    if name.is_empty()
        || name.starts_with('/')
        || filepath::clean(name) != *name
        || name == ".."
        || name.starts_with("../")
    {
        return Err(Cause::BadDevname(BadDevname::Invalid(name.clone())));
    }
    Ok(format!("{DEV_ROOT}/{name}"))
}

/// Applies policy to one candidate. Deny globs are checked on every name the device was found
/// by: any match is a deny vote. The allow lists must pass for the canonical and the resolved
/// name; an alias missing them is not a vote either way (a by-path or by-id link is not what
/// authorizes a device).
#[must_use]
pub fn evaluate_identity(
    alias: &str,
    resolved: &str,
    canonical: &str,
    global: &Global,
    container: &Container,
) -> Decision {
    for name in [alias, resolved, canonical] {
        if global.denied(container, name) {
            return Decision {
                denied_by: Some(name.to_owned()),
                authorized: false,
            };
        }
    }
    Decision {
        denied_by: None,
        authorized: global.authorized(container, canonical)
            && global.authorized(container, resolved),
    }
}

/// Logs a dangling name: a warning when an explicit allow glob names it, since the operator
/// expected a device there.
fn skip_unresolvable(alias: &str, judge: Judge<'_>) {
    let attrs = [Attr::string("path", alias)];
    if judge.global.explicitly_allowed(judge.container, alias) {
        judge.log.warn(
            "device symlink matches allow policy but cannot be resolved",
            &attrs,
        );
    } else {
        judge.log.debug("unresolvable symlink skipped", &attrs);
    }
}

/// Logs a symlink that leads outside `/dev`: a warning when an explicit allow glob names it,
/// since the operator expected a device there. Otherwise debug: every whole-`/dev` mount holds
/// such links (`/dev/stdin -> /proc/self/fd/0`, `/dev/log -> /run/systemd/journal/dev-log`), and
/// a warning for each of them on every pass buries the warnings that matter.
fn skip_outside(alias: &str, target: &str, judge: Judge<'_>) {
    let attrs = [
        Attr::string("path", alias),
        Attr::string("target", target),
        Attr::string("reason", SKIP_OUTSIDE_DEV),
    ];
    if judge.global.explicitly_allowed(judge.container, alias) {
        judge.log.warn("device path resolves outside /dev", &attrs);
    } else {
        judge.log.debug("device path resolves outside /dev", &attrs);
    }
}

/// Every candidate that resolved to one device.
#[derive(Debug)]
struct DeviceVotes {
    candidates: usize,
    authorized: bool,
    denied_by: Option<String>,
}

/// Collects candidates per device across all mounts of a container, so one name's deny vote
/// holds against every other name of the same device.
#[derive(Debug, Default)]
struct Aggregation {
    order: Vec<DeviceId>,
    votes: HashMap<DeviceId, DeviceVotes>,
}

impl Aggregation {
    fn add(&mut self, found: &Candidate) {
        let NodeKind::Device(id) = found.kind else {
            return;
        };
        let votes = self.votes.entry(id).or_insert_with(|| {
            self.order.push(id);
            DeviceVotes {
                candidates: 0,
                authorized: false,
                denied_by: None,
            }
        });
        votes.candidates += 1;
        if votes.denied_by.is_none() {
            votes.denied_by.clone_from(&found.decision.denied_by);
        }
        votes.authorized |= found.decision.authorized;
    }

    /// The rules for every device that is authorized and has no deny vote, in the order devices
    /// were first seen, and how many candidates were not granted.
    fn emit(&self, container_id: &str, log: &LogHandle) -> (Vec<DeviceRule>, usize) {
        let mut rules = Vec::new();
        let mut skipped = 0;
        for device in &self.order {
            let votes = &self.votes[device];
            let id = Attr::string("id", container_id);
            let named = Attr::string("device", device.to_string());
            match (&votes.denied_by, votes.authorized) {
                (Some(denied_by), true) => {
                    log.info(
                        "device denied by policy under one of its names",
                        &[id, named, Attr::string("denied_by", denied_by.as_str())],
                    );
                    skipped += votes.candidates;
                }
                (Some(denied_by), false) => {
                    log.debug(
                        "device excluded by policy",
                        &[id, named, Attr::string("denied_by", denied_by.as_str())],
                    );
                    skipped += votes.candidates;
                }
                (None, false) => {
                    log.debug("device not in the allow list", &[id, named]);
                    skipped += votes.candidates;
                }
                (None, true) => rules.push(DeviceRule {
                    allow: true,
                    kind: device.kind.letter().to_owned(),
                    major: Some(i64::from(device.major)),
                    minor: Some(i64::from(device.minor)),
                    access: DEVICE_ACCESS_ALL.to_owned(),
                }),
            }
        }
        (rules, skipped)
    }
}

/// The per-mount results for one container, aggregated.
#[derive(Debug, Default)]
pub struct ContainerRules {
    /// One rule per granted device.
    pub granted: Vec<DeviceRule>,
    pub skipped: usize,
    pub device_errs: Vec<DeviceError>,
    pub dev_mounts: usize,
}

/// Whether `mount` bind-mounts something from `/dev`. Only a bind mount's source is a host path;
/// a volume or tmpfs mount is never walked, whatever its source says.
#[must_use]
pub fn is_device_mount(mount: &MountPoint) -> bool {
    mount.kind == BIND_MOUNT && is_mount_source(&mount.source)
}

/// Evaluates every `/dev` mount of a container and aggregates the candidates per device across
/// all of them: a device is granted only when it is authorized and no name it was found by is
/// denied. Per-device errors are collected; any error leaves the set unknown.
#[must_use]
pub fn collect_container_rules(
    dev: &dyn DevFs,
    container_id: &str,
    pid: i64,
    mounts: &[MountPoint],
    judge: Judge<'_>,
    metrics: &Recorder,
) -> ContainerRules {
    let mut result = ContainerRules::default();
    let mut devices = Aggregation::default();
    for mount in mounts.iter().filter(|mount| is_device_mount(mount)) {
        result.dev_mounts += 1;
        judge.log.debug(
            "device mount detected",
            &[
                Attr::string("id", container_id),
                Attr::int("pid", pid),
                Attr::string("source", mount.source.as_str()),
                Attr::string("destination", mount.destination.as_str()),
            ],
        );
        let mount_result = collect_mount_rules(dev, &mount.source, judge);
        result.skipped += mount_result.skipped;
        result.device_errs.extend(mount_result.errs);
        for (reason, count) in mount_result.outcomes {
            metrics.add_candidates_skipped(reason, count);
        }
        for candidate in &mount_result.candidates {
            devices.add(candidate);
        }
    }
    let (granted, excluded) = devices.emit(container_id, judge.log);
    result.granted = granted;
    result.skipped += excluded;
    result
}

/// Why a container's device set is unknown, worded as the Go daemon's errors.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DeviceError {
    /// Go's `errUnresolved`: a candidate whose device identity could not be established.
    Unresolved {
        alias: String,
        /// The resolved name and the device, once known.
        device: Option<(String, DeviceId)>,
        cause: Cause,
    },
    /// The mount source Docker mounted no longer exists (`errUnresolved`).
    MountSourceGone(String),
    /// Go's `errNotDevice`: a single file mounted from `/dev` that is not a device.
    NotDevice(String),
    /// Go's `errIncompleteWalk`: a name of a directory mount could not be enumerated.
    IncompleteWalk { name: String, cause: SysError },
    /// Go's `errMountTooLarge` (an `errIncompleteWalk`): a directory mount over the entry cap.
    MountTooLarge(String),
}

impl DeviceError {
    fn unresolved(alias: &str, device: Option<(String, DeviceId)>, cause: Cause) -> Self {
        Self::Unresolved {
            alias: alias.to_owned(),
            device,
            cause,
        }
    }

    /// Whether it is Go's `errUnresolved`.
    #[must_use]
    pub const fn is_unresolved(&self) -> bool {
        matches!(self, Self::Unresolved { .. } | Self::MountSourceGone(_))
    }

    /// Whether it is Go's `errIncompleteWalk`.
    #[must_use]
    pub const fn is_incomplete_walk(&self) -> bool {
        matches!(self, Self::IncompleteWalk { .. } | Self::MountTooLarge(_))
    }
}

/// `device identity could not be established`, Go's `errUnresolved`.
const UNRESOLVED: &str = "device identity could not be established";
/// `device mount enumeration incomplete`, Go's `errIncompleteWalk`.
const INCOMPLETE_WALK: &str = "device mount enumeration incomplete";

impl fmt::Display for DeviceError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unresolved {
                alias,
                device: None,
                cause,
            } => write!(formatter, "{UNRESOLVED}: {alias}: {cause}"),
            Self::Unresolved {
                alias,
                device: Some((resolved, id)),
                cause,
            } => write!(
                formatter,
                "{UNRESOLVED}: {alias} (resolved {resolved}, {id}): {cause}"
            ),
            Self::MountSourceGone(path) => write!(
                formatter,
                "{UNRESOLVED}: mount source {path}: {}",
                SysError::new("", Errno::NOENT)
            ),
            Self::NotDevice(path) => write!(
                formatter,
                "device {} is neither character nor block device",
                quote(path)
            ),
            Self::IncompleteWalk { name, cause } => {
                write!(formatter, "{INCOMPLETE_WALK}: {name}: {cause}")
            }
            Self::MountTooLarge(source) => write!(
                formatter,
                "{INCOMPLETE_WALK}: {source}: mount too large; narrow the bind mount: more than \
                 {MAX_MOUNT_ENTRIES} entries"
            ),
        }
    }
}

impl std::error::Error for DeviceError {}

/// Why a candidate's identity could not be established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// A system call on the name or its device failed.
    Sys(SysError),
    /// Go's `errBadDevname`.
    BadDevname(BadDevname),
    /// Go's `errTooManyLinks`: a chain of absolute symlinks over the hop limit.
    TooManyLinks,
}

impl fmt::Display for Cause {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Sys(error) => error.fmt(formatter),
            Self::BadDevname(error) => error.fmt(formatter),
            Self::TooManyLinks => formatter.write_str("too many levels of symbolic links"),
        }
    }
}

/// A sysfs uevent without exactly one valid `DEVNAME`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadDevname {
    /// Not exactly one `DEVNAME` line.
    Count(usize),
    /// A value that is not a clean relative path without `..`.
    Invalid(String),
}

impl fmt::Display for BadDevname {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("uevent has no single valid DEVNAME: ")?;
        match self {
            Self::Count(count) => write!(formatter, "{count} DEVNAME lines"),
            Self::Invalid(name) => formatter.write_str(&quote(name)),
        }
    }
}

#[cfg(test)]
mod tests;
