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

//! The processor: the one idempotent reconcile of a container's device grants, the revocation of
//! ended runs, the sweep and the publication of a new config. Go's `processor.go`,
//! `reconcile.go` and `publish.go`.
//!
//! Everything Go does under the processor lock runs here as one blocking transaction on
//! tokio's blocking pool: it takes the lock (a `std::sync::Mutex`) inside the closure and owns
//! the pidfd, the `/proc`, `/dev` and cgroup descriptors and the program handles until it
//! returns. The async caller awaits its `JoinHandle` to completion, and a caller that is dropped
//! or cancelled meanwhile does not end it: the next reconcile waits on the lock, and every
//! transaction is tracked, so shutdown waits for it ([`Processor::close_and_wait`]). Only the Docker
//! inspects are cancellable: the first one runs before the transaction, outside the lock, as in
//! Go; the re-inspects of the verification are driven from inside it, under the caller's
//! context, and a cancelled one fails the verification before anything is mutated.
//!
//! The lifecycle history is the processor's own, shared with whoever needs it
//! ([`Processor::lifecycles`]), where Go's coordinator installed its own with `SetLifecycles`.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use jiff::{SignedDuration, Timestamp};
use rustix::io::Errno;
use tokio::runtime::Handle;
use tokio_util::task::TaskTracker;

use super::devfs::{DEV_ROOT, DevFs, RealDevFs, SysError, sysfs_root_for};
use super::error::{Error, VerifyCause, join};
use super::lifecycle::{LifecycleRecord, LifecycleState, Lifecycles, RunCgroup};
use super::pin::{PidfdPinner, PinnedProcess, ProcessPinner, host_cgroup_path, read_proc_cgroup};
use super::rules::{ContainerRules, DeviceError, Judge, collect_container_rules, is_device_mount};
use crate::cgroup::{
    self, CgroupHandle, CgroupVersion, DeviceRule, FilterCache, HandleError, Identity, Interface,
    Ledger,
};
use crate::clock::Clock;
use crate::config::{Publisher, Runtime, Store};
use crate::docker::types::{ContainerInspect, ContainerState, MountPoint};
use crate::docker::{self, CallContext, ContextError, DockerApi, with_context};
use crate::go_compat::quote::quote;
use crate::logger::{Attr, LogHandle};
use crate::metrics::Recorder;
use crate::policy::{Container, Global, unknown_labels};
use crate::task::finish;

/// How long a provisional record without a cgroup is kept before the sweep drops it: it names no
/// cgroup, so nothing can be revoked through it.
pub const PROVISIONAL_TTL: Duration = Duration::from_mins(10);

/// The Docker call the processor makes: Go's `DockerInspector`. Policy comes from the global
/// config and the container's own labels only, so no service or node inspection is part of it.
pub trait Inspector: Send + Sync + 'static {
    /// moby's `ContainerInspect`.
    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, docker::Error>> + Send;
}

impl<Api: DockerApi + 'static> Inspector for Api {
    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, docker::Error>> + Send {
        DockerApi::container_inspect(self, id)
    }
}

/// Asks for a reconciliation pass over every running container under a config generation (or a
/// newer one). It must not block on the pass.
pub type PassRequester = Arc<dyn Fn(&CallContext, u64) + Send + Sync>;

/// What a processor is built from.
pub struct Settings<I> {
    pub inspector: I,
    pub store: Arc<Store>,
    /// The one publisher of the store, used by [`Processor::publish_and_reconcile`] only.
    pub publisher: Publisher,
    pub metrics: Recorder,
    /// The container-internal path of the host root (`/host`).
    pub host_root: String,
    /// The root `/proc` is read under (`/` in production).
    pub proc_root: String,
    /// Bounds each Docker call the processor makes, independent of the caller's context; `None`
    /// leaves the calls bounded by the caller's context only.
    pub call_timeout: Option<Duration>,
    pub clock: Arc<dyn Clock>,
    pub log: LogHandle,
}

/// Builds the cgroup controller of a version.
type NewCgroup = dyn Fn(CgroupVersion, Arc<Ledger>, Arc<FilterCache>, LogHandle) -> Box<dyn Interface>
    + Send
    + Sync;

/// Opens the view of `/dev` and sysfs for one pass.
type OpenDevFs = dyn Fn() -> Result<Box<dyn DevFs>, SysError> + Send + Sync;

/// What tests replace: the pinner, the cgroup controllers, the `/dev` view, and a hook run under
/// the lock between computing and applying the rules.
pub(super) struct Seams {
    pub(super) pinner: Box<dyn ProcessPinner>,
    pub(super) new_cgroup: Box<NewCgroup>,
    pub(super) devfs: Option<Box<OpenDevFs>>,
    pub(super) after_compute: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Default for Seams {
    fn default() -> Self {
        Self {
            pinner: Box::new(PidfdPinner),
            new_cgroup: Box::new(cgroup::new),
            devfs: None,
            after_compute: None,
        }
    }
}

/// What the processor lock guards besides the transactions it serializes.
#[derive(Default)]
struct Locked {
    request_pass: Option<PassRequester>,
}

/// Applies cgroup device rules to containers that bind-mount `/dev` paths.
pub struct Processor<I> {
    inspector: I,
    store: Arc<Store>,
    publisher: Publisher,
    metrics: Recorder,
    host_root: String,
    proc_root: String,
    call_timeout: Option<Duration>,
    log: LogHandle,
    /// Serializes policy evaluation and cgroup mutation across every caller (event loop,
    /// startup enumeration, systemd re-apply, reload). It is held from the config load through
    /// the mutation, not around the mutation alone: otherwise a worker that computed rules under
    /// an old config could apply them after a newer config was published and a newer computation
    /// had already been applied. Any Docker I/O made while holding it is bounded by
    /// `call_timeout`.
    lock: Mutex<Locked>,
    /// The cgroup v1 grants this processor made, so a later set can revoke them without touching
    /// the runtime's own exceptions.
    ledger: Arc<Ledger>,
    /// The cgroup v2 runtime device filters seen per cgroup, so a filter wiped by systemd's
    /// daemon-reload can be rebuilt.
    filter_cache: Arc<FilterCache>,
    /// The history of container runs and the cgroups they were verified in. It is how grants are
    /// revoked once the process is gone or Docker cannot be asked: see `revoke_at`.
    lifecycles: Arc<Lifecycles>,
    /// Every blocking transaction, so shutdown can wait for those whose caller is gone.
    transactions: TaskTracker,
    seams: Seams,
}

/// The outcome of evaluating a container's labels, the current policy and its `/dev` mounts.
#[derive(Default)]
struct Desired {
    /// What the container's cgroup must hold. It is empty whenever the set cannot be established
    /// with confidence.
    rules: Vec<DeviceRule>,
    enabled: bool,
    collected: ContainerRules,
    /// Set when some device of an enabled container could not be resolved; the container is
    /// then retried.
    incomplete: Option<Error>,
}

/// What `apply_pinned` holds while it verifies: the caller's runtime and context, the run, its
/// pinned process and the opened cgroup.
#[derive(Clone, Copy)]
struct Pinned<'held> {
    runtime: &'held Handle,
    context: &'held CallContext,
    container_id: &'held str,
    state: &'held ContainerState,
    process: &'held dyn PinnedProcess,
    handle: &'held CgroupHandle,
}

/// A recorded cgroup reopened, or why it holds none of the run's grants.
enum Recorded {
    Open(CgroupHandle),
    Gone(&'static str),
}

impl<I: Inspector> Processor<I> {
    /// A processor with the production pinner, cgroup controllers and `/dev`.
    #[must_use]
    pub fn new(settings: Settings<I>) -> Arc<Self> {
        Arc::new(Self::with_seams(settings, Seams::default()))
    }

    pub(super) fn with_seams(settings: Settings<I>, seams: Seams) -> Self {
        Self {
            inspector: settings.inspector,
            store: settings.store,
            publisher: settings.publisher,
            metrics: settings.metrics,
            host_root: settings.host_root,
            proc_root: settings.proc_root,
            call_timeout: settings.call_timeout,
            log: settings.log,
            lock: Mutex::new(Locked::default()),
            ledger: Arc::new(Ledger::default()),
            filter_cache: Arc::new(FilterCache::new()),
            lifecycles: Arc::new(Lifecycles::new(settings.clock)),
            transactions: TaskTracker::new(),
            seams,
        }
    }

    /// The generation of the config in force: the one a pass is requested under.
    #[must_use]
    pub fn generation(&self) -> u64 {
        self.store.generation()
    }

    /// The lifecycle history of container runs.
    #[must_use]
    pub fn lifecycles(&self) -> &Arc<Lifecycles> {
        &self.lifecycles
    }

    /// Makes the device grants of a container's cgroup equal exactly what the current config
    /// allows it. Start, unpause, startup enumeration, systemd re-apply and reload all run this
    /// one idempotent path.
    ///
    /// Every outcome that cannot establish the desired set with confidence (policy disabled or
    /// not opted in, invalid labels, an unresolved device) is the empty set, and the empty set is
    /// applied like any other: returning early would keep grants the current config no longer
    /// allows. The cost is that a transient error revokes the container's grants until a retry
    /// grants them again.
    ///
    /// The container's process is pinned (see `apply_pinned`), so a recycled pid cannot direct
    /// the rules to another cgroup. A container that is no longer running, or that Docker cannot
    /// report on, has its grants revoked in the cgroup its lifecycle was last verified in.
    ///
    /// # Errors
    ///
    /// [`Error`] when the container is to be retried, or is gone
    /// ([`Error::is_container_gone`]).
    pub async fn reconcile(
        self: &Arc<Self>,
        context: &CallContext,
        container_id: &str,
    ) -> Result<(), Error> {
        let inspected = self.inspect(context, container_id).await;
        let this = Arc::clone(self);
        let context = context.clone();
        let container_id = container_id.to_owned();
        let runtime = Handle::current();
        finish(self.transactions.spawn_blocking(move || {
            this.reconcile_locked(&runtime, &context, &container_id, inspected)
        }))
        .await
        .unwrap_or(Err(Error::Canceled))
    }

    /// Handles an event that ended a run of the container at `event_time` (die, destroy). The
    /// event names no run, so it is resolved against the history: the verified run with the
    /// latest start not after `event_time` (never a newer run under the same ID), else the
    /// provisional records. The empty set is applied in that run's cgroup; the record stays
    /// until the sweep verifies the cgroup gone. Dry-run never mutates.
    ///
    /// # Errors
    ///
    /// [`Error`] when a revocation failed.
    pub async fn terminate(
        self: &Arc<Self>,
        container_id: &str,
        event_time: Timestamp,
    ) -> Result<(), Error> {
        let this = Arc::clone(self);
        let container_id = container_id.to_owned();
        finish(
            self.transactions
                .spawn_blocking(move || this.terminate_locked(&container_id, event_time)),
        )
        .await
        .unwrap_or(Err(Error::Canceled))
    }

    /// Re-checks every lifecycle record: a record whose cgroup no longer exists, or was recreated
    /// with another inode, is released together with what the cgroup API remembered for it; an
    /// ended run whose revoke has not succeeded yet is revoked again; records that never named a
    /// cgroup are dropped once ended or stale. Returns how many records it released.
    pub async fn sweep(self: &Arc<Self>) -> usize {
        let this = Arc::clone(self);
        finish(
            self.transactions
                .spawn_blocking(move || this.sweep_locked()),
        )
        .await
        .unwrap_or(0)
    }

    /// Installs the function [`Processor::publish_and_reconcile`] hands each new generation to
    /// (the daemon's reconciliation coordinator).
    pub async fn set_pass_requester(self: &Arc<Self>, request: PassRequester) {
        let this = Arc::clone(self);
        finish(self.transactions.spawn_blocking(move || {
            this.locked().request_pass = Some(request);
        }))
        .await;
    }

    /// Waits for every blocking transaction started so far to complete, those whose caller was
    /// dropped or cancelled included: the daemon's shutdown waits here before the runtime stops.
    /// Transactions started later are still tracked and waited for while this waits.
    pub async fn close_and_wait(&self) {
        self.transactions.close();
        self.transactions.wait().await;
    }

    /// Publishes `runtime` as the new config and requests a pass that reconciles every running
    /// container under it. It is the only way the config changes after startup.
    ///
    /// The publication takes the same lock as the reconcile, so no container is between loading
    /// the config and mutating its cgroup when the generation changes: a worker that computed
    /// under the old config finishes first, and the pass then overwrites its result. The pass is
    /// requested after the lock is released and takes it per container, so it cannot deadlock
    /// against this call.
    pub async fn publish_and_reconcile(
        self: &Arc<Self>,
        context: &CallContext,
        runtime: Runtime,
    ) -> u64 {
        let this = Arc::clone(self);
        let published = finish(self.transactions.spawn_blocking(move || {
            let locked = this.locked();
            let generation = this.publisher.publish(runtime);
            (generation, locked.request_pass.clone())
        }))
        .await;
        let Some((generation, request)) = published else {
            return 0;
        };
        if let Some(request) = request {
            request(context, generation);
        }
        generation
    }

    /// One inspect, bounded by the call timeout.
    async fn inspect(
        &self,
        context: &CallContext,
        container_id: &str,
    ) -> Result<ContainerInspect, Error> {
        let call = match self.call_timeout {
            Some(timeout) if !timeout.is_zero() => context.with_timeout(timeout),
            Some(_) | None => context.clone(),
        };
        with_context(&call, self.inspector.container_inspect(container_id))
            .await
            .map_err(|source| Error::Inspect {
                id: container_id.to_owned(),
                source,
            })
    }

    fn locked(&self) -> MutexGuard<'_, Locked> {
        // Under panic = "abort" a poisoned lock is only reachable in tests; what it guards is
        // replaced whole.
        self.lock.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Everything after the first inspect, under the lock.
    fn reconcile_locked(
        &self,
        runtime: &Handle,
        context: &CallContext,
        container_id: &str,
        inspected: Result<ContainerInspect, Error>,
    ) -> Result<(), Error> {
        let _locked = self.locked();
        let config = self.store.load();
        let info = match inspected {
            Ok(info) => info,
            // The caller cancelled (daemon shutdown), not Docker: nothing would retry and grant
            // again, so a running container keeps its grants. A deadline is Docker not
            // answering in time, and does revoke.
            Err(inspect_error) if context.err() == Some(ContextError::Canceled) => {
                return Err(inspect_error);
            }
            Err(inspect_error) => {
                return self.revoke_after_inspect_failure(
                    container_id,
                    config.dry_run,
                    inspect_error,
                );
            }
        };
        let Some(state) = info
            .state
            .as_ref()
            .filter(|state| state.running && state.pid != 0)
        else {
            self.log.debug(
                "container has no live pid; skipping",
                &[Attr::string("id", container_id)],
            );
            self.metrics.record_container_skipped("no_pid");
            if config.dry_run {
                return Ok(());
            }
            let started_at = info
                .state
                .as_ref()
                .map(|state| state.started_at.as_str())
                .unwrap_or_default();
            return self.revoke_ended(container_id, started_at);
        };

        let desired = self.compute_desired(container_id, &info, state.pid, &config);
        if let Some(after_compute) = &self.seams.after_compute {
            after_compute();
        }
        if config.dry_run {
            self.log_dry_run(container_id, &desired.rules);
        } else {
            let privileged = info
                .host_config
                .as_ref()
                .is_some_and(|host| host.privileged);
            let (cgroup_path, applied) = self.apply_pinned(
                runtime,
                context,
                container_id,
                state,
                privileged,
                &desired.rules,
            );
            if let Err(apply_error) = applied {
                // A container skipped for good still reports an incomplete set.
                return join([
                    self.classify_apply_error(container_id, &cgroup_path, privileged, apply_error),
                    desired.incomplete,
                ])
                .map_or(Ok(()), Err);
            }
        }
        if desired.enabled && desired.collected.dev_mounts > 0 {
            self.log.info(
                "container processed",
                &[
                    Attr::string("id", container_id),
                    Attr::int("pid", state.pid),
                    Attr::int("devices_granted", count(desired.rules.len())),
                    Attr::int("skipped", count(desired.collected.skipped)),
                    Attr::int("errors", count(desired.collected.device_errs.len())),
                    Attr::bool("dry_run", config.dry_run),
                ],
            );
        }
        desired.incomplete.map_or(Ok(()), Err)
    }

    /// Evaluates the container's labels and `/dev` mounts under `config`. It never touches the
    /// container's cgroup.
    fn compute_desired(
        &self,
        container_id: &str,
        info: &ContainerInspect,
        pid: i64,
        config: &Runtime,
    ) -> Desired {
        let labels: HashMap<String, String> = info
            .config
            .as_ref()
            .map(|config| config.labels.clone().into_iter().collect())
            .unwrap_or_default();
        // The container's labels are the only label input, so this is the one place a
        // misspelled key can be reported instead of silently ignored.
        for unknown in unknown_labels(&labels) {
            self.log.warn(
                "unrecognized swarm-device-access label on container",
                &[
                    Attr::string("id", container_id),
                    Attr::string("label", unknown),
                ],
            );
        }
        let container = match Container::parse(&labels) {
            Ok(container) => container,
            Err(parse_error) => {
                self.log.warn(
                    "container skipped: invalid policy labels",
                    &[
                        Attr::string("id", container_id),
                        Attr::error("err", &parse_error),
                    ],
                );
                self.metrics.record_container_skipped("invalid_labels");
                return Desired::default();
            }
        };
        if !config.policy.enabled(&container) {
            self.log.debug(
                "container skipped by policy",
                &[
                    Attr::string("id", container_id),
                    Attr::string("mode", config.policy.mode().as_str()),
                ],
            );
            self.metrics.record_container_skipped("policy");
            return Desired::default();
        }
        self.metrics.record_container_scanned();

        let collected =
            self.collect_devices(container_id, pid, &info.mounts, &config.policy, &container);
        for device_error in &collected.device_errs {
            self.log.warn(
                "device rule failed",
                &[
                    Attr::string("id", container_id),
                    Attr::error("err", device_error),
                ],
            );
        }
        self.metrics
            .add_device_files_discovered(collected.granted.len());
        if collected.device_errs.is_empty() {
            return Desired {
                rules: collected.granted.clone(),
                enabled: true,
                collected,
                incomplete: None,
            };
        }
        self.metrics.add_rule_failures(collected.device_errs.len());
        self.log.warn(
            "device set incomplete; no devices are granted until every device resolves",
            &[
                Attr::string("id", container_id),
                Attr::int("errors", count(collected.device_errs.len())),
                Attr::string("reason", "incomplete_device_set"),
            ],
        );
        Desired {
            rules: Vec::new(),
            enabled: true,
            incomplete: Some(Error::Incomplete {
                id: container_id.to_owned(),
                errors: collected.device_errs.clone(),
            }),
            collected,
        }
    }

    /// Opens `/dev` for this pass and collects the container's rules. Failing to open it leaves
    /// every device unresolved.
    fn collect_devices(
        &self,
        container_id: &str,
        pid: i64,
        mounts: &[MountPoint],
        global: &Global,
        container: &Container,
    ) -> ContainerRules {
        if !mounts.iter().any(is_device_mount) {
            return ContainerRules::default();
        }
        let opened = match &self.seams.devfs {
            Some(open) => open(),
            None => RealDevFs::open(DEV_ROOT, &sysfs_root_for(&self.host_root))
                .map(|dev| Box::new(dev) as Box<dyn DevFs>),
        };
        let dev = match opened {
            Ok(dev) => dev,
            Err(open_error) => {
                return ContainerRules {
                    dev_mounts: 1,
                    device_errs: vec![DeviceError::DevOpen(open_error)],
                    ..ContainerRules::default()
                };
            }
        };
        let judge = Judge {
            global,
            container,
            log: &self.log,
        };
        collect_container_rules(&*dev, container_id, pid, mounts, judge, &self.metrics)
    }

    /// Reports what a real run would set. Dry-run stays unprivileged: it reads neither `/proc`
    /// nor the cgroup.
    fn log_dry_run(&self, container_id: &str, rules: &[DeviceRule]) {
        for rule in rules {
            self.log.info(
                "dry-run: would add device rule",
                &[
                    Attr::string("id", container_id),
                    Attr::string("type", rule.kind.as_str()),
                    Attr::int("major", rule.major.unwrap_or_default()),
                    Attr::int("minor", rule.minor.unwrap_or_default()),
                ],
            );
        }
        self.log.info(
            "dry-run: would set device rules",
            &[
                Attr::string("id", container_id),
                Attr::int("rules", count(rules.len())),
            ],
        );
        self.metrics.add_dry_run_skips(rules.len());
    }

    /// Turns a cgroup mutation failure into the container's outcome: `None` when the container
    /// is skipped for good, otherwise an error that names the reason, so the caller retries it.
    fn classify_apply_error(
        &self,
        container_id: &str,
        cgroup_path: &str,
        privileged: bool,
        apply_error: Error,
    ) -> Option<Error> {
        let id = || Attr::string("id", container_id);
        let path = || Attr::string("cgroup", cgroup_path);
        let retryable = |reason, apply_error| Error::Retryable {
            id: container_id.to_owned(),
            reason,
            source: Box::new(apply_error),
        };
        if apply_error.is_cgroup(cgroup::Error::is_filter_missing) && privileged {
            // A privileged container runs without a device filter by design; there is nothing to
            // add grants to and nothing to restrict.
            self.log.info(
                "privileged container has no device filter; nothing to do",
                &[id(), path()],
            );
            self.metrics
                .record_container_skipped("privileged_no_filter");
            return None;
        }
        if apply_error.is_cgroup(cgroup::Error::is_filter_missing) {
            // Most likely wiped by systemd's daemon-reload before this process saw the cgroup,
            // so the runtime's own filter cannot be rebuilt.
            self.log.warn(
                "device filter missing and no cached original; restart the container",
                &[id(), path(), Attr::string("reason", "filter_missing")],
            );
            return Some(retryable("filter_missing", apply_error));
        }
        if apply_error.is_cgroup(cgroup::Error::is_unsupported_attach_mode) {
            // Permanent for this container: the runtime chose the attach mode. Nothing was
            // attached or detached, so it keeps only the runtime's own device filter.
            self.log.error(
                "container skipped: device filter attach mode not supported; the runtime must \
                 attach device filters with BPF_F_ALLOW_MULTI",
                &[id(), path(), Attr::error("err", &apply_error)],
            );
            self.metrics
                .record_container_skipped("unsupported_attach_mode");
            return None;
        }
        if apply_error.is_cgroup(cgroup::Error::is_owned_block_conflict) {
            // Nothing was changed. Only a fresh device filter (a container restart) clears it;
            // retrying keeps the container pending.
            self.log.error(
                "device filter carries an invalid swarm-device-access block; nothing was \
                 changed, restart the container to replace its device filter",
                &[
                    id(),
                    path(),
                    Attr::string("reason", "owned_block_conflict"),
                    Attr::error("err", &apply_error),
                ],
            );
            return Some(retryable("owned_block_conflict", apply_error));
        }
        if apply_error.is_cgroup(cgroup::Error::is_program_not_wrappable) {
            return Some(retryable("program_not_wrappable", apply_error));
        }
        if apply_error.is_cgroup(cgroup::Error::is_filters_inaccessible) {
            return Some(retryable("filters_inaccessible", apply_error));
        }
        Some(apply_error)
    }

    /// Sets `rules` on the cgroup of the container's running process, and returns the cgroup
    /// path once resolved, for error reporting.
    ///
    /// The pid from the inspect can be reused by another process before the cgroup is resolved,
    /// so the process is pinned: a pidfd is opened first, `/proc/<pid>` is read through one
    /// directory descriptor, and immediately before the mutation the pinned process must still
    /// be alive and a second inspect must report the same pid, start time and running state.
    /// While the pinned process is alive its pid cannot belong to anyone else, so the container
    /// still reporting that pid proves the reads were of the container's own process. A grant
    /// additionally requires the pid to be listed in the cgroup's `cgroup.procs`; an empty set is
    /// only ever narrowing.
    fn apply_pinned(
        &self,
        runtime: &Handle,
        context: &CallContext,
        container_id: &str,
        state: &ContainerState,
        privileged: bool,
        rules: &[DeviceRule],
    ) -> (String, Result<(), Error>) {
        let pid = state.pid;
        let pinned = match self.seams.pinner.pin(pid) {
            Ok(pinned) => pinned,
            Err(source) => {
                let id = container_id.to_owned();
                return (String::new(), Err(Error::Pin { id, source }));
            }
        };
        let resolved = match read_proc_cgroup(&self.proc_root, pid) {
            Ok(resolved) => resolved,
            Err(source) => {
                let id = container_id.to_owned();
                return (String::new(), Err(Error::ProcCgroup { id, source }));
            }
        };
        let cgroup_path = host_cgroup_path(
            &self.host_root,
            &resolved.mount_point,
            &resolved.mount_prefix,
            &resolved.root,
        );
        self.log.debug(
            "cgroup path resolved",
            &[
                Attr::int("pid", pid),
                Attr::int("version", version_number(resolved.version)),
                Attr::string("path", cgroup_path.as_str()),
            ],
        );
        let api = self.cgroup_api(resolved.version);
        let handle = match CgroupHandle::open(&cgroup_path) {
            Ok(handle) => handle,
            Err(open_error) => return (cgroup_path, Err(Error::OpenCgroup(open_error))),
        };
        let held = Pinned {
            runtime,
            context,
            container_id,
            state,
            process: &*pinned,
            handle: &handle,
        };
        let verify = |grants| self.verify_pinned(&held, grants);
        if let Err(verify_error) = verify(!rules.is_empty()) {
            return (cgroup_path, Err(verify_error));
        }
        let identity = handle.identity().clone();

        // The same run can move to another cgroup: a container running systemd moves its
        // processes into a child cgroup, and a grant left on the parent keeps applying to every
        // descendant. The grants at the recorded cgroup go before the record is replaced; if
        // they cannot, the record keeps naming that cgroup and the container is retried. On
        // cgroup v1 a child cannot be granted what its parent denies, so such a run ends up with
        // no grants at all: fail closed.
        if let Some(moved) = self.lifecycles.lookup(container_id, &state.started_at)
            && let Some(previous) = moved.cgroup.as_ref()
            && previous.identity != identity
        {
            if let Err(revoke_error) = self.revoke_moved(&moved, previous, &identity) {
                return (cgroup_path, Err(revoke_error));
            }
            // The revoke is real work between the check above and the mutation below, and the
            // run can move again meanwhile: check once more, membership included even for the
            // empty set, so the record never names a cgroup the run was not verified in.
            if let Err(verify_error) = verify(true) {
                return (cgroup_path, Err(verify_error));
            }
        }

        // Recorded after verification, so an identity resolved through a recycled pid is never
        // kept, and before the mutation, so a failed or partial one can still be revoked here
        // later.
        self.lifecycles.verify(
            container_id,
            &state.started_at,
            RunCgroup {
                identity,
                version: resolved.version,
                privileged,
            },
        );
        for rule in rules {
            self.log.debug(
                "setting device rule",
                &[
                    Attr::int("pid", pid),
                    Attr::string("cgroup", cgroup_path.as_str()),
                    Attr::string("type", rule.kind.as_str()),
                    Attr::int("major", rule.major.unwrap_or_default()),
                    Attr::int("minor", rule.minor.unwrap_or_default()),
                ],
            );
        }
        let applied = api
            .set_device_rules(&handle, rules)
            .map_err(|set_error| Error::SetRules(Box::new(set_error)));
        (cgroup_path, applied)
    }

    /// The check made immediately before a mutation; see `apply_pinned`. The re-inspect runs on
    /// the async runtime under the caller's context, from this blocking thread.
    fn verify_pinned(&self, held: &Pinned<'_>, grants: bool) -> Result<(), Error> {
        let Pinned {
            runtime,
            context,
            container_id,
            state,
            process: pinned,
            handle,
        } = *held;
        let id = || container_id.to_owned();
        let again = runtime
            .block_on(self.inspect(context, container_id))
            .map_err(|inspect_error| Error::Verify {
                id: id(),
                cause: VerifyCause::Inspect(Box::new(inspect_error)),
            })?;
        let same = again.state.as_ref().is_some_and(|again| {
            again.running && again.pid == state.pid && again.started_at == state.started_at
        });
        if !same {
            return Err(Error::LifecycleChanged {
                id: id(),
                pid: state.pid,
            });
        }
        pinned.alive().map_err(|source| Error::PinnedGone {
            id: id(),
            pid: state.pid,
            source,
        })?;
        if !grants {
            return Ok(());
        }
        let member = handle
            .has_process(state.pid)
            .map_err(|source| Error::Verify {
                id: id(),
                cause: VerifyCause::Membership(source),
            })?;
        if !member {
            return Err(Error::NotInCgroup {
                id: id(),
                pid: state.pid,
            });
        }
        Ok(())
    }

    /// Handles a container Docker cannot report on. The failure creates or refreshes a
    /// provisional record. Without a pid there is no cgroup to resolve, so every run of the
    /// container with a verified cgroup is revoked there. The container stays pending, unless
    /// Docker reported it does not exist and every revoke succeeded: the error then is Go's
    /// `ErrContainerGone` (the records stay until the sweep verifies their cgroups gone).
    /// Dry-run never mutates, so for it a container Docker does not know is simply gone.
    fn revoke_after_inspect_failure(
        &self,
        container_id: &str,
        dry_run: bool,
        inspect_error: Error,
    ) -> Result<(), Error> {
        let mut errors = Vec::new();
        if !dry_run {
            self.lifecycles.provisional(container_id);
            for record in self.lifecycles.records(container_id) {
                if !record.has_identity() {
                    continue;
                }
                // An ended run is not running, whatever Docker can say.
                let may_run = record.state != LifecycleState::Terminal;
                if let Err(revoke_error) = self.revoke_at(&record, may_run) {
                    errors.push(revoke_error);
                }
            }
        }
        if errors.is_empty() && inspect_error.is_not_found() {
            return Err(Error::ContainerGone(Box::new(inspect_error)));
        }
        let mut joined = vec![inspect_error];
        joined.extend(errors);
        Err(Error::Joined(joined))
    }

    /// Applies the empty set for a run whose process is gone, in the cgroup it was verified in,
    /// if any: nothing can be pinned any more, and the cgroup may already have been removed.
    fn revoke_ended(&self, container_id: &str, started_at: &str) -> Result<(), Error> {
        let Some(mut record) = self
            .lifecycles
            .lookup(container_id, started_at)
            .filter(LifecycleRecord::has_identity)
        else {
            return Ok(());
        };
        self.lifecycles.mark_terminal(container_id, started_at);
        record.state = LifecycleState::Terminal;
        self.revoke_at(&record, false)
    }

    fn terminate_locked(&self, container_id: &str, event_time: Timestamp) -> Result<(), Error> {
        let _locked = self.locked();
        let ended = self.lifecycles.terminate(container_id, event_time);
        if self.store.load().dry_run {
            return Ok(());
        }
        join(
            ended
                .iter()
                .filter(|record| record.has_identity())
                .map(|record| self.revoke_at(record, false).err()),
        )
        .map_or(Ok(()), Err)
    }

    fn sweep_locked(&self) -> usize {
        let _locked = self.locked();
        let now = self.lifecycles.now();
        let dry_run = self.store.load().dry_run;
        let mut released = 0;
        for record in self.lifecycles.all() {
            let Some(cgroup) = record.cgroup.as_ref() else {
                let last_seen = record.first_seen.max(record.observed);
                let stale = SignedDuration::try_from(PROVISIONAL_TTL)
                    .is_ok_and(|ttl| now.duration_since(last_seen) > ttl);
                if record.state == LifecycleState::Terminal || stale {
                    self.lifecycles.release(&record);
                    released += 1;
                }
                continue;
            };
            match cgroup_gone(&cgroup.identity) {
                Err(stat_error) => self.log.warn(
                    "could not check container cgroup",
                    &[
                        Attr::string("id", record.container_id.as_str()),
                        Attr::error("err", &stat_error),
                    ],
                ),
                Ok(true) => {
                    self.release(&record, "cgroup gone");
                    released += 1;
                }
                Ok(false)
                    if record.state == LifecycleState::Terminal && !record.revoked && !dry_run =>
                {
                    if let Err(revoke_error) = self.revoke_at(&record, false) {
                        self.log.warn(
                            "could not revoke ended container",
                            &[
                                Attr::string("id", record.container_id.as_str()),
                                Attr::error("err", &revoke_error),
                            ],
                        );
                    }
                }
                Ok(false) => {}
            }
        }
        released
    }

    /// Reopens the recorded cgroup and applies the empty set only if it is still the same
    /// directory (same inode). A cgroup that is gone, or was recreated at the same path, holds
    /// none of the run's grants: the record is released and nothing is mutated. Otherwise the
    /// record is kept even after the revoke, until the sweep verifies the cgroup gone. `may_run`
    /// says the container may still be running (Docker could not say).
    fn revoke_at(&self, record: &LifecycleRecord, may_run: bool) -> Result<(), Error> {
        let Some(cgroup) = record.cgroup.as_ref() else {
            return Ok(());
        };
        let handle = match self.open_recorded(record, &cgroup.identity)? {
            Recorded::Open(handle) => handle,
            Recorded::Gone(why) => {
                self.release(record, why);
                return Ok(());
            }
        };
        let api = self.cgroup_api(cgroup.version);
        match api.set_device_rules(&handle, &[]) {
            Err(set_error) if set_error.is_filter_missing() && may_run && !cgroup.privileged => {
                // Nothing attached holds a grant, but a container that may still be running is
                // unfiltered. The container stays pending either way.
                self.log.warn(
                    "device filter missing and no cached original; restart the container",
                    &[
                        Attr::string("id", record.container_id.as_str()),
                        Attr::string("cgroup", cgroup.identity.path.as_str()),
                        Attr::string("reason", "filter_missing"),
                    ],
                );
            }
            // An exited or privileged container has nothing attached, so nothing to revoke.
            Err(set_error) if set_error.is_filter_missing() => {}
            Err(set_error) => {
                return Err(Error::Revoke {
                    id: record.container_id.clone(),
                    path: cgroup.identity.path.clone(),
                    source: Box::new(set_error),
                });
            }
            Ok(()) => {}
        }
        self.lifecycles
            .mark_revoked(&record.container_id, &record.started_at);
        self.log.debug(
            "device grants revoked",
            &[
                Attr::string("id", record.container_id.as_str()),
                Attr::string("cgroup", cgroup.identity.path.as_str()),
            ],
        );
        Ok(())
    }

    /// Applies the empty set at the cgroup a still-running run was recorded in, once the run is
    /// verified in another one (`current`). The cgroup API then forgets that directory: no record
    /// names it any more. A missing filter means nothing attached there holds a grant.
    fn revoke_moved(
        &self,
        record: &LifecycleRecord,
        previous: &RunCgroup,
        current: &Identity,
    ) -> Result<(), Error> {
        if let Recorded::Open(handle) = self.open_recorded(record, &previous.identity)? {
            let api = self.cgroup_api(previous.version);
            match api.set_device_rules(&handle, &[]) {
                Err(set_error) if !set_error.is_filter_missing() => {
                    return Err(Error::RevokeMoved {
                        id: record.container_id.clone(),
                        path: previous.identity.path.clone(),
                        source: Box::new(set_error),
                    });
                }
                Err(_) | Ok(()) => {}
            }
        }
        self.log.info(
            "container moved to another cgroup; previous grants revoked",
            &[
                Attr::string("id", record.container_id.as_str()),
                Attr::string("from", previous.identity.path.as_str()),
                Attr::string("to", current.path.as_str()),
            ],
        );
        self.ledger.forget(&previous.identity);
        self.filter_cache.forget(&previous.identity);
        Ok(())
    }

    /// Reopens the cgroup a record names: [`Recorded::Gone`] when the directory is gone or was
    /// recreated at the same path (another inode); either way it holds none of the run's grants.
    fn open_recorded(
        &self,
        record: &LifecycleRecord,
        identity: &Identity,
    ) -> Result<Recorded, Error> {
        let _ = self;
        match CgroupHandle::open(&identity.path) {
            Ok(handle) if handle.identity() == identity => Ok(Recorded::Open(handle)),
            Ok(_) => Ok(Recorded::Gone("cgroup recreated")),
            Err(HandleError::Open { source, .. } | HandleError::Stat { source, .. })
                if source.kind() == std::io::ErrorKind::NotFound =>
            {
                Ok(Recorded::Gone("cgroup removed"))
            }
            Err(source) => Err(Error::RevokeOpen {
                id: record.container_id.clone(),
                source,
            }),
        }
    }

    /// Forgets a run whose cgroup is verified gone, together with what the cgroup API remembered
    /// for that directory (the v1 ledger and the cached v2 filters).
    fn release(&self, record: &LifecycleRecord, why: &str) {
        let path = record.identity().map(|identity| identity.path.as_str());
        self.log.debug(
            "container cgroup gone; released",
            &[
                Attr::string("id", record.container_id.as_str()),
                Attr::string("cgroup", path.unwrap_or_default()),
                Attr::string("why", why),
            ],
        );
        self.lifecycles.release(record);
        if let Some(identity) = record.identity() {
            self.ledger.forget(identity);
            self.filter_cache.forget(identity);
        }
    }

    fn cgroup_api(&self, version: CgroupVersion) -> Box<dyn Interface> {
        (self.seams.new_cgroup)(
            version,
            Arc::clone(&self.ledger),
            Arc::clone(&self.filter_cache),
            self.log.clone(),
        )
    }
}

/// Whether the directory `identity` names no longer exists or was recreated (another inode).
fn cgroup_gone(identity: &Identity) -> Result<bool, StatError> {
    match rustix::fs::stat(identity.path.as_str()) {
        Ok(stat) => Ok(stat.st_ino != identity.inode),
        Err(Errno::NOENT) => Ok(true),
        Err(errno) => Err(StatError {
            path: identity.path.clone(),
            errno,
        }),
    }
}

/// `stat cgroup "<path>": <errno>`.
#[derive(Debug)]
struct StatError {
    path: String,
    errno: Errno,
}

impl std::fmt::Display for StatError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "stat cgroup {}: {}",
            quote(&self.path),
            SysError::new("", self.errno)
        )
    }
}

/// The version as Go's `int` logs it.
const fn version_number(version: CgroupVersion) -> i64 {
    match version {
        CgroupVersion::V1 => 1,
        CgroupVersion::V2 => 2,
    }
}

/// A count as a log attribute.
fn count(value: usize) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

#[cfg(test)]
mod tests;
