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

//! Go: `reload_test.go`, on the paused clock, with a channel for the signal source and scripted
//! opens for the supervisor. The decoding of zbus messages, the match rules, the address parsing
//! and the dial errors are new: godbus did them for the Go watcher.

use std::future::Future;
use std::sync::atomic::{AtomicUsize, Ordering};

use tokio::sync::{Notify, mpsc};

use super::*;
use crate::logger::Level;
use crate::test_support::{TempDir, capture_logs};

const TEST_OWNER: &str = ":1.1";

/// Go: `completed`.
fn completed(sender: &str, path: &str) -> Signal {
    Signal {
        sender: sender.to_owned(),
        path: path.to_owned(),
        name: RELOADING_FULL_NAME.to_owned(),
        body: vec![Arg::Bool(false)],
    }
}

/// Go: `ownerChanged`.
fn owner_changed(sender: &str, new_owner: &str) -> Signal {
    Signal {
        sender: sender.to_owned(),
        path: BUS_DAEMON_PATH.to_owned(),
        name: NAME_OWNER_CHANGED_FULL_NAME.to_owned(),
        body: vec![
            Arg::Str(SYSTEMD_BUS_NAME.to_owned()),
            Arg::Str(TEST_OWNER.to_owned()),
            Arg::Str(new_owner.to_owned()),
        ],
    }
}

/// Sends `signal` and returns once the watcher has taken it. The watcher handles a signal without
/// awaiting, so on this single-threaded runtime taken is handled: Go's unbuffered send.
async fn deliver(sender: &mpsc::Sender<Signal>, signal: Signal) {
    sender.send(signal).await.unwrap();
    while sender.capacity() < sender.max_capacity() {
        tokio::task::yield_now().await;
    }
}

/// A watcher on a channel of its own, owned by [`TEST_OWNER`], logging to `log`.
fn start_watch(
    on_reload: OnReload,
    log: LogHandle,
) -> (mpsc::Sender<Signal>, tokio::task::JoinHandle<()>) {
    let (sender, mut signals) = mpsc::channel(1);
    let task = tokio::spawn(async move {
        watch(
            &CancellationToken::new(),
            &mut signals,
            TEST_OWNER.to_owned(),
            &on_reload,
            &log,
        )
        .await;
    });
    (sender, task)
}

/// Feeds `signals` one by one and returns how many times `on_reload` ran once the watcher has
/// returned. Go: `runWatch`.
async fn run_watch(signals: Vec<Signal>) -> usize {
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&calls);
    let (sender, task) = start_watch(
        Arc::new(move || {
            counter.fetch_add(1, Ordering::SeqCst);
        }),
        capture_logs(Level::DEBUG).0,
    );
    for signal in signals {
        deliver(&sender, signal).await;
    }
    drop(sender);
    task.await.unwrap();
    calls.load(Ordering::SeqCst)
}

/// Go: `TestIsReloadCompleted`; Go's nil signal has no Rust counterpart.
#[test]
fn only_the_completion_edge_is_a_completed_reload() {
    let reloading = |body: Vec<Arg>| Signal {
        body,
        ..completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)
    };
    let cases = [
        (
            "completion edge (active=false)",
            reloading(vec![Arg::Bool(false)]),
            true,
        ),
        (
            "start edge (active=true)",
            reloading(vec![Arg::Bool(true)]),
            false,
        ),
        (
            "wrong signal name",
            Signal {
                name: "org.freedesktop.systemd1.Manager.UnitNew".to_owned(),
                ..reloading(vec![Arg::Bool(false)])
            },
            false,
        ),
        ("empty body", reloading(vec![]), false),
        (
            "non-boolean body",
            reloading(vec![Arg::Str("not-a-bool".to_owned())]),
            false,
        ),
    ];
    for (name, signal, want) in cases {
        assert_eq!(is_reload_completed(&signal), want, "{name}");
    }
}

/// Go: `TestWatch_OnlySystemdCounts`.
#[tokio::test(start_paused = true)]
async fn only_systemd_counts() {
    let cases = [
        ("foreign sender", completed(":1.99", SYSTEMD_OBJECT_PATH)),
        (
            "well-known name spoof",
            completed(SYSTEMD_BUS_NAME, SYSTEMD_OBJECT_PATH),
        ),
        (
            "foreign object path",
            completed(TEST_OWNER, "/org/example/evil"),
        ),
        (
            "start edge from owner",
            Signal {
                body: vec![Arg::Bool(true)],
                ..completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)
            },
        ),
    ];
    for (name, signal) in cases {
        assert_eq!(run_watch(vec![signal]).await, 0, "{name}");
    }

    // A peer claiming the name changed hands to it is not believed.
    let calls = run_watch(vec![
        owner_changed(":1.99", ":1.99"),
        completed(":1.99", SYSTEMD_OBJECT_PATH),
    ])
    .await;
    assert_eq!(calls, 0, "owner change by a peer");

    let calls = run_watch(vec![completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)]).await;
    assert_eq!(calls, 1, "systemd's own completion");
}

/// Go: `TestWatch_FollowsOwnerChangeFromTheBusDaemon`. A peer that claims the owner changed must
/// not be believed; the bus daemon's `NameOwnerChanged` moves trust to systemd's new unique name.
#[tokio::test(start_paused = true)]
async fn the_owner_follows_the_bus_daemon() {
    let calls = run_watch(vec![
        owner_changed(":1.99", ":1.99"),
        completed(":1.99", SYSTEMD_OBJECT_PATH),
        owner_changed(BUS_DAEMON_NAME, ":1.5"),
        completed(TEST_OWNER, SYSTEMD_OBJECT_PATH),
        completed(":1.5", SYSTEMD_OBJECT_PATH),
        owner_changed(BUS_DAEMON_NAME, ""),
        completed(":1.5", SYSTEMD_OBJECT_PATH),
    ])
    .await;
    assert_eq!(calls, 1, "only :1.5 while it owned the name");
}

/// Go: `TestWatch_CoalescesWhileRunning`. Reloads that complete while a re-apply runs collapse
/// into one more run after it.
#[tokio::test(start_paused = true)]
async fn reloads_during_a_run_coalesce() {
    let calls = Arc::new(AtomicUsize::new(0));
    let started = Arc::new(Notify::new());
    let (release, released) = std::sync::mpsc::channel::<()>();
    let released = Mutex::new(released);
    let on_reload: OnReload = {
        let calls = Arc::clone(&calls);
        let started = Arc::clone(&started);
        Arc::new(move || {
            if calls.fetch_add(1, Ordering::SeqCst) == 0 {
                started.notify_one();
                released.lock().unwrap().recv().unwrap();
            }
        })
    };
    let (sender, task) = start_watch(on_reload, capture_logs(Level::DEBUG).0);

    deliver(&sender, completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)).await;
    started.notified().await;
    for _ in 0..3 {
        deliver(&sender, completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)).await;
    }
    deliver(&sender, completed(":1.99", SYSTEMD_OBJECT_PATH)).await;

    release.send(()).unwrap();
    drop(sender);
    task.await.unwrap();
    assert_eq!(
        calls.load(Ordering::SeqCst),
        2,
        "the running one and one trailing run"
    );
}

/// Go: `TestWatch_UsesTheCurrentLogger`. A reload of the log settings reaches a watcher that is
/// already running: it logs through the shared handle at each site.
#[tokio::test(start_paused = true)]
async fn the_watcher_logs_through_the_current_logger() {
    let (log, before) = capture_logs(Level::INFO);
    let (after_log, after) = capture_logs(Level::INFO);
    let (sender, task) = start_watch(Arc::new(|| {}), log.clone());

    deliver(&sender, completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)).await;
    deliver(&sender, completed(":1.99", SYSTEMD_OBJECT_PATH)).await;
    log.set(after_log.logger());
    deliver(&sender, completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)).await;
    drop(sender);
    task.await.unwrap();

    let line = "level=INFO systemd reload completed; re-applying device rules\n";
    assert_eq!(before.contents(), line);
    assert_eq!(after.contents(), line);
}

/// One scripted result of an open.
enum Step {
    /// Go: `failOpen`.
    Fail,
    /// A watcher on the given signal source. Go: `openOn`.
    OpenOn(mpsc::Receiver<Signal>),
    /// An open that never finishes, like a wedged bus, until it is dropped. Go: `hungOpen`, and
    /// the open that returns a watcher only once its context is cancelled, which in Rust cannot
    /// return at all: a timed-out open is dropped.
    Hang,
    /// [`Step::Hang`], announcing that it started.
    HangAndAnnounce(Arc<Notify>),
    /// A signal source already handed out.
    Used,
}

/// The calls so far, the opens in flight, and the most that ever were.
#[derive(Default)]
struct OpenStats {
    calls: usize,
    in_flight: usize,
    max_in_flight: usize,
}

type Opening = Pin<Box<dyn Future<Output = Result<Watcher, Error>> + Send>>;

/// Scripts successive open results for a supervisor and records how many attempts ever ran at
/// once. Go: `fakeOpens`.
struct FakeOpens {
    steps: Mutex<Vec<Step>>,
    stats: Mutex<OpenStats>,
}

impl FakeOpens {
    fn new(steps: Vec<Step>) -> Arc<Self> {
        Arc::new(Self {
            steps: Mutex::new(steps),
            stats: Mutex::default(),
        })
    }

    fn open(self: &Arc<Self>) -> Opening {
        let step = {
            let mut steps = self.steps.lock().unwrap();
            // The last step repeats; a signal source can be handed out once.
            if steps.len() > 1 {
                steps.remove(0)
            } else {
                match &steps[0] {
                    Step::Fail => Step::Fail,
                    Step::Hang => Step::Hang,
                    Step::HangAndAnnounce(entered) => Step::HangAndAnnounce(Arc::clone(entered)),
                    Step::OpenOn(_) => std::mem::replace(&mut steps[0], Step::Used),
                    Step::Used => panic!("a single-use signal source opened twice"),
                }
            }
        };
        let in_flight = InFlight::enter(Arc::clone(self));
        Box::pin(async move {
            let _in_flight = in_flight;
            match step {
                Step::Fail => Err(Error::SetupTimeout),
                Step::OpenOn(signals) => Ok(Watcher::from_signals(signals, TEST_OWNER)),
                Step::Hang | Step::Used => std::future::pending().await,
                Step::HangAndAnnounce(entered) => {
                    entered.notify_one();
                    std::future::pending().await
                }
            }
        })
    }

    /// How many opens ran, and the most that ever ran at once.
    fn stats(&self) -> (usize, usize) {
        let stats = self.stats.lock().unwrap();
        (stats.calls, stats.max_in_flight)
    }
}

/// One open in flight, counted until it is dropped.
struct InFlight(Arc<FakeOpens>);

impl InFlight {
    fn enter(opens: Arc<FakeOpens>) -> Self {
        {
            let mut stats = opens.stats.lock().unwrap();
            stats.calls += 1;
            stats.in_flight += 1;
            stats.max_in_flight = stats.max_in_flight.max(stats.in_flight);
        }
        Self(opens)
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.stats.lock().unwrap().in_flight -= 1;
    }
}

/// A supervisor over `opens` with the given timings.
fn supervisor(
    opens: &Arc<FakeOpens>,
    setup_timeout: Duration,
    min_retry: Duration,
    max_retry: Duration,
) -> Supervisor<impl Fn() -> Opening + use<>> {
    let opens = Arc::clone(opens);
    Supervisor {
        open: move || opens.open(),
        setup_timeout,
        min_retry,
        max_retry,
        log: capture_logs(Level::DEBUG).0,
    }
}

/// A running supervisor with millisecond timings, the channels that receive one value per
/// `on_subscribed` and `on_reload` call, and its stop: it must return promptly. Go:
/// `startSupervisor`.
struct Running {
    subscribed: mpsc::UnboundedReceiver<()>,
    reloaded: mpsc::UnboundedReceiver<()>,
    cancel: CancellationToken,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    fn start(opens: &Arc<FakeOpens>) -> Self {
        let (subscribed_sender, subscribed) = mpsc::unbounded_channel();
        let (reloaded_sender, reloaded) = mpsc::unbounded_channel();
        let supervisor = supervisor(
            opens,
            Duration::from_millis(50),
            Duration::from_millis(1),
            Duration::from_millis(4),
        );
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let task = tokio::spawn(async move {
            let on_reload: OnReload = Arc::new(move || reloaded_sender.send(()).unwrap());
            supervisor
                .run(&token, &on_reload, || subscribed_sender.send(()).unwrap())
                .await;
        });
        Self {
            subscribed,
            reloaded,
            cancel,
            task,
        }
    }

    async fn stop(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(5), self.task)
            .await
            .expect("supervisor did not return after cancel")
            .unwrap();
    }
}

/// Waits for one value on `receiver`, failing after five seconds. Go: `receive`.
async fn receive(receiver: &mut mpsc::UnboundedReceiver<()>, what: &str) {
    tokio::time::timeout(Duration::from_secs(5), receiver.recv())
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {what}"))
        .unwrap();
}

/// Go: `TestSupervise_RetriesUntilSubscribed`. A system bus that is not reachable yet is
/// retried; the first subscription requests a pass and reloads are delivered from then on.
#[tokio::test(start_paused = true)]
async fn the_supervisor_retries_until_subscribed() {
    let (sender, signals) = mpsc::channel(1);
    let opens = FakeOpens::new(vec![Step::Fail, Step::Fail, Step::OpenOn(signals)]);
    let mut running = Running::start(&opens);

    receive(&mut running.subscribed, "the first subscription").await;
    sender
        .send(completed(TEST_OWNER, SYSTEMD_OBJECT_PATH))
        .await
        .unwrap();
    receive(&mut running.reloaded, "the reload").await;
    running.stop().await;
}

/// Go: `TestSupervise_ResubscribesAfterDisconnect`. A lost connection (the signal channel
/// closes, as on a D-Bus restart) is resubscribed, and the new subscription requests a pass
/// again: reloads during the outage were not seen.
#[tokio::test(start_paused = true)]
async fn the_supervisor_resubscribes_after_a_disconnect() {
    let (first, first_signals) = mpsc::channel(1);
    let (second, second_signals) = mpsc::channel(1);
    let opens = FakeOpens::new(vec![
        Step::OpenOn(first_signals),
        Step::OpenOn(second_signals),
    ]);
    let mut running = Running::start(&opens);

    receive(&mut running.subscribed, "the first subscription").await;
    drop(first);
    receive(&mut running.subscribed, "the resubscription").await;
    second
        .send(completed(TEST_OWNER, SYSTEMD_OBJECT_PATH))
        .await
        .unwrap();
    receive(&mut running.reloaded, "the reload on the new subscription").await;
    running.stop().await;
}

/// Go: `TestSupervise_CancelsAHungAttempt`. An attempt stuck on a wedged bus is given up after
/// the setup timeout, and ends before the next attempt starts.
#[tokio::test(start_paused = true)]
async fn the_supervisor_gives_up_a_hung_attempt() {
    let (_sender, signals) = mpsc::channel(1);
    let opens = FakeOpens::new(vec![Step::Hang, Step::Hang, Step::OpenOn(signals)]);
    let mut running = Running::start(&opens);

    receive(
        &mut running.subscribed,
        "the subscription after the hung attempts",
    )
    .await;
    assert_eq!(opens.stats(), (3, 1), "want 3 attempts, one at a time");
    running.stop().await;
}

/// Go: `TestSupervise_CancelWhileAttemptHangs`.
#[tokio::test(start_paused = true)]
async fn a_cancel_while_an_attempt_hangs_returns() {
    let entered = Arc::new(Notify::new());
    let opens = FakeOpens::new(vec![Step::HangAndAnnounce(Arc::clone(&entered))]);
    let hour = Duration::from_hours(1);
    let supervisor = supervisor(&opens, hour, hour, hour);
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let task = tokio::spawn(async move {
        let on_reload: OnReload = Arc::new(|| {});
        supervisor
            .run(&token, &on_reload, || {
                panic!("subscribed without a watcher")
            })
            .await;
    });

    entered.notified().await;
    cancel.cancel();
    tokio::time::timeout(Duration::from_secs(5), task)
        .await
        .expect("supervisor did not return after cancel while an attempt hung")
        .unwrap();
    assert_eq!(opens.stats().0, 1, "no attempt after cancel");
}

/// The supervisor's log lines and levels, Go's: a warning on the first failure, debug after,
/// info when it is back, and a warning on a lost subscription.
#[tokio::test(start_paused = true)]
async fn the_supervisor_logs_as_go_does() {
    let (sender, signals) = mpsc::channel(1);
    let opens = FakeOpens::new(vec![Step::Fail, Step::Fail, Step::OpenOn(signals)]);
    let (log, sink) = capture_logs(Level::DEBUG);
    let supervisor = Supervisor {
        log,
        ..supervisor(
            &opens,
            Duration::from_millis(50),
            Duration::from_secs(1),
            Duration::from_mins(5),
        )
    };
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let (subscribed_sender, mut subscribed) = mpsc::unbounded_channel();
    let task = tokio::spawn(async move {
        let on_reload: OnReload = Arc::new(|| {});
        supervisor
            .run(&token, &on_reload, || subscribed_sender.send(()).unwrap())
            .await;
    });
    receive(&mut subscribed, "the subscription").await;
    drop(sender);
    // A bounded window for the disconnect to be logged: the retry that follows sleeps 4s.
    tokio::time::sleep(Duration::from_secs(1)).await;
    cancel.cancel();
    task.await.unwrap();

    assert_eq!(
        sink.contents(),
        "level=WARN systemd reload handling unavailable; retrying in the background \
         err=connecting to the system bus timed out retry_in=1s\n\
         level=DEBUG systemd reload watcher still unavailable \
         err=connecting to the system bus timed out retry_in=2s\n\
         level=INFO systemd reload handling available again\n\
         level=DEBUG systemd reload watcher started systemd=:1.1\n\
         level=DEBUG systemd signal channel closed\n\
         level=WARN systemd reload watcher disconnected; resubscribing retry_in=4s\n"
    );
}

/// The match rules are godbus's, with zbus's key order: `path` comes last.
#[test]
fn the_match_rules_are_godbus_s() {
    let reloading = rule(
        SYSTEMD_BUS_NAME,
        SYSTEMD_OBJECT_PATH,
        RELOADING_INTERFACE,
        RELOADING_MEMBER,
        None,
    )
    .unwrap();
    assert_eq!(
        reloading.to_string(),
        "type='signal',sender='org.freedesktop.systemd1',\
         interface='org.freedesktop.systemd1.Manager',member='Reloading',\
         path='/org/freedesktop/systemd1'"
    );
    let owner_changed = rule(
        BUS_DAEMON_NAME,
        BUS_DAEMON_PATH,
        BUS_DAEMON_NAME,
        NAME_OWNER_CHANGED_MEMBER,
        Some(SYSTEMD_BUS_NAME),
    )
    .unwrap();
    assert_eq!(
        owner_changed.to_string(),
        "type='signal',sender='org.freedesktop.DBus',interface='org.freedesktop.DBus',\
         member='NameOwnerChanged',path='/org/freedesktop/DBus',\
         arg0='org.freedesktop.systemd1'"
    );
}

/// A message decodes to godbus's view of it: the sender, the path, the interface and member
/// joined, and the arguments the watcher reads.
#[test]
fn messages_decode_as_godbus_signals() {
    let message = Message::signal(SYSTEMD_OBJECT_PATH, RELOADING_INTERFACE, RELOADING_MEMBER)
        .sender(TEST_OWNER)
        .build(&(false,))
        .unwrap();
    assert_eq!(
        Signal::from_message(&message),
        completed(TEST_OWNER, SYSTEMD_OBJECT_PATH)
    );

    let message = Message::signal(BUS_DAEMON_PATH, BUS_DAEMON_NAME, NAME_OWNER_CHANGED_MEMBER)
        .sender(BUS_DAEMON_NAME)
        .build(&(SYSTEMD_BUS_NAME, TEST_OWNER, ":1.5"))
        .unwrap();
    assert_eq!(
        Signal::from_message(&message),
        owner_changed(BUS_DAEMON_NAME, ":1.5")
    );

    // An object path is not a string to godbus, and a signal without a body has no arguments.
    let path = zbus::ObjectPath::try_from("/x").unwrap();
    let message = Message::signal(SYSTEMD_OBJECT_PATH, RELOADING_INTERFACE, RELOADING_MEMBER)
        .build(&(path, 7_u32))
        .unwrap();
    assert_eq!(
        Signal::from_message(&message).body,
        [Arg::Other, Arg::Other]
    );
    let message = Message::signal(SYSTEMD_OBJECT_PATH, RELOADING_INTERFACE, RELOADING_MEMBER)
        .build(&())
        .unwrap();
    let signal = Signal::from_message(&message);
    assert_eq!((signal.sender.as_str(), signal.body.len()), ("", 0));
}

/// Only a single `unix:path=` address is dialled here, with D-Bus's escapes undone.
#[test]
fn unix_path_addresses_are_parsed() {
    let cases = [
        (
            "unix:path=/var/run/dbus/system_bus_socket",
            Some("/var/run/dbus/system_bus_socket"),
        ),
        ("unix:path=/tmp/a%20b", Some("/tmp/a b")),
        ("unix:path=/tmp/a%2", None),
        ("unix:path=/tmp/a%zz", None),
        ("unix:path=/a,guid=0123", None),
        ("unix:path=/a;unix:path=/b", None),
        ("unix:abstract=/a", None),
        ("tcp:host=localhost,port=1", None),
    ];
    for (address, want) in cases {
        assert_eq!(unix_path(address).as_deref(), want, "{address}");
    }
}

/// A socket that is missing or refuses the connection fails as Go's `net.Dial` does; the texts
/// were printed by the Go watcher's `Open`.
#[tokio::test]
async fn dial_failures_read_as_go() {
    let dir = TempDir::new();
    let missing = dir.path().join("missing");
    let refusing = dir.path().join("plainfile");
    std::fs::write(&refusing, b"").unwrap();
    let (log, _) = capture_logs(Level::DEBUG);
    for (path, cause) in [
        (missing, "no such file or directory"),
        (refusing, "connection refused"),
    ] {
        let path = path.to_str().unwrap();
        let error = open_at(&format!("unix:path={path}"), &log)
            .await
            .unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("connect system bus: dial unix {path}: connect: {cause}")
        );
    }
}

/// A D-Bus error reply reads as godbus's: its message alone. The text was printed by the Go
/// watcher's `Open` against a bus where no one owns systemd's name.
#[test]
fn error_replies_read_as_godbus() {
    let reply = zbus::fdo::Error::NameHasNoOwner(
        "Could not get owner of name 'org.freedesktop.systemd1': no such name".to_owned(),
    );
    assert_eq!(
        Error::ResolveOwner(zbus::Error::FDO(Box::new(reply))).to_string(),
        "resolve the owner of org.freedesktop.systemd1: Could not get owner of name \
         'org.freedesktop.systemd1': no such name"
    );
}
