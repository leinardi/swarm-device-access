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

//! Go's `processor_test.go`, `reconcile_test.go`, `applyerr_test.go`, `lifecycle_test.go` and
//! `serialize_test.go`, and the reconcile halves of the `computeDesired` tests of
//! `rules_test.go`, against a scripted inspector, pinner and cgroup controller, a fake `/proc`
//! and a fake host cgroup directory.
//!
//! The fakes' own errors are this port's (a Docker error for the inspector, an errno for the
//! pinner, a `cgroup::Error` variant for the controller), where Go's fakes returned arbitrary
//! errors: each test checks the same classification Go's `errors.Is` checked. Go's
//! `ServiceInspect` counter has no counterpart: the inspector trait has no such call to count.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak, mpsc};
use std::time::Duration;

use hyper::StatusCode;
use jiff::{SignedDuration, Timestamp};

use super::*;
use crate::cgroup::{Conflict, GateError, OwnedError};
use crate::clock::FakeClock;
use crate::config::new_store;
use crate::docker::types::{ContainerConfig, HostConfig};
use crate::logger::{Level, MemorySink};
use crate::policy::{LABEL_ENABLE, LABEL_PREFIX, Mode};
use crate::processor::devfs::NodeKind;
use crate::processor::fake_devfs::{DEV_CARD, DEV_NULL, DEV_SDA, FakeDevFs, FakeNode};
use crate::processor::pin::PinError;
use crate::test_support::{TempDir, capture_logs};

const RECONCILE_PID: i64 = 80;
const RECONCILE_STARTED: &str = "2026-09-27T10:00:00.000000000Z";
const RESTARTED_AT: &str = "2026-09-27T10:05:00.000000000Z";

/// A cgroup v2 container, the only layout the processor tests need.
const TEST_CGROUP_CONTENT: &str = "0::/docker/testcontainer\n";
const TEST_MOUNTINFO_CONTENT: &str =
    "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n";

type Reply = Result<ContainerInspect, docker::Error>;

/// Go's `seqInspector`: answers the n-th inspect with the n-th reply, repeating the last one
/// once they run out.
#[derive(Default)]
struct InspectorState {
    replies: Mutex<Vec<Reply>>,
    calls: AtomicUsize,
}

impl InspectorState {
    /// Makes every later inspect answer with `replies`, in order.
    fn then(&self, replies: Vec<Reply>) {
        *self.replies.lock().unwrap() = replies;
        self.calls.store(0, Ordering::SeqCst);
    }

    fn next(&self) -> Reply {
        let replies = self.replies.lock().unwrap();
        let call = self.calls.fetch_add(1, Ordering::SeqCst);
        replies[call.min(replies.len() - 1)].clone()
    }
}

#[derive(Clone)]
struct FakeInspector(Arc<InspectorState>);

impl Inspector for FakeInspector {
    fn container_inspect(&self, _id: &str) -> impl Future<Output = Reply> + Send {
        let reply = self.0.next();
        async move { reply }
    }
}

/// Go's `failingCgroup`: records every set and returns the injected error.
#[derive(Default)]
struct FakeCgroup {
    calls: Mutex<Vec<(Identity, Vec<DeviceRule>)>>,
    err: Mutex<Option<fn() -> cgroup::Error>>,
    /// Runs inside every call, after it is recorded.
    on_set: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
}

impl FakeCgroup {
    fn calls(&self) -> usize {
        self.calls.lock().unwrap().len()
    }

    fn rules(&self, call: usize) -> Vec<DeviceRule> {
        self.calls.lock().unwrap()[call].1.clone()
    }

    fn identities(&self) -> Vec<Identity> {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .map(|(identity, _)| identity.clone())
            .collect()
    }

    /// The identity of the last call.
    fn identity(&self) -> Identity {
        self.identities().last().cloned().unwrap()
    }

    fn fail_with(&self, error: fn() -> cgroup::Error) {
        *self.err.lock().unwrap() = Some(error);
    }
}

struct FakeApi(Arc<FakeCgroup>);

impl Interface for FakeApi {
    fn set_device_rules(
        &self,
        handle: &CgroupHandle,
        rules: &[DeviceRule],
    ) -> Result<(), cgroup::Error> {
        self.0
            .calls
            .lock()
            .unwrap()
            .push((handle.identity().clone(), rules.to_vec()));
        let on_set = self.0.on_set.lock().unwrap().clone();
        if let Some(on_set) = on_set {
            on_set();
        }
        (*self.0.err.lock().unwrap()).map_or(Ok(()), |error| Err(error()))
    }
}

/// Go's `fakePinner`: every pinned process is alive unless `gone` is set. It also counts the
/// pins still held, for the transaction's ownership of its descriptors.
#[derive(Default)]
struct PinnerState {
    pins: Mutex<Vec<i64>>,
    gone: AtomicBool,
    held: Arc<AtomicUsize>,
}

struct FakePinner(Arc<PinnerState>);

impl ProcessPinner for FakePinner {
    fn pin(&self, pid: i64) -> Result<Box<dyn PinnedProcess>, PinError> {
        self.0.pins.lock().unwrap().push(pid);
        self.0.held.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(FakePinned {
            gone: self.0.gone.load(Ordering::SeqCst),
            held: Arc::clone(&self.0.held),
        }))
    }
}

struct FakePinned {
    gone: bool,
    held: Arc<AtomicUsize>,
}

impl PinnedProcess for FakePinned {
    fn alive(&self) -> Result<(), PinError> {
        if self.gone {
            return Err(PinError::Gone(Errno::SRCH));
        }
        Ok(())
    }
}

impl Drop for FakePinned {
    fn drop(&mut self) {
        self.held.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Go's `buildProcRoot`: a `/proc/<pid>/{cgroup,mountinfo}` under a temporary directory.
fn build_proc_root(pid: i64) -> TempDir {
    let root = TempDir::new();
    let dir = root.path().join(format!("proc/{pid}"));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("cgroup"), TEST_CGROUP_CONTENT).unwrap();
    std::fs::write(dir.join("mountinfo"), TEST_MOUNTINFO_CONTENT).unwrap();
    root
}

/// Go's `testCgroupDir`: where `build_proc_root`'s `/proc` places the container, below a host
/// root.
fn test_cgroup_dir(host_root: &Path) -> PathBuf {
    host_root.join("sys/fs/cgroup/docker/testcontainer")
}

/// Go's `writeProcs`: replaces the test cgroup's `cgroup.procs` with `pids`.
fn write_procs(host_root: &Path, pids: &[i64]) {
    let content = pids.iter().fold(String::new(), |mut content, pid| {
        let _ = writeln!(content, "{pid}");
        content
    });
    std::fs::write(test_cgroup_dir(host_root).join("cgroup.procs"), content).unwrap();
}

/// Go's `hostRootWithCgroup`: a host root holding the cgroup directory `build_proc_root`'s
/// `/proc` resolves to, with `pid` listed in its `cgroup.procs`.
fn host_root_with_cgroup(pid: i64) -> TempDir {
    let root = TempDir::new();
    std::fs::create_dir_all(test_cgroup_dir(root.path())).unwrap();
    write_procs(root.path(), &[pid]);
    root
}

fn bind(source: &str, destination: &str) -> MountPoint {
    MountPoint {
        kind: "bind".to_owned(),
        source: source.to_owned(),
        destination: destination.to_owned(),
    }
}

fn running(pid: i64, started_at: &str, mounts: Vec<MountPoint>) -> ContainerInspect {
    ContainerInspect {
        state: Some(ContainerState {
            running: true,
            pid,
            started_at: started_at.to_owned(),
        }),
        mounts,
        ..ContainerInspect::default()
    }
}

fn running_dev_null(started_at: &str) -> ContainerInspect {
    running(
        RECONCILE_PID,
        started_at,
        vec![bind("/dev/null", "/dev/null")],
    )
}

fn exited() -> ContainerInspect {
    ContainerInspect {
        state: Some(ContainerState {
            running: false,
            pid: 0,
            started_at: RECONCILE_STARTED.to_owned(),
        }),
        ..ContainerInspect::default()
    }
}

fn labelled(mut info: ContainerInspect, labels: &[(&str, &str)]) -> ContainerInspect {
    info.config = Some(ContainerConfig {
        labels: labels
            .iter()
            .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
            .collect::<BTreeMap<_, _>>(),
    });
    info
}

fn privileged(mut info: ContainerInspect) -> ContainerInspect {
    info.host_config = Some(HostConfig { privileged: true });
    info
}

fn daemon_unavailable() -> Reply {
    Err(docker::Error::connection_failed(
        "daemon unavailable".to_owned(),
    ))
}

fn not_found() -> Reply {
    Err(docker::Error::response(
        StatusCode::NOT_FOUND,
        "Error response from daemon: No such container: abc".to_owned(),
    ))
}

fn runtime(mode: &str, dry_run: bool) -> Runtime {
    Runtime {
        dry_run,
        policy: Global::new(mode, None, None).unwrap(),
    }
}

/// Go's `reconcileEnv`, with the directories, clock and log it needs kept alive.
struct Env {
    proc: Arc<Processor<FakeInspector>>,
    inspector: Arc<InspectorState>,
    fake: Arc<FakeCgroup>,
    pinner: Arc<PinnerState>,
    apis: Arc<AtomicUsize>,
    host: TempDir,
    proc_root: TempDir,
    clock: Arc<FakeClock>,
    sink: MemorySink,
}

/// How an [`Env`] is built.
struct Options {
    runtime: Runtime,
    reply: Reply,
    /// Replaces the fake host root (Go's `HostRoot: "/host"`).
    host_root: Option<String>,
    /// Replaces the fake `/proc` root.
    proc_root: Option<String>,
    /// Go's `pinner: nil`: the real pidfd pinner.
    real_pinner: bool,
    after_compute: Option<Box<dyn Fn() + Send + Sync>>,
}

impl Options {
    fn new(mode: &str, dry_run: bool) -> Self {
        Self {
            runtime: runtime(mode, dry_run),
            reply: Ok(running_dev_null(RECONCILE_STARTED)),
            host_root: None,
            proc_root: None,
            real_pinner: false,
            after_compute: None,
        }
    }
}

impl Env {
    /// Go's `newReconcileEnv`.
    fn new(mode: &str, dry_run: bool) -> Self {
        Self::with(Options::new(mode, dry_run))
    }

    fn with(options: Options) -> Self {
        let inspector = Arc::new(InspectorState::default());
        inspector.then(vec![options.reply]);
        let fake = Arc::new(FakeCgroup::default());
        let pinner = Arc::new(PinnerState::default());
        let apis = Arc::new(AtomicUsize::new(0));
        let host = host_root_with_cgroup(RECONCILE_PID);
        let proc_root = build_proc_root(RECONCILE_PID);
        let clock = Arc::new(FakeClock::new(
            "2026-09-27T12:00:00Z".parse::<Timestamp>().unwrap(),
        ));
        let (log, sink) = capture_logs(Level::DEBUG);
        let (store, publisher) = new_store(options.runtime);
        let settings = Settings {
            inspector: FakeInspector(Arc::clone(&inspector)),
            store,
            publisher,
            metrics: Recorder::noop(),
            host_root: options
                .host_root
                .unwrap_or_else(|| host.path().display().to_string()),
            proc_root: options
                .proc_root
                .unwrap_or_else(|| proc_root.path().display().to_string()),
            call_timeout: None,
            clock: Arc::clone(&clock) as Arc<dyn Clock>,
            log,
        };
        let new_cgroup = {
            let fake = Arc::clone(&fake);
            let apis = Arc::clone(&apis);
            move |_: CgroupVersion, _: Arc<Ledger>, _: Arc<FilterCache>, _: LogHandle| {
                apis.fetch_add(1, Ordering::SeqCst);
                Box::new(FakeApi(Arc::clone(&fake))) as Box<dyn Interface>
            }
        };
        let pinner_seam: Box<dyn ProcessPinner> = if options.real_pinner {
            Box::new(PidfdPinner)
        } else {
            Box::new(FakePinner(Arc::clone(&pinner)))
        };
        let seams = Seams {
            pinner: pinner_seam,
            new_cgroup: Box::new(new_cgroup),
            devfs: None,
            after_compute: options.after_compute,
        };
        Self {
            proc: Arc::new(Processor::with_seams(settings, seams)),
            inspector,
            fake,
            pinner,
            apis,
            host,
            proc_root,
            clock,
            sink,
        }
    }

    async fn reconcile(&self) -> Result<(), Error> {
        self.proc.reconcile(&CallContext::default(), "abc").await
    }

    fn publish(&self, runtime: Runtime) {
        self.proc.publisher.publish(runtime);
    }

    /// Go's `grantOnce`: one successful reconcile that grants `/dev/null`, leaving the
    /// lifecycle's cgroup known.
    async fn grant_once(&self) {
        self.reconcile().await.unwrap();
        assert_eq!(self.fake.calls(), 1, "grant: want one set");
        assert_eq!(self.fake.rules(0).len(), 1, "grant: want /dev/null");
        assert_eq!(
            verified_records(&self.proc).len(),
            1,
            "want the lifecycle recorded"
        );
    }

    fn cgroup_dir(&self) -> PathBuf {
        test_cgroup_dir(self.host.path())
    }

    fn write_proc_cgroup(&self, content: &str) {
        let path = self
            .proc_root
            .path()
            .join(format!("proc/{RECONCILE_PID}/cgroup"));
        std::fs::write(path, content).unwrap();
    }

    fn log(&self) -> String {
        self.sink.contents()
    }
}

/// Go's `verifiedRecords`: the lifecycle records that name a cgroup.
fn verified_records<I: Inspector>(proc: &Processor<I>) -> Vec<LifecycleRecord> {
    proc.lifecycles()
        .all()
        .into_iter()
        .filter(LifecycleRecord::has_identity)
        .collect()
}

/// Recreates the test cgroup at the same path. The replacement is built before the original is
/// removed, so the new directory cannot reuse the old inode number.
fn recreate(dir: &Path) {
    let replacement = dir.with_extension("new");
    std::fs::create_dir(&replacement).unwrap();
    std::fs::remove_dir_all(dir).unwrap();
    std::fs::rename(&replacement, dir).unwrap();
}

fn started(value: &str) -> Timestamp {
    value.parse().unwrap()
}

// ---- processor_test.go ----

/// Go: `TestHostCGroupPath`.
#[test]
fn host_cgroup_path_joins_the_host_view() {
    for (name, mount_point, prefix, root, want) in [
        (
            "host cgroup namespace",
            "/sys/fs/cgroup",
            "/",
            "/system.slice/docker-abc.scope",
            "/host/sys/fs/cgroup/system.slice/docker-abc.scope",
        ),
        (
            "private cgroup namespace",
            "/sys/fs/cgroup",
            "/system.slice/docker-abc.scope",
            "/",
            "/host/sys/fs/cgroup/system.slice/docker-abc.scope",
        ),
        (
            "mount prefix trimmed from proc cgroup",
            "/sys/fs/cgroup",
            "/docker",
            "/abc",
            "/host/sys/fs/cgroup/docker/abc",
        ),
    ] {
        assert_eq!(
            host_cgroup_path("/host", mount_point, prefix, root),
            want,
            "{name}"
        );
    }
}

/// Go: `TestReconcile_InspectError`.
#[tokio::test]
async fn reconcile_inspect_error() {
    let env = Env::new("all", false);
    env.inspector.then(vec![daemon_unavailable()]);
    assert!(env.reconcile().await.is_err());
}

/// Go: `TestReconcile_NilState`.
#[tokio::test]
async fn reconcile_nil_state() {
    let env = Env::new("all", false);
    env.inspector.then(vec![Ok(ContainerInspect::default())]);
    env.reconcile().await.unwrap();
}

/// Go: `TestReconcile_ZeroPid`.
#[tokio::test]
async fn reconcile_zero_pid() {
    let env = Env::new("all", false);
    env.inspector.then(vec![Ok(running(0, "", Vec::new()))]);
    env.reconcile().await.unwrap();
}

/// Go: `TestReconcile_NoDevMounts`.
#[tokio::test]
async fn reconcile_no_dev_mounts() {
    let env = Env::new("all", false);
    env.inspector.then(vec![Ok(running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![bind("/tmp/data", "/data"), bind("/var/log", "/logs")],
    ))]);

    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1);
    assert!(env.fake.rules(0).is_empty(), "want the empty set");
    assert!(
        !env.log().contains("container processed"),
        "no summary for a container without /dev mounts:\n{}",
        env.log()
    );
}

/// Go: `TestReconcile_DevMountFilterApplied`.
#[tokio::test]
async fn reconcile_dev_mount_without_cgroup_fails() {
    let mut options = Options::new("all", false);
    options.host_root = Some("/host".to_owned());
    let env = Env::with(options);
    assert!(
        env.reconcile().await.is_err(),
        "the cgroup path cannot be opened"
    );
}

/// Go: `TestReconcile_DevMount_DryRunNoError`.
#[tokio::test]
async fn reconcile_dev_mount_dry_run_no_error() {
    let env = Env::new("all", true);
    env.reconcile().await.unwrap();
}

/// Go: `TestReconcile_DeduplicatesDuplicateMounts`.
#[tokio::test]
async fn reconcile_deduplicates_duplicate_mounts() {
    let env = Env::new("all", true);
    env.inspector.then(vec![Ok(running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![
            bind("/dev/null", "/dev/null"),
            bind("/dev/null", "/dev/null2"),
        ],
    ))]);
    env.reconcile().await.unwrap();
}

/// Go: `TestReconcile_OptInProcessesEnabled`.
#[tokio::test]
async fn reconcile_opt_in_processes_enabled() {
    let env = Env::new("opt-in", true);
    let info = running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![bind("/dev/null", "/dev/null")],
    );
    env.inspector
        .then(vec![Ok(labelled(info, &[(LABEL_ENABLE, "true")]))]);
    env.reconcile().await.unwrap();
}

/// Go: `TestReconcile_DevDirectorySummary`. A whole-`/dev` mount is processed without a
/// container-level error and emits one INFO summary.
#[tokio::test]
async fn reconcile_dev_directory_summary() {
    let mut options = Options::new("all", true);
    options.runtime.policy = Global::new("all", Some(vec!["/dev/null".to_owned()]), None).unwrap();
    options.reply = Ok(running(46, "", vec![bind("/dev", "/dev")]));
    let env = Env::with(options);

    env.reconcile().await.unwrap();
    let log = env.log();
    let summaries: Vec<&str> = log
        .lines()
        .filter(|line| line.contains("container processed"))
        .collect();
    assert_eq!(summaries.len(), 1, "want one summary:\n{log}");
    for want in [
        "level=INFO",
        "devices_granted=1",
        "errors=0",
        "dry_run=true",
    ] {
        assert!(summaries[0].contains(want), "summary {}", summaries[0]);
    }
}

/// Go: `TestReconcile_IgnoresServiceLevelLabels`. Policy comes from the container's own labels
/// only: a container whose only opt-in is a service-level label is skipped.
#[tokio::test]
async fn reconcile_ignores_service_level_labels() {
    let env = Env::new("opt-in", true);
    let info = running(51, "", Vec::new());
    env.inspector.then(vec![Ok(labelled(
        info,
        &[("com.docker.swarm.service.id", "svc456")],
    ))]);
    env.reconcile().await.unwrap();
    assert!(
        env.log().contains("skipped by policy"),
        "a service-level opt-in must not enable the container:\n{}",
        env.log()
    );
}

/// Go: `TestReconcile_WarnsOnUnknownContainerLabel`.
#[tokio::test]
async fn reconcile_warns_on_unknown_container_label() {
    let env = Env::new("opt-in", true);
    let key = format!("{LABEL_PREFIX}enabled");
    let info = running(51, "", Vec::new());
    env.inspector
        .then(vec![Ok(labelled(info, &[(key.as_str(), "true")]))]);
    env.reconcile().await.unwrap();
    assert!(
        env.log()
            .contains("level=WARN unrecognized swarm-device-access label on container id=abc label=swarm-device-access.enabled"),
        "{}",
        env.log()
    );
}

// ---- reconcile_test.go ----

/// Go: `TestReconcile_PidRecycledBeforePinDoesNotMutate`. The container exits right after the
/// first inspect and its pid goes to an unrelated process in another cgroup. The pin and the
/// `/proc` reads then describe that process, but the second inspect no longer reports the pid,
/// so nothing is mutated and the foreign cgroup is not recorded.
#[tokio::test]
async fn reconcile_pid_recycled_before_pin_does_not_mutate() {
    let env = Env::new("all", false);
    let other = env.host.path().join("sys/fs/cgroup/system.slice/other");
    std::fs::create_dir_all(&other).unwrap();
    std::fs::write(other.join("cgroup.procs"), format!("{RECONCILE_PID}\n")).unwrap();
    env.write_proc_cgroup("0::/system.slice/other\n");
    env.inspector
        .then(vec![Ok(running_dev_null(RECONCILE_STARTED)), Ok(exited())]);

    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_lifecycle_changed(), "{error}");
    assert_eq!(env.fake.calls(), 0);
    assert!(verified_records(&env.proc).is_empty());
}

/// Go: `TestReconcile_StartedAtChangedDoesNotMutate`.
#[tokio::test]
async fn reconcile_started_at_changed_does_not_mutate() {
    let env = Env::new("all", false);
    env.inspector.then(vec![
        Ok(running_dev_null(RECONCILE_STARTED)),
        Ok(running_dev_null("2026-09-27T10:00:05.000000000Z")),
    ]);
    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_lifecycle_changed(), "{error}");
    assert_eq!(env.fake.calls(), 0);
}

/// Go: `TestReconcile_PinnedProcessGoneDoesNotMutate`.
#[tokio::test]
async fn reconcile_pinned_process_gone_does_not_mutate() {
    let env = Env::new("all", false);
    env.pinner.gone.store(true, Ordering::SeqCst);
    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_pinned_process_gone(), "{error}");
    assert_eq!(env.fake.calls(), 0);
}

/// Go: `TestReconcile_MembershipGatesGrantsOnly`. A pid missing from `cgroup.procs` blocks a
/// grant, but not the empty set, which can only narrow.
#[tokio::test]
async fn reconcile_membership_gates_grants_only() {
    let env = Env::new("all", false);
    write_procs(env.host.path(), &[]);
    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_not_in_cgroup(), "{error}");
    assert_eq!(env.fake.calls(), 0);

    env.publish(runtime("opt-in", false));
    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1);
    assert!(env.fake.rules(0).is_empty());
}

/// Go: `TestReconcile_RevokeAfterExitWithMatchingInode`. Once the container has exited nothing
/// can be pinned; the empty set goes to the cgroup the lifecycle was verified in, still the same
/// directory, and the entry is kept until the cgroup is verified gone.
#[tokio::test]
async fn reconcile_revoke_after_exit_with_matching_inode() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let granted = env.fake.identity();
    write_procs(env.host.path(), &[]);
    env.inspector.then(vec![Ok(exited())]);

    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 2);
    assert!(env.fake.rules(1).is_empty());
    assert_eq!(env.fake.identity(), granted);
    assert_eq!(
        env.pinner.pins.lock().unwrap().len(),
        1,
        "only the grant pins"
    );
    assert_eq!(
        verified_records(&env.proc).len(),
        1,
        "the entry is kept until the cgroup is gone"
    );
}

/// Go: `TestReconcile_InodeMismatchReleasesWithoutMutation`. The cgroup was recreated at the
/// same path, so it holds none of the lifecycle's grants.
#[tokio::test]
async fn reconcile_inode_mismatch_releases_without_mutation() {
    let env = Env::new("all", false);
    env.grant_once().await;
    recreate(&env.cgroup_dir());
    env.inspector.then(vec![Ok(exited())]);

    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1, "only the grant");
    assert!(verified_records(&env.proc).is_empty(), "entry released");
}

/// Go: `TestReconcile_ExitAfterCgroupRemovedReleases`.
#[tokio::test]
async fn reconcile_exit_after_cgroup_removed_releases() {
    let env = Env::new("all", false);
    env.grant_once().await;
    std::fs::remove_dir_all(env.cgroup_dir()).unwrap();
    env.inspector.then(vec![Ok(exited())]);

    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1);
    assert!(verified_records(&env.proc).is_empty());
}

/// Go's `moveToChildCgroup`: moves the test container's process into a child of its cgroup, as
/// systemd running inside it does, and returns the child's path.
fn move_to_child_cgroup(env: &Env) -> PathBuf {
    let child = env.cgroup_dir().join("init.scope");
    std::fs::create_dir(&child).unwrap();
    std::fs::write(child.join("cgroup.procs"), format!("{RECONCILE_PID}\n")).unwrap();
    write_procs(env.host.path(), &[]);
    env.write_proc_cgroup("0::/docker/testcontainer/init.scope\n");
    child
}

/// Go: `TestReconcile_MovedRunRevokesPreviousCgroup`. A run found in another cgroup than the one
/// it was granted in loses the grants there before it is granted in the new one, since a grant on
/// a parent cgroup applies to every descendant.
#[tokio::test]
async fn reconcile_moved_run_revokes_previous_cgroup() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let parent = env.fake.identity();
    let child = move_to_child_cgroup(&env);

    env.reconcile().await.unwrap();
    let identities = env.fake.identities();
    assert_eq!(env.fake.calls(), 3);
    assert_eq!(identities[1], parent);
    assert!(env.fake.rules(1).is_empty());
    assert_eq!(identities[2].path, child.display().to_string());
    assert_eq!(env.fake.rules(2).len(), 1);
    let records = verified_records(&env.proc);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].identity().unwrap().path,
        child.display().to_string()
    );
    assert!(
        env.log().contains(
            "level=INFO container moved to another cgroup; previous grants revoked id=abc"
        ),
        "{}",
        env.log()
    );
}

/// Go: `TestReconcile_MovedAgainDuringRevokeIsRechecked`. The run leaves the new cgroup while the
/// previous one is revoked, so the new one is neither granted nor recorded, and the container is
/// retried.
#[tokio::test]
async fn reconcile_moved_again_during_revoke_is_rechecked() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let parent = env.fake.identity();
    let child = move_to_child_cgroup(&env);
    let procs = child.join("cgroup.procs");
    *env.fake.on_set.lock().unwrap() = Some(Arc::new(move || {
        std::fs::write(&procs, "").unwrap();
    }));

    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_not_in_cgroup(), "{error}");
    assert_eq!(env.fake.calls(), 2, "only the revoke");
    assert_eq!(env.fake.identities()[1], parent);
    assert!(env.fake.rules(1).is_empty());
    let records = verified_records(&env.proc);
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].identity(),
        Some(&parent),
        "the parent's record kept"
    );
}

/// Go: `TestReconcile_MovedRunRevokeFailureKeepsRecord`. When the previous cgroup cannot be
/// revoked, the new one is not granted and the record keeps naming the previous cgroup, so a
/// retry revokes it again.
#[tokio::test]
async fn reconcile_moved_run_revoke_failure_keeps_record() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let parent = env.fake.identity();
    move_to_child_cgroup(&env);
    env.fake
        .fail_with(|| cgroup::Error::Drift(vec!["revoke failed".to_owned()]));

    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_cgroup(cgroup::Error::is_drift), "{error}");
    assert_eq!(env.fake.calls(), 2, "only the revoke attempt");
    assert_eq!(env.fake.identities()[1], parent);
    let records = verified_records(&env.proc);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].identity(), Some(&parent));
}

/// Go: `TestReconcile_InspectFailureRevokesKnownIdentity`. Without a pid the container's grants
/// are revoked where its lifecycle was verified, and the container stays pending. With no known
/// identity it is pending only.
#[tokio::test]
async fn reconcile_inspect_failure_revokes_known_identity() {
    let env = Env::new("all", false);
    env.inspector.then(vec![daemon_unavailable()]);
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.is_docker(docker::ErrorKind::ConnectionFailed),
        "{error}"
    );
    assert_eq!(env.fake.calls(), 0);

    env.inspector
        .then(vec![Ok(running_dev_null(RECONCILE_STARTED))]);
    env.grant_once().await;
    env.inspector.then(vec![daemon_unavailable()]);
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.is_docker(docker::ErrorKind::ConnectionFailed) && !error.is_container_gone(),
        "{error}"
    );
    assert_eq!(env.fake.calls(), 2);
    assert!(env.fake.rules(1).is_empty(), "the empty set applied");
    assert_eq!(
        verified_records(&env.proc).len(),
        1,
        "not-found alone never releases"
    );
}

/// Go: `TestReconcile_DisabledContainerSetsEmptySet`.
#[tokio::test]
async fn reconcile_disabled_container_sets_empty_set() {
    let env = Env::new("opt-in", false);
    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1);
    assert!(env.fake.rules(0).is_empty());
    assert!(
        !env.log().contains("container processed"),
        "a container skipped by policy must not also be reported processed:\n{}",
        env.log()
    );
}

/// Go: `TestReconcile_InvalidLabelsSetEmptySet`.
#[tokio::test]
async fn reconcile_invalid_labels_set_empty_set() {
    let env = Env::new("all", false);
    let info = running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![bind("/dev/null", "/dev/null")],
    );
    env.inspector
        .then(vec![Ok(labelled(info, &[(LABEL_ENABLE, "maybe")]))]);
    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1);
    assert!(env.fake.rules(0).is_empty());
    let log = env.log();
    let line = log
        .lines()
        .find(|line| line.contains("container skipped: invalid policy labels"))
        .unwrap_or_default();
    assert_eq!(
        line,
        "level=WARN container skipped: invalid policy labels id=abc err=invalid value for label \
         \"swarm-device-access.enable\": \"maybe\" is not a boolean"
    );
}

/// Go: `TestReconcile_IncompleteDeviceSetSetsEmptySet`. One unresolved device empties the whole
/// set; the container is retried.
#[tokio::test]
async fn reconcile_incomplete_device_set_sets_empty_set() {
    let env = Env::new("all", false);
    env.inspector.then(vec![Ok(running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![
            bind("/dev/null", "/dev/null"),
            bind("/dev/sda-does-not-exist", "/dev/x"),
        ],
    ))]);
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.to_string().contains("incomplete_device_set"),
        "{error}"
    );
    assert_eq!(env.fake.calls(), 1);
    assert!(env.fake.rules(0).is_empty());
}

/// Go: `TestReconcile_DryRunIssuesNoCgroupCalls`. Dry-run computes and reports the set but pins
/// nothing, reads no `/proc` and builds no cgroup API.
#[tokio::test]
async fn reconcile_dry_run_issues_no_cgroup_calls() {
    let absent = TempDir::new();
    let mut options = Options::new("all", true);
    options.proc_root = Some(absent.path().join("absent").display().to_string());
    let env = Env::with(options);

    env.reconcile().await.unwrap();
    env.inspector.then(vec![Ok(exited())]);
    env.reconcile().await.unwrap();
    assert_eq!(env.apis.load(Ordering::SeqCst), 0);
    assert_eq!(env.fake.calls(), 0);
    assert!(env.pinner.pins.lock().unwrap().is_empty());
    assert!(
        env.log().contains(
            "level=INFO dry-run: would add device rule id=abc type=c major=1 minor=3\n\
             level=INFO dry-run: would set device rules id=abc rules=1"
        ),
        "{}",
        env.log()
    );
}

/// Go: `TestReconcile_CancelledCallerKeepsGrants`. An inspect that fails because the caller gave
/// up (daemon shutdown) is not Docker failing; nothing would retry, so a running container must
/// keep its grants.
#[tokio::test]
async fn reconcile_cancelled_caller_keeps_grants() {
    let env = Env::new("all", false);
    env.grant_once().await;
    env.inspector
        .then(vec![Err(docker::Error::context(ContextError::Canceled))]);
    let context = CallContext::default();
    context.cancel();

    let error = env.proc.reconcile(&context, "abc").await.unwrap_err();
    assert!(error.is_docker(docker::ErrorKind::Canceled), "{error}");
    assert_eq!(env.fake.calls(), 1, "only the grant");
}

/// Go: `TestReconcile_EmptySetWithMissingFilterIsNotDone`. An unprivileged container with no
/// device filter is never reported done, whatever its desired set.
#[tokio::test]
async fn reconcile_empty_set_with_missing_filter_is_not_done() {
    let env = Env::new("opt-in", false);
    env.fake.fail_with(|| cgroup::Error::FilterMissing);
    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_cgroup(cgroup::Error::is_filter_missing), "{error}");
    assert_eq!(
        error.to_string(),
        "container \"abc\" (reason filter_missing, retryable): set device rules: no device \
         filter attached to the cgroup"
    );
}

/// Go: `TestReconcile_SkippedContainerStillReportsIncompleteSet`. A container skipped for good
/// (privileged, no filter) keeps its incomplete-set error.
#[tokio::test]
async fn reconcile_skipped_container_still_reports_incomplete_set() {
    let env = Env::new("all", false);
    env.fake.fail_with(|| cgroup::Error::FilterMissing);
    env.inspector.then(vec![Ok(privileged(running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![
            bind("/dev/null", "/dev/null"),
            bind("/dev/sda-does-not-exist", "/dev/x"),
        ],
    )))]);
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.to_string().contains("incomplete_device_set"),
        "{error}"
    );
}

/// Go: `TestReconcile_DeadlineExceededRevokes`. An inspect that ran out of time is Docker not
/// answering, even when the caller's own deadline has passed too; the known grants are revoked.
#[tokio::test]
async fn reconcile_deadline_exceeded_revokes() {
    let env = Env::new("all", false);
    env.grant_once().await;
    env.inspector
        .then(vec![Err(docker::Error::deadline_exceeded())]);
    let context = CallContext::default().with_timeout(Duration::ZERO);

    let error = env.proc.reconcile(&context, "abc").await.unwrap_err();
    assert!(
        error.is_docker(docker::ErrorKind::DeadlineExceeded),
        "{error}"
    );
    assert_eq!(env.fake.calls(), 2);
    assert!(env.fake.rules(1).is_empty(), "the empty set applied");
}

/// Go: `TestReconcile_InspectFailureOfPrivilegedDoesNotWarn`. A privileged container never has a
/// device filter, so a missing one is not reported.
#[tokio::test]
async fn reconcile_inspect_failure_of_privileged_does_not_warn() {
    let env = Env::new("all", false);
    env.inspector.then(vec![Ok(privileged(running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![bind("/dev/null", "/dev/null")],
    )))]);
    env.grant_once().await;
    let before = env.log().len();
    env.fake.fail_with(|| cgroup::Error::FilterMissing);
    env.inspector.then(vec![daemon_unavailable()]);

    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.is_docker(docker::ErrorKind::ConnectionFailed),
        "{error}"
    );
    let after = &env.log()[before..];
    assert!(
        !after.contains("restart the container"),
        "a privileged container must not get the filter_missing warning:\n{after}"
    );
}

/// Go: `TestReconcile_NotFoundIsContainerGone`. A container Docker does not know is reported
/// gone, so it is not retried forever, but only once nothing is left to revoke for it.
#[tokio::test]
async fn reconcile_not_found_is_container_gone() {
    // Unknown.
    let env = Env::new("all", false);
    env.inspector.then(vec![not_found()]);
    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_container_gone(), "{error}");
    assert_eq!(env.fake.calls(), 0);
    assert_eq!(
        error.to_string(),
        "container no longer exists: inspect container \"abc\": Error response from daemon: No \
         such container: abc"
    );

    // Dry-run.
    let env = Env::new("all", true);
    env.inspector.then(vec![not_found()]);
    assert!(env.reconcile().await.unwrap_err().is_container_gone());

    // Known and revoked.
    let env = Env::new("all", false);
    env.grant_once().await;
    env.inspector.then(vec![not_found()]);
    let error = env.reconcile().await.unwrap_err();
    assert!(error.is_container_gone(), "{error}");
    assert_eq!(env.fake.calls(), 2);
    assert!(env.fake.rules(1).is_empty());

    // Known and revoke failed.
    let env = Env::new("all", false);
    env.grant_once().await;
    env.inspector.then(vec![not_found()]);
    env.fake
        .fail_with(|| cgroup::Error::Drift(vec!["revoke failed".to_owned()]));
    let error = env.reconcile().await.unwrap_err();
    assert!(
        !error.is_container_gone() && error.is_cgroup(cgroup::Error::is_drift),
        "want the revoke failure and the container kept pending: {error}"
    );
}

// ---- applyerr_test.go ----

/// Go's `devNullProcessor`.
fn dev_null_env() -> Env {
    Env::new("all", false)
}

/// Go: `TestReconcile_SetsRulesThroughOpenedHandle`. The processor opens the resolved cgroup
/// directory itself and hands the cgroup API a handle carrying that directory's inode.
#[tokio::test]
async fn reconcile_sets_rules_through_opened_handle() {
    let env = dev_null_env();
    env.reconcile().await.unwrap();
    let path = env.cgroup_dir();
    let inode = std::os::unix::fs::MetadataExt::ino(&std::fs::metadata(&path).unwrap());
    assert_eq!(
        env.fake.identity(),
        Identity {
            path: path.display().to_string(),
            inode,
        }
    );
}

/// Go: `TestReconcile_UnsupportedAttachModeSkipsContainer`.
#[tokio::test]
async fn reconcile_unsupported_attach_mode_skips_container() {
    let env = dev_null_env();
    env.fake
        .fail_with(|| cgroup::Error::UnsupportedAttachMode(0));
    env.reconcile().await.unwrap();
    assert_eq!(env.fake.calls(), 1);
    let log = env.log();
    assert!(
        log.contains("level=ERROR") && log.contains("attach mode none (exclusive)"),
        "want an ERROR naming the attach mode:\n{log}"
    );
}

/// Go: `TestReconcile_InaccessibleFiltersIsRetryableError`.
#[tokio::test]
async fn reconcile_inaccessible_filters_is_retryable_error() {
    let env = dev_null_env();
    env.fake.fail_with(|| cgroup::Error::FiltersInaccessible {
        inaccessible: 1,
        total: 2,
    });
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.is_cgroup(cgroup::Error::is_filters_inaccessible),
        "{error}"
    );
    assert!(
        error.to_string().contains("filters_inaccessible"),
        "{error}"
    );
}

/// Go: `TestReconcile_OwnedBlockConflictIsLoggedWithRemedy`.
#[tokio::test]
async fn reconcile_owned_block_conflict_is_logged_with_remedy() {
    let env = dev_null_env();
    env.fake
        .fail_with(|| cgroup::Error::Owned(OwnedError::Conflict(Conflict::Trailer)));
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.is_cgroup(cgroup::Error::is_owned_block_conflict)
            && error.to_string().contains("owned_block_conflict"),
        "{error}"
    );
    let log = env.log();
    assert!(
        log.contains("level=ERROR")
            && log.contains("restart the container")
            && log.contains("cgroup="),
        "want an ERROR with the cgroup and the remedy:\n{log}"
    );
}

/// Go: `TestReconcile_NotWrappableIsRetryableError`.
#[tokio::test]
async fn reconcile_not_wrappable_is_retryable_error() {
    let env = dev_null_env();
    env.fake
        .fail_with(|| cgroup::Error::Gate(GateError::UsesMaps(1)));
    let error = env.reconcile().await.unwrap_err();
    assert!(
        error.is_cgroup(cgroup::Error::is_program_not_wrappable)
            && error.to_string().contains("program_not_wrappable"),
        "{error}"
    );
}

/// Go: `TestReconcile_FilterMissing`.
#[tokio::test]
async fn reconcile_filter_missing() {
    for is_privileged in [true, false] {
        let env = dev_null_env();
        env.fake.fail_with(|| cgroup::Error::FilterMissing);
        let info = running(
            RECONCILE_PID,
            RECONCILE_STARTED,
            vec![bind("/dev/null", "/dev/null")],
        );
        env.inspector.then(vec![Ok(if is_privileged {
            privileged(info)
        } else {
            info
        })]);

        let result = env.reconcile().await;
        if is_privileged {
            assert!(result.is_ok(), "privileged: want a no-op: {result:?}");
            assert!(
                env.log()
                    .contains("privileged container has no device filter"),
                "{}",
                env.log()
            );
        } else {
            let error = result.unwrap_err();
            assert!(
                error.is_cgroup(cgroup::Error::is_filter_missing)
                    && error.to_string().contains("filter_missing"),
                "{error}"
            );
            assert!(env.log().contains("restart the container"), "{}", env.log());
        }
    }
}

// ---- lifecycle_test.go ----

/// Go: `TestLifecycle_InspectFailureThenSuccessUpgradesRecord`. An inspect failure leaves a
/// provisional record; the next successful reconcile upgrades that same record to verified with
/// the run's cgroup.
#[tokio::test]
async fn lifecycle_inspect_failure_then_success_upgrades_record() {
    let env = Env::new("all", false);
    env.inspector.then(vec![daemon_unavailable()]);
    let _ = env.reconcile().await;
    let records = env.proc.lifecycles().records("abc");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, LifecycleState::Provisional);
    assert!(!records[0].has_identity());

    env.inspector
        .then(vec![Ok(running_dev_null(RECONCILE_STARTED))]);
    env.reconcile().await.unwrap();
    let records = env.proc.lifecycles().records("abc");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, LifecycleState::Verified);
    assert_eq!(records[0].started_at, RECONCILE_STARTED);
    assert_eq!(records[0].identity(), Some(&env.fake.identity()));
}

/// Go: `TestTerminate_StaleDieAfterRestartLeavesNewRun`. The container restarted under the same
/// ID into a new cgroup; a die event for the old run, delivered late, revokes the old run's
/// cgroup only.
#[tokio::test]
async fn terminate_stale_die_after_restart_leaves_new_run() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let old_identity = env.fake.identity();
    let new_dir = env.host.path().join("sys/fs/cgroup/docker/restarted");
    std::fs::create_dir_all(&new_dir).unwrap();
    std::fs::write(new_dir.join("cgroup.procs"), format!("{RECONCILE_PID}\n")).unwrap();
    env.write_proc_cgroup("0::/docker/restarted\n");
    env.inspector.then(vec![Ok(running_dev_null(RESTARTED_AT))]);
    env.reconcile().await.unwrap();
    let new_identity = env.fake.identity();

    let event_time = started(RESTARTED_AT) - SignedDuration::from_mins(1);
    env.proc.terminate("abc", event_time).await.unwrap();
    assert_eq!(env.fake.calls(), 3);
    assert!(env.fake.rules(2).is_empty());
    assert_eq!(env.fake.identity(), old_identity);
    let new_run = env.proc.lifecycles().lookup("abc", RESTARTED_AT).unwrap();
    assert_eq!(new_run.state, LifecycleState::Verified);
    assert_eq!(new_run.identity(), Some(&new_identity));
}

/// Go: `TestTerminate_DieRevokesAndRetainsUntilGone`. A die event while the cgroup still exists
/// revokes there and keeps the record; the sweep releases it only once the cgroup is gone.
#[tokio::test]
async fn terminate_die_revokes_and_retains_until_gone() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let event_time = started(RECONCILE_STARTED) + SignedDuration::from_mins(1);
    env.proc.terminate("abc", event_time).await.unwrap();
    assert_eq!(env.fake.calls(), 2);
    assert!(env.fake.rules(1).is_empty());
    let records = verified_records(&env.proc);
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, LifecycleState::Terminal);
    assert!(records[0].revoked);

    assert_eq!(env.proc.sweep().await, 0, "the cgroup is still there");
    assert_eq!(verified_records(&env.proc).len(), 1);
    std::fs::remove_dir_all(env.cgroup_dir()).unwrap();
    assert_eq!(env.proc.sweep().await, 1, "the cgroup was removed");
    assert!(verified_records(&env.proc).is_empty());
    assert_eq!(env.fake.calls(), 2, "the sweep mutated nothing");
}

/// Go: `TestTerminate_ProvisionalOnly`. Without a verified run the provisional record becomes
/// terminal, nothing is mutated, and the sweep drops it.
#[tokio::test]
async fn terminate_provisional_only() {
    let env = Env::new("all", false);
    env.inspector.then(vec![daemon_unavailable()]);
    let _ = env.reconcile().await;

    env.proc.terminate("abc", env.clock.now()).await.unwrap();
    let records = env.proc.lifecycles().records("abc");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, LifecycleState::Terminal);
    assert_eq!(env.fake.calls(), 0);
    assert_eq!(env.proc.sweep().await, 1);
    assert!(env.proc.lifecycles().records("abc").is_empty());
}

/// Go: `TestSweep_DropsDeadEntries`. The sweep releases runs whose cgroup is gone or recreated
/// and stale provisional records, and keeps live runs.
#[tokio::test]
async fn sweep_drops_dead_entries() {
    let env = Env::new("all", false);
    env.grant_once().await;
    let now = env.clock.now();
    env.proc.lifecycles().provisional("gone");
    assert_eq!(env.proc.sweep().await, 0, "live or fresh records");

    let ttl = SignedDuration::try_from(PROVISIONAL_TTL).unwrap();
    env.clock.set(now + ttl + SignedDuration::from_secs(1));
    assert_eq!(env.proc.sweep().await, 1, "the stale provisional record");
    assert!(env.proc.lifecycles().records("gone").is_empty());

    recreate(&env.cgroup_dir());
    assert_eq!(env.proc.sweep().await, 1, "the run with a recreated cgroup");
    assert!(verified_records(&env.proc).is_empty());
    assert_eq!(env.fake.calls(), 1, "only the grant");
}

/// Go: `TestSweep_ReleasesOnlyTheEndedRecord`. Records without a start time all share an empty
/// `started_at`; dropping an ended one must keep a fresh one.
#[tokio::test]
async fn sweep_releases_only_the_ended_record() {
    let env = Env::new("all", false);
    let lifecycles = env.proc.lifecycles();
    let now = env.clock.now();
    lifecycles.provisional("abc");
    let _ = lifecycles.terminate("abc", now);
    env.clock.set(now + SignedDuration::from_secs(1));
    lifecycles.provisional("abc");

    assert_eq!(env.proc.sweep().await, 1);
    let records = lifecycles.records("abc");
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].state, LifecycleState::Provisional);
}

// ---- serialize_test.go ----

/// Go: `TestReconcile_SerializesConfigLoadThroughApply`. One worker is held between computing its
/// rules and applying them, then a second worker for the same container starts and a new config
/// is published. The second worker must not load the config or compute anything until the first
/// has finished, so no computation can be overtaken by an older one, and must then compute under
/// the new config.
#[tokio::test]
async fn reconcile_serializes_config_load_through_apply() {
    let modes: Arc<Mutex<Vec<Mode>>> = Arc::default();
    let computed = Arc::new(AtomicUsize::new(0));
    let (inside_sender, inside) = mpsc::channel::<()>();
    let (release, released) = mpsc::channel::<()>();
    let store_cell: Arc<Mutex<Option<Arc<Store>>>> = Arc::default();
    let hook = {
        let modes = Arc::clone(&modes);
        let computed = Arc::clone(&computed);
        let store_cell = Arc::clone(&store_cell);
        let inside_sender = Mutex::new(inside_sender);
        let released = Mutex::new(released);
        move || {
            let store = store_cell.lock().unwrap().clone().unwrap();
            modes.lock().unwrap().push(store.load().policy.mode());
            if computed.fetch_add(1, Ordering::SeqCst) == 0 {
                inside_sender.lock().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
        }
    };
    let mut options = Options::new("all", true);
    options.reply = Ok(running(60, "", Vec::new()));
    options.after_compute = Some(Box::new(hook));
    let env = Env::with(options);
    *store_cell.lock().unwrap() = Some(Arc::clone(&env.proc.store));

    let first = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        async move { proc.reconcile(&CallContext::default(), "c1").await }
    });
    tokio::task::spawn_blocking(move || inside.recv().unwrap())
        .await
        .unwrap();
    let second = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        async move { proc.reconcile(&CallContext::default(), "c1").await }
    });
    // Positive signal first: the second worker has finished its inspect, so it is at (or past)
    // the lock.
    crate::test_support::eventually("the second inspect", Duration::from_secs(2), || {
        env.inspector.calls.load(Ordering::SeqCst) >= 2
    })
    .await;
    // Published directly, not through publish_and_reconcile: this test is about the lock
    // ordering between workers, and the publication must land while the first worker holds the
    // lock.
    env.publish(runtime("opt-in", true));
    // Negative assertion: give the second worker a bounded window to (wrongly) get past the
    // lock.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        computed.load(Ordering::SeqCst),
        1,
        "the second worker computed while the first held the lock"
    );

    release.send(()).unwrap();
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    // The second worker computes (the empty set, for an unlabelled container under opt-in) from
    // the config published while it waited.
    assert_eq!(*modes.lock().unwrap(), [Mode::All, Mode::OptIn]);
}

/// Go: `TestPublishAndReconcile_StalePolicyRace`. A worker is paused after it computed a grant
/// under the old config, a narrower config is published through `publish_and_reconcile`, then
/// the worker is released. The publication waits for the worker, and the pass it requests runs
/// afterwards, so the cgroup ends with the narrower (empty) set rather than the stale grant.
#[tokio::test]
async fn publish_and_reconcile_stale_policy_race() {
    let (held_sender, held) = mpsc::channel::<()>();
    let (release, released) = mpsc::channel::<()>();
    let once = AtomicBool::new(false);
    let hook = {
        let held_sender = Mutex::new(held_sender);
        let released = Mutex::new(released);
        move || {
            if !once.swap(true, Ordering::SeqCst) {
                held_sender.lock().unwrap().send(()).unwrap();
                released.lock().unwrap().recv().unwrap();
            }
        }
    };
    let mut options = Options::new("all", false);
    options.after_compute = Some(Box::new(hook));
    let env = Env::with(options);
    // The pass the coordinator would run: reconcile the one container.
    let (pass_sender, mut pass) = tokio::sync::mpsc::unbounded_channel();
    let weak: Weak<Processor<FakeInspector>> = Arc::downgrade(&env.proc);
    env.proc
        .set_pass_requester(Arc::new(move |context, _generation| {
            let proc = weak.upgrade().unwrap();
            let context = context.clone();
            let pass_sender = pass_sender.clone();
            tokio::spawn(async move {
                pass_sender
                    .send(proc.reconcile(&context, "abc").await)
                    .unwrap();
            });
        }))
        .await;

    let worker = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        async move { proc.reconcile(&CallContext::default(), "abc").await }
    });
    tokio::task::spawn_blocking(move || held.recv().unwrap())
        .await
        .unwrap();
    let publication = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        async move {
            proc.publish_and_reconcile(&CallContext::default(), runtime("opt-in", false))
                .await
        }
    });
    // Negative assertion: the publication must wait for the worker.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert!(
        !publication.is_finished(),
        "published while a worker held the lock"
    );

    release.send(()).unwrap();
    worker.await.unwrap().unwrap();
    assert_eq!(publication.await.unwrap(), 2);
    pass.recv().await.unwrap().unwrap();
    assert_eq!(env.fake.calls(), 2);
    assert_eq!(env.fake.rules(0).len(), 1);
    assert!(
        env.fake.rules(1).is_empty(),
        "the stale grant replaced by the empty set"
    );
}

// ---- the blocking transaction ----

/// The descriptors this process holds on `dir`.
fn descriptors_on(dir: &Path) -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| std::fs::read_link(entry.path()).is_ok_and(|target| target == dir))
        .count()
}

/// A caller cancelled (and dropped) during an apply does not end the mutation: the transaction
/// keeps the processor lock, the pin and the cgroup descriptor until the mutation completes, the
/// next reconcile of the same container waits for it, and so does the shutdown wait.
#[tokio::test]
async fn cancelled_caller_does_not_end_the_mutation() {
    let computed = Arc::new(AtomicUsize::new(0));
    let mut options = Options::new("all", false);
    options.after_compute = Some(Box::new({
        let computed = Arc::clone(&computed);
        move || {
            computed.fetch_add(1, Ordering::SeqCst);
        }
    }));
    let env = Env::with(options);
    let (entered_sender, entered) = mpsc::channel::<()>();
    let (release, released) = mpsc::channel::<()>();
    let first_call = AtomicBool::new(true);
    let gate = (Mutex::new(entered_sender), Mutex::new(released));
    *env.fake.on_set.lock().unwrap() = Some(Arc::new(move || {
        if first_call.swap(false, Ordering::SeqCst) {
            gate.0.lock().unwrap().send(()).unwrap();
            gate.1.lock().unwrap().recv().unwrap();
        }
    }));
    let dir = env.cgroup_dir();

    let context = CallContext::default();
    let caller = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        let context = context.clone();
        async move { proc.reconcile(&context, "abc").await }
    });
    tokio::task::spawn_blocking(move || entered.recv().unwrap())
        .await
        .unwrap();
    context.cancel();
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());

    assert!(env.proc.lock.try_lock().is_err(), "the lock is still held");
    assert_eq!(env.pinner.held.load(Ordering::SeqCst), 1, "the pin is held");
    assert_eq!(descriptors_on(&dir), 1, "the cgroup descriptor is held");

    let shutdown = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        async move { proc.close_and_wait().await }
    });
    let next = tokio::spawn({
        let proc = Arc::clone(&env.proc);
        async move { proc.reconcile(&CallContext::default(), "abc").await }
    });
    // Negative assertion: give the next reconcile a bounded window to (wrongly) compute while
    // the mutation still holds the lock.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(
        computed.load(Ordering::SeqCst),
        1,
        "the next reconcile computed during the mutation"
    );
    assert!(
        !shutdown.is_finished(),
        "the shutdown wait ended before the mutation of a dropped caller"
    );

    release.send(()).unwrap();
    shutdown.await.unwrap();
    assert_eq!(
        env.fake.calls(),
        2,
        "the shutdown wait covered both transactions"
    );
    next.await.unwrap().unwrap();
    assert_eq!(computed.load(Ordering::SeqCst), 2);
    assert_eq!(
        env.fake.calls(),
        2,
        "the mutation completed, then the next one"
    );
    assert_eq!(env.pinner.held.load(Ordering::SeqCst), 0);
    assert_eq!(descriptors_on(&dir), 0);
}

// ---- texts ----

/// The texts of the processor's own errors, each the output of a Go program reconciling the same
/// container the same way (the real pidfd pinner, the fake `/proc` and host cgroup).
#[tokio::test]
async fn error_texts() {
    let env = Env::new("all", false);
    env.inspector.then(vec![
        Ok(running_dev_null(RECONCILE_STARTED)),
        Ok(running_dev_null("2026-09-27T10:00:05.000000000Z")),
    ]);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "container \"abc\" pid 80: container changed between inspect and mutation"
    );

    let env = Env::new("all", false);
    write_procs(env.host.path(), &[]);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "container \"abc\" pid 80: container process is not in the resolved cgroup"
    );

    let mut options = Options::new("all", false);
    options.host_root = Some("/nonexistent-host".to_owned());
    let env = Env::with(options);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "set device rules: open cgroup \"/nonexistent-host/sys/fs/cgroup/docker/testcontainer\": \
         no such file or directory"
    );

    let mut options = Options::new("all", false);
    options.proc_root = Some("/nonexistent-proc".to_owned());
    let env = Env::with(options);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "container \"abc\": open \"/nonexistent-proc/proc/80\": no such file or directory"
    );

    let mut options = Options::new("all", false);
    options.real_pinner = true;
    options.reply = Ok(running(-1, "", vec![bind("/dev/null", "/dev/null")]));
    let env = Env::with(options);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "container \"abc\": pidfd_open(-1): invalid argument"
    );

    let env = Env::new("all", false);
    env.inspector.then(vec![Ok(running(
        RECONCILE_PID,
        RECONCILE_STARTED,
        vec![bind("/dev/ttyUSB-gone", "/dev/x")],
    ))]);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "container \"abc\" (reason incomplete_device_set, retryable): device identity could not \
         be established: mount source /dev/ttyUSB-gone: no such file or directory"
    );
    assert!(
        env.log().contains(
            "level=WARN device set incomplete; no devices are granted until every device \
             resolves id=abc errors=1 reason=incomplete_device_set"
        ),
        "{}",
        env.log()
    );

    let env = Env::new("all", false);
    env.inspector.then(vec![
        Ok(running_dev_null(RECONCILE_STARTED)),
        daemon_unavailable(),
    ]);
    assert_eq!(
        env.reconcile().await.unwrap_err().to_string(),
        "verify container \"abc\": inspect container \"abc\": daemon unavailable"
    );
}

// ---- the computeDesired tests of rules_test.go ----

/// `compute_desired` of a running container with `mounts` under the `all` mode, `/dev` opened by
/// `devfs`.
fn compute_with(devfs: Box<OpenDevFs>, mounts: Vec<MountPoint>) -> Desired {
    let (store, publisher) = new_store(runtime("all", false));
    let proc = Processor::with_seams(
        Settings {
            inspector: FakeInspector(Arc::default()),
            store,
            publisher,
            metrics: Recorder::noop(),
            host_root: "/host".to_owned(),
            proc_root: "/".to_owned(),
            call_timeout: None,
            clock: Arc::new(FakeClock::new(Timestamp::UNIX_EPOCH)),
            log: capture_logs(Level::DEBUG).0,
        },
        Seams {
            devfs: Some(devfs),
            ..Seams::default()
        },
    );
    proc.compute_desired("abc", &running(1, "", mounts), 1, &runtime("all", false))
}

/// `/dev` opened as the fake.
fn over(dev: &Arc<FakeDevFs>) -> Box<OpenDevFs> {
    let dev = Arc::clone(dev);
    Box::new(move || Ok(Box::new(Arc::clone(&dev)) as Box<dyn DevFs>))
}

fn incomplete_errors(desired: &Desired) -> &[DeviceError] {
    match &desired.incomplete {
        Some(Error::Incomplete { errors, .. }) => errors,
        other => panic!("want an incomplete set, got {other:?}"),
    }
}

/// Go: `TestCollect_IncompleteEnumeration`, its `computeDesired` half. Every way a directory
/// mount's names can go unseen leaves the set unknown, and the desired set empty.
#[test]
fn compute_desired_empties_an_incomplete_enumeration() {
    type Setup = fn(&mut FakeDevFs);
    type Class = fn(&DeviceError) -> bool;
    let cases: [(&str, Setup, Class); 4] = [
        (
            "root cannot be opened",
            |dev| {
                dev.directory("dri");
                dev.device("dri/card0", DEV_CARD, "dri/card0");
                dev.errs.insert(
                    "dri".to_owned(),
                    SysError::new("openat2 dri", Errno::ACCESS),
                );
            },
            DeviceError::is_unresolved,
        ),
        (
            "root vanished before the walk",
            |dev| {
                dev.nodes.insert(
                    "dri".to_owned(),
                    FakeNode {
                        kind: NodeKind::Dir,
                        path: "/dev/dri".to_owned(),
                    },
                );
            },
            DeviceError::is_incomplete_walk,
        ),
        (
            "directory read error",
            |dev| {
                dev.directory("dri");
                dev.directory("dri/by-path");
                dev.walk_errs
                    .insert("dri/by-path".to_owned(), SysError::new("", Errno::ACCESS));
            },
            DeviceError::is_incomplete_walk,
        ),
        (
            "entry error",
            |dev| {
                dev.directory("dri");
                dev.touch("dri/renderD128");
                dev.walk_errs
                    .insert("dri/renderD128".to_owned(), SysError::new("", Errno::IO));
            },
            DeviceError::is_incomplete_walk,
        ),
    ];
    for (name, setup, class) in cases {
        let mut dev = FakeDevFs::new();
        dev.device("null", DEV_NULL, "null");
        setup(&mut dev);
        let desired = compute_with(
            over(&Arc::new(dev)),
            vec![bind("/dev/null", "/dev/null"), bind("/dev/dri", "/dev/dri")],
        );
        assert!(desired.rules.is_empty(), "{name}: {:?}", desired.rules);
        assert!(
            incomplete_errors(&desired).iter().any(class),
            "{name}: {:?}",
            desired.incomplete
        );
    }
}

/// Go: `TestCollect_OnlyBindMountsAreWalked`, its `computeDesired` half. A container without a
/// `/dev` bind mount does not even open `/dev`, and its set is empty and complete.
#[test]
fn compute_desired_does_not_open_dev_without_a_bind_mount() {
    let opened = Arc::new(AtomicBool::new(false));
    let devfs: Box<OpenDevFs> = {
        let opened = Arc::clone(&opened);
        Box::new(move || {
            opened.store(true, Ordering::SeqCst);
            Err(SysError::new("", Errno::NOSYS))
        })
    };
    let mount = |kind: &str, destination: &str| MountPoint {
        kind: kind.to_owned(),
        source: "/dev/sda".to_owned(),
        destination: destination.to_owned(),
    };
    let desired = compute_with(
        devfs,
        vec![
            mount("volume", "/data"),
            mount("tmpfs", "/tmp"),
            mount("npipe", "/pipe"),
            mount("", "/untyped"),
        ],
    );
    assert!(
        !opened.load(Ordering::SeqCst),
        "/dev opened for a container without a /dev bind mount"
    );
    assert!(desired.rules.is_empty());
    assert!(desired.incomplete.is_none(), "{:?}", desired.incomplete);
}

/// Go: `TestProcessor_DevFSOpenFailureEmptiesTheSet`.
#[test]
fn compute_desired_empties_the_set_when_dev_cannot_be_opened() {
    let desired = compute_with(
        Box::new(|| Err(SysError::new("open /dev", Errno::NOSYS))),
        vec![bind("/dev/null", "/dev/null")],
    );
    assert!(desired.rules.is_empty());
    let incomplete = desired.incomplete.as_ref().unwrap().to_string();
    assert!(incomplete.contains("retryable"), "{incomplete}");
    assert_eq!(
        incomplete,
        "container \"abc\" (reason incomplete_device_set, retryable): device identity could not \
         be established: open /dev: function not implemented"
    );
}

/// Go: `TestProcessor_UnresolvedCandidateEmptiesTheSet`.
#[test]
fn compute_desired_empties_the_set_for_an_unresolved_candidate() {
    let mut dev = FakeDevFs::new();
    dev.device("null", DEV_NULL, "null");
    dev.device("sda", DEV_SDA, "sda");
    dev.uevents.remove(&DEV_SDA);
    let desired = compute_with(
        over(&Arc::new(dev)),
        vec![bind("/dev/null", "/dev/null"), bind("/dev/sda", "/dev/sda")],
    );
    assert!(desired.rules.is_empty());
    let message = desired.incomplete.as_ref().unwrap().to_string();
    for fragment in ["incomplete_device_set, retryable", "/dev/sda", "b 8:0"] {
        assert!(
            message.contains(fragment),
            "err = {message:?}, want {fragment:?}"
        );
    }
}
