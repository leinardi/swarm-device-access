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

//! Watches systemd's D-Bus `Reloading` signal, so the daemon can re-apply cgroup device rules
//! after a daemon-reload clears them. Go's `internal/systemd`.
//!
//! The connection is zbus on the tokio runtime, in godbus's sequence: the system bus address from
//! `DBUS_SYSTEM_BUS_ADDRESS`, else `/var/run/dbus/system_bus_socket`; the authentication and
//! `Hello`; the same two match rules, each with its stream registered as the rule is added, so
//! both are in place before `GetNameOwner`; and no `Manager.Subscribe`, which systemd needs for
//! its unit signals but not for `Reloading`. A `unix:path=` address is dialled here, so the
//! failure every host without the socket logs reads as Go's; zbus's own wording is kept for the
//! rarer failures after the dial (Q8).

use std::io;
use std::pin::Pin;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;
use std::{env, fmt};

use futures_core::Stream;
use tokio::sync::mpsc;
use tokio::task::{JoinHandle, JoinSet};
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use zbus::message::Type;
use zbus::{Connection, DBusError, MatchRule, Message, MessageStream, Structure, Variant};

use crate::go_compat::errno::io_error_text;
use crate::logger::{Attr, LogHandle};

const SYSTEMD_BUS_NAME: &str = "org.freedesktop.systemd1";
const SYSTEMD_OBJECT_PATH: &str = "/org/freedesktop/systemd1";
const RELOADING_INTERFACE: &str = "org.freedesktop.systemd1.Manager";
const RELOADING_MEMBER: &str = "Reloading";
const RELOADING_FULL_NAME: &str = "org.freedesktop.systemd1.Manager.Reloading";

const BUS_DAEMON_NAME: &str = "org.freedesktop.DBus";
const BUS_DAEMON_PATH: &str = "/org/freedesktop/DBus";
const NAME_OWNER_CHANGED_MEMBER: &str = "NameOwnerChanged";
const NAME_OWNER_CHANGED_FULL_NAME: &str = "org.freedesktop.DBus.NameOwnerChanged";

/// godbus's system bus address when `DBUS_SYSTEM_BUS_ADDRESS` is not set.
const DEFAULT_SYSTEM_BUS_ADDRESS: &str = "unix:path=/var/run/dbus/system_bus_socket";

const SIGNAL_CHAN_BUFFER: usize = 16;

/// Bounds one attempt to connect and subscribe: none of the D-Bus calls involved take a deadline
/// of their own.
const SETUP_TIMEOUT: Duration = Duration::from_secs(10);
/// The bounds of the pause between subscription attempts.
const MIN_RETRY: Duration = Duration::from_secs(1);
const MAX_RETRY: Duration = Duration::from_mins(5);

/// A callback the watcher runs on each completed reload; it may block.
pub type OnReload = Arc<dyn Fn() + Send + Sync>;

/// Keeps a watcher subscribed until `cancel` fires, running `on_reload` on each completed systemd
/// reload. Run it on its own task: it never returns before `cancel`. Each attempt to connect and
/// subscribe is bounded by a setup timeout; a failed attempt, or a connection that is lost later
/// (a D-Bus restart), is retried with backoff up to five minutes.
///
/// `on_subscribed` runs after every successful subscription, the first one included: a reload
/// that completed while no watcher was subscribed (during startup, or while the bus was
/// unreachable) was not seen, so the caller must reconcile everything once.
pub async fn supervise(
    cancel: CancellationToken,
    log: LogHandle,
    on_reload: OnReload,
    on_subscribed: impl FnMut(),
) {
    let open_log = log.clone();
    let supervisor = Supervisor {
        open: move || {
            let log = open_log.clone();
            async move { open(&log).await }
        },
        setup_timeout: SETUP_TIMEOUT,
        min_retry: MIN_RETRY,
        max_retry: MAX_RETRY,
        log,
    };
    supervisor.run(&cancel, &on_reload, on_subscribed).await;
}

/// [`supervise`] with its seams: tests replace `open` and shorten the timings.
pub(crate) struct Supervisor<Open> {
    pub(crate) open: Open,
    pub(crate) setup_timeout: Duration,
    pub(crate) min_retry: Duration,
    pub(crate) max_retry: Duration,
    pub(crate) log: LogHandle,
}

impl<Open, Opening> Supervisor<Open>
where
    Open: Fn() -> Opening,
    Opening: Future<Output = Result<Watcher, Error>>,
{
    pub(crate) async fn run(
        &self,
        cancel: &CancellationToken,
        on_reload: &OnReload,
        mut on_subscribed: impl FnMut(),
    ) {
        let mut retry = self.min_retry;
        let mut failures = 0_u32;
        loop {
            let outcome = self
                .attempt(cancel, on_reload, || {
                    if failures > 0 {
                        self.log
                            .info("systemd reload handling available again", &[]);
                    }
                    failures = 0;
                    on_subscribed();
                })
                .await;
            if cancel.is_cancelled() {
                return;
            }
            match outcome {
                Err(error) => {
                    // Hosts without systemd or without the socket mounted fail every attempt;
                    // say so once, then keep trying quietly.
                    let attrs = [Attr::error("err", &error), duration_attr("retry_in", retry)];
                    if failures == 0 {
                        self.log.warn(
                            "systemd reload handling unavailable; retrying in the background",
                            &attrs,
                        );
                    } else {
                        self.log
                            .debug("systemd reload watcher still unavailable", &attrs);
                    }
                    failures += 1;
                }
                Ok(held) => {
                    // A subscription that held for a while starts the backoff over; one that
                    // keeps dropping right away backs off like a failure, so a flapping bus does
                    // not request a pass every minimum retry.
                    if held >= self.max_retry {
                        retry = self.min_retry;
                    }
                    self.log.warn(
                        "systemd reload watcher disconnected; resubscribing",
                        &[duration_attr("retry_in", retry)],
                    );
                }
            }
            tokio::select! {
                () = cancel.cancelled() => return,
                () = tokio::time::sleep(retry) => {}
            }
            retry = retry.saturating_mul(2).min(self.max_retry);
        }
    }

    /// Connects, subscribes and watches until the subscription is lost or `cancel` fires, and
    /// returns how long the subscription held, or the setup failure. Everything the attempt
    /// opened is closed before it returns, so attempts never overlap: an open that times out or
    /// is cancelled is dropped, which closes whatever it had opened.
    async fn attempt(
        &self,
        cancel: &CancellationToken,
        on_reload: &OnReload,
        on_subscribed: impl FnOnce(),
    ) -> Result<Duration, Error> {
        let opened = tokio::select! {
            () = cancel.cancelled() => return Ok(Duration::ZERO),
            opened = tokio::time::timeout(self.setup_timeout, (self.open)()) => opened,
        };
        let mut watcher = opened.map_err(|_| Error::SetupTimeout)??;
        let subscribed_at = Instant::now();
        // Signals are buffered from here on, so none is lost while the caller reconciles.
        on_subscribed();
        watcher.watch(cancel, on_reload, &self.log).await;
        let held = subscribed_at.elapsed();
        if let Err(error) = watcher.close().await {
            self.log
                .warn("close systemd watcher", &[Attr::error("err", &error)]);
        }
        Ok(held)
    }
}

/// A connection subscribed to systemd's `Reloading` signal and to the owner changes of its name.
/// systemctl daemon-reload clears cgroup BPF programs, so any container that had device-allow
/// rules attached loses access; [`Watcher::watch`] runs the callback after each reload completes
/// so the caller can re-apply rules.
#[derive(Debug)]
pub(crate) struct Watcher {
    connection: Option<Connection>,
    /// Fed from the match rules' streams, which were registered before the owner was resolved,
    /// so an owner change that races the lookup is queued rather than dropped. Closed when the
    /// connection is lost.
    signals: mpsc::Receiver<Signal>,
    /// The unique bus name systemd held when [`open`] resolved it.
    owner: String,
    forwarders: JoinSet<()>,
}

impl Watcher {
    /// A watcher on a signal source of the caller's, for tests.
    #[cfg(test)]
    pub(crate) fn from_signals(signals: mpsc::Receiver<Signal>, owner: &str) -> Self {
        Self {
            connection: None,
            signals,
            owner: owner.to_owned(),
            forwarders: JoinSet::new(),
        }
    }

    /// Blocks until `cancel` fires or the subscription is lost, running `on_reload` on each
    /// completed systemd reload (see [`watch`]).
    pub(crate) async fn watch(
        &mut self,
        cancel: &CancellationToken,
        on_reload: &OnReload,
        log: &LogHandle,
    ) {
        watch(
            cancel,
            &mut self.signals,
            self.owner.clone(),
            on_reload,
            log,
        )
        .await;
    }

    /// Releases the connection.
    ///
    /// # Errors
    ///
    /// [`CloseError`] when the connection does not close cleanly.
    pub(crate) async fn close(mut self) -> Result<(), CloseError> {
        self.forwarders.shutdown().await;
        match self.connection.take() {
            Some(connection) => connection.close().await.map_err(CloseError),
            None => Ok(()),
        }
    }
}

/// Connects to the system bus, resolves the unique name that owns `org.freedesktop.systemd1`,
/// and adds the matches for systemd's `Reloading` signal (from systemd's name and object path
/// only) and for the owner changes of that name.
///
/// # Errors
///
/// [`Error`] when D-Bus is unavailable (no socket bind-mount, no systemd, the daemon running off
/// the host). [`supervise`] retries; the daemon runs without reload handling meanwhile.
pub(crate) async fn open(log: &LogHandle) -> Result<Watcher, Error> {
    let address = env::var("DBUS_SYSTEM_BUS_ADDRESS")
        .ok()
        .filter(|address| !address.is_empty())
        .unwrap_or_else(|| DEFAULT_SYSTEM_BUS_ADDRESS.to_owned());
    open_at(&address, log).await
}

/// [`open`] on the bus at `address`.
async fn open_at(address: &str, log: &LogHandle) -> Result<Watcher, Error> {
    let connection = connect(address).await.map_err(Error::Connect)?;
    match subscribe(&connection).await {
        Ok((signals, owner, forwarders)) => Ok(Watcher {
            connection: Some(connection),
            signals,
            owner,
            forwarders,
        }),
        Err(error) => {
            if let Err(close_error) = connection.close().await {
                log.warn(
                    "close dbus conn after subscription failure",
                    &[Attr::error("err", &CloseError(close_error))],
                );
            }
            Err(error)
        }
    }
}

/// The bus connection at `address`, authenticated and named, as godbus's `ConnectSystemBus` makes
/// it.
async fn connect(address: &str) -> Result<Connection, ConnectError> {
    let builder = match unix_path(address) {
        Some(path) => {
            let stream = tokio::net::UnixStream::connect(&path)
                .await
                .and_then(tokio::net::UnixStream::into_std)
                .map_err(|source| ConnectError::Dial { path, source })?;
            zbus::connection::Builder::unix_stream(stream)
        }
        None => zbus::connection::Builder::address(address),
    };
    builder.build().await.map_err(ConnectError::Bus)
}

/// The socket path of a single `unix:path=` address, with D-Bus's `%xx` escapes undone; any other
/// address is left to zbus.
fn unix_path(address: &str) -> Option<String> {
    let escaped = address.strip_prefix("unix:path=")?;
    if escaped.contains([',', ';']) {
        return None;
    }
    let bytes = escaped.as_bytes();
    let mut path = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while let Some(&byte) = bytes.get(index) {
        if byte == b'%' {
            let digits = escaped.get(index + 1..index + 3)?;
            path.push(u8::from_str_radix(digits, 16).ok()?);
            index += 3;
        } else {
            path.push(byte);
            index += 1;
        }
    }
    String::from_utf8(path).ok()
}

/// Adds the matches, with their streams registered, before it resolves systemd's owner: a
/// `NameOwnerChanged` between the lookup and the registration would otherwise be lost and leave
/// the watcher trusting a dead name.
async fn subscribe(
    connection: &Connection,
) -> Result<(mpsc::Receiver<Signal>, String, JoinSet<()>), Error> {
    let reloading = rule(
        SYSTEMD_BUS_NAME,
        SYSTEMD_OBJECT_PATH,
        RELOADING_INTERFACE,
        RELOADING_MEMBER,
        None,
    )
    .map_err(|source| Error::AddMatch {
        signal: RELOADING_FULL_NAME,
        source,
    })?;
    let reloads = MessageStream::for_match_rule(reloading, connection, None)
        .await
        .map_err(|source| Error::AddMatch {
            signal: RELOADING_FULL_NAME,
            source,
        })?;
    let owner_changed = rule(
        BUS_DAEMON_NAME,
        BUS_DAEMON_PATH,
        BUS_DAEMON_NAME,
        NAME_OWNER_CHANGED_MEMBER,
        Some(SYSTEMD_BUS_NAME),
    )
    .map_err(|source| Error::AddMatch {
        signal: NAME_OWNER_CHANGED_FULL_NAME,
        source,
    })?;
    let owners = MessageStream::for_match_rule(owner_changed, connection, None)
        .await
        .map_err(|source| Error::AddMatch {
            signal: NAME_OWNER_CHANGED_FULL_NAME,
            source,
        })?;

    let (sender, signals) = mpsc::channel(SIGNAL_CHAN_BUFFER);
    let mut forwarders = JoinSet::new();
    forwarders.spawn(forward(reloads, sender.clone()));
    forwarders.spawn(forward(owners, sender));

    let reply = connection
        .call_method(
            Some(BUS_DAEMON_NAME),
            BUS_DAEMON_PATH,
            Some(BUS_DAEMON_NAME),
            "GetNameOwner",
            &(SYSTEMD_BUS_NAME,),
        )
        .await
        .map_err(Error::ResolveOwner)?;
    let owner: String = reply.body().deserialize().map_err(Error::ResolveOwner)?;
    Ok((signals, owner, forwarders))
}

/// A signal match rule; `arg0` narrows it to one value of the first argument.
fn rule(
    sender: &'static str,
    path: &'static str,
    interface: &'static str,
    member: &'static str,
    arg0: Option<&'static str>,
) -> zbus::Result<MatchRule<'static>> {
    let builder = MatchRule::builder()
        .msg_type(Type::Signal)
        .sender(sender)
        .path(path)
        .interface(interface)
        .member(member);
    match arg0 {
        Some(arg0) => builder.add_arg(arg0).build(),
        None => builder.build(),
    }
}

/// Feeds the stream's signals into the watcher's channel until the stream ends or fails: a read
/// error is a lost connection, as godbus closes its signal channels.
async fn forward(mut stream: MessageStream, sender: mpsc::Sender<Signal>) {
    loop {
        let next = std::future::poll_fn(|context| Pin::new(&mut stream).poll_next(context)).await;
        let Some(Ok(message)) = next else {
            return;
        };
        if sender.send(Signal::from_message(&message)).await.is_err() {
            return;
        }
    }
}

/// A signal as the watcher reads it: godbus's `dbus.Signal`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Signal {
    pub(crate) sender: String,
    pub(crate) path: String,
    /// The interface and the member, joined by a dot.
    pub(crate) name: String,
    pub(crate) body: Vec<Arg>,
}

/// One argument of a signal, as far as the watcher reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Arg {
    Bool(bool),
    /// A D-Bus string; an object path or a signature is [`Arg::Other`], as godbus types them.
    Str(String),
    Other,
}

impl Signal {
    fn from_message(message: &Message) -> Self {
        let header = message.header();
        let text =
            |value: Option<&dyn fmt::Display>| value.map(ToString::to_string).unwrap_or_default();
        let name = format!(
            "{}.{}",
            text(
                header
                    .interface()
                    .map(|interface| interface as &dyn fmt::Display)
            ),
            text(header.member().map(|member| member as &dyn fmt::Display))
        );
        let body = message
            .body()
            .deserialize::<Structure<'_>>()
            .map(|structure| structure.fields().iter().map(Arg::from_variant).collect())
            .unwrap_or_default();
        Self {
            sender: text(header.sender().map(|sender| sender as &dyn fmt::Display)),
            path: text(header.path().map(|path| path as &dyn fmt::Display)),
            name,
            body,
        }
    }
}

impl Arg {
    fn from_variant(variant: &Variant<'_>) -> Self {
        // An if-let chain, not a match: every other kind of value, today's and any zbus adds, is
        // one this watcher does not read.
        if let Variant::Bool(value) = variant {
            Self::Bool(*value)
        } else if let Variant::Str(value) = variant {
            Self::Str(value.as_str().to_owned())
        } else {
            Self::Other
        }
    }
}

/// Consumes signals until `cancel` fires or the channel closes. The `Reloading` signal fires
/// twice per reload: once with `active=true` when the reload starts and once with `active=false`
/// when it finishes. Only the completion edge counts (re-applying mid-reload races the cgroup
/// wipe), and only from systemd: the signal must come from `owner`, the unique name of
/// `org.freedesktop.systemd1`, and from systemd's object path. Any other client on the bus can
/// emit a signal with the same name, so the match rules alone are not trusted. `owner` follows
/// `NameOwnerChanged` from the bus daemon, so a systemd re-exec does not silence the watcher.
///
/// `on_reload` runs on its own thread and is coalesced: a reload that completes while it runs is
/// absorbed into one more run after it finishes. It returns only after the last run has
/// finished.
pub(crate) async fn watch(
    cancel: &CancellationToken,
    signals: &mut mpsc::Receiver<Signal>,
    mut owner: String,
    on_reload: &OnReload,
    log: &LogHandle,
) {
    let mut runs = Coalescer::new(Arc::clone(on_reload));
    log.debug(
        "systemd reload watcher started",
        &[Attr::string("systemd", owner.as_str())],
    );
    loop {
        let signal = tokio::select! {
            () = cancel.cancelled() => break,
            signal = signals.recv() => signal,
        };
        let Some(signal) = signal else {
            log.debug("systemd signal channel closed", &[]);
            break;
        };
        if let Some(new_owner) = owner_change(&signal) {
            log.debug(
                "systemd bus name changed owner",
                &[
                    Attr::string("old", owner.as_str()),
                    Attr::string("new", new_owner.as_str()),
                ],
            );
            owner = new_owner;
            continue;
        }
        if !from_systemd(&signal, &owner) || !is_reload_completed(&signal) {
            continue;
        }
        log.info("systemd reload completed; re-applying device rules", &[]);
        runs.trigger();
    }
    runs.wait().await;
}

/// Whether `signal` was sent by `owner` from systemd's object path. An empty owner (systemd not
/// on the bus) matches nothing.
fn from_systemd(signal: &Signal, owner: &str) -> bool {
    !owner.is_empty() && signal.sender == owner && signal.path == SYSTEMD_OBJECT_PATH
}

/// The new owner of `org.freedesktop.systemd1` when `signal` is the bus daemon's
/// `NameOwnerChanged` for it.
fn owner_change(signal: &Signal) -> Option<String> {
    if signal.name != NAME_OWNER_CHANGED_FULL_NAME
        || signal.sender != BUS_DAEMON_NAME
        || signal.path != BUS_DAEMON_PATH
    {
        return None;
    }
    let [Arg::Str(name), _, Arg::Str(new_owner)] = signal.body.as_slice() else {
        return None;
    };
    (name == SYSTEMD_BUS_NAME).then(|| new_owner.clone())
}

/// Whether `signal` is a `Reloading` completion edge: the name matches and the first argument is
/// `false`. The start edge, other signals and malformed payloads are not.
pub(crate) fn is_reload_completed(signal: &Signal) -> bool {
    signal.name == RELOADING_FULL_NAME && matches!(signal.body.first(), Some(Arg::Bool(false)))
}

/// Runs a function on its own thread, one run at a time: triggers that arrive during a run
/// collapse into one run after it.
struct Coalescer {
    run: OnReload,
    state: Arc<Mutex<CoalescerState>>,
    task: Option<JoinHandle<()>>,
}

#[derive(Default)]
struct CoalescerState {
    running: bool,
    pending: bool,
}

impl Coalescer {
    fn new(run: OnReload) -> Self {
        Self {
            run,
            state: Arc::default(),
            task: None,
        }
    }

    fn trigger(&mut self) {
        {
            let mut state = lock(&self.state);
            if state.running {
                state.pending = true;
                return;
            }
            state.running = true;
        }
        let run = Arc::clone(&self.run);
        let state = Arc::clone(&self.state);
        // The previous task, if any, has cleared `running`: it makes no further run.
        self.task = Some(tokio::task::spawn_blocking(move || {
            loop {
                run();
                let mut state = lock(&state);
                if !state.pending {
                    state.running = false;
                    return;
                }
                state.pending = false;
            }
        }));
    }

    /// Waits until no run is in progress.
    async fn wait(&mut self) {
        if let Some(task) = self.task.take() {
            // A run that panicked has ended too; under panic = "abort" it cannot happen.
            let _ = task.await;
        }
    }
}

fn lock(state: &Mutex<CoalescerState>) -> std::sync::MutexGuard<'_, CoalescerState> {
    // A poisoned lock is only reachable in tests (panic = "abort"); both flags are plain booleans,
    // consistent either way.
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

fn duration_attr(key: &str, duration: Duration) -> Attr {
    Attr::duration(key, i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX))
}

/// Why a subscription attempt failed, worded as the Go watcher's errors.
#[derive(Debug)]
pub enum Error {
    /// `connect system bus: <cause>`.
    Connect(ConnectError),
    /// `add match signal <signal>: <cause>`.
    AddMatch {
        signal: &'static str,
        source: zbus::Error,
    },
    /// `resolve the owner of org.freedesktop.systemd1: <cause>`.
    ResolveOwner(zbus::Error),
    /// The attempt did not finish within the setup timeout.
    SetupTimeout,
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connect(source) => write!(formatter, "connect system bus: {source}"),
            Self::AddMatch { signal, source } => {
                write!(
                    formatter,
                    "add match signal {signal}: {}",
                    bus_error_text(source)
                )
            }
            Self::ResolveOwner(source) => write!(
                formatter,
                "resolve the owner of {SYSTEMD_BUS_NAME}: {}",
                bus_error_text(source)
            ),
            Self::SetupTimeout => formatter.write_str("connecting to the system bus timed out"),
        }
    }
}

impl std::error::Error for Error {}

/// Why the connection to the system bus failed.
#[derive(Debug)]
pub enum ConnectError {
    /// The socket did not connect: `dial unix <path>: connect: <errno>`, as Go's `net.Dial`.
    Dial { path: String, source: io::Error },
    /// The authentication, `Hello` or a non-`unix:path=` address failed, in zbus's words.
    Bus(zbus::Error),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Dial { path, source } => write!(
                formatter,
                "dial unix {path}: connect: {}",
                io_error_text(source)
            ),
            Self::Bus(source) => formatter.write_str(&bus_error_text(source)),
        }
    }
}

/// The connection did not close cleanly: `close dbus conn: <cause>`.
#[derive(Debug)]
pub struct CloseError(zbus::Error);

impl fmt::Display for CloseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "close dbus conn: {}", bus_error_text(&self.0))
    }
}

/// A zbus error as godbus words it where they meet: a D-Bus error reply is its message (godbus's
/// `Error.Error()` returns the first body string, else the error name), whether zbus keeps it as
/// a method error or as one of the bus's own; anything else is zbus's own text.
fn bus_error_text(error: &zbus::Error) -> String {
    // An if-let chain, not a match: zbus::Error is non-exhaustive, and every other variant keeps
    // its own text.
    if let zbus::Error::MethodError(name, description, _) = error {
        description
            .clone()
            .unwrap_or_else(|| name.as_str().to_owned())
    } else if let zbus::Error::FDO(reply) = error {
        reply_text(reply.as_ref())
    } else if let zbus::Error::DBus(reply) = error {
        reply_text(reply.as_ref())
    } else {
        error.to_string()
    }
}

/// A D-Bus error reply's message, else its name.
fn reply_text(reply: &dyn DBusError) -> String {
    reply
        .description()
        .map_or_else(|| reply.name().as_str().to_owned(), str::to_owned)
}

#[cfg(test)]
mod tests;
