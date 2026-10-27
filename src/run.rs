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

// Adapted from swarm-scheduler-exporter/src/run.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The binary's wiring, Go's `run` and `runLaunch`: the flags, the settings and the startup
//! checks, the logger and the start log, the store, `RLIMIT_MEMLOCK`, the Docker client, the
//! metrics server, the processor, the `SIGHUP` reload and the daemon's run, or the launcher on a
//! leading `launch`.
//!
//! One shutdown budget, [`SHUTDOWN_BUDGET`], runs from the first `SIGINT` or `SIGTERM`: the
//! daemon's run (which waits for its tasks and its in-flight transactions, as Go's `daemon.Run`
//! waits for its goroutines), the metrics server's drain and the runtime's shutdown in `main` all
//! come out of it. Later signals are swallowed, as with Go's `signal.NotifyContext` until `run`
//! returns.

use std::ffi::OsString;
use std::future::Future;
use std::io::{self, Write as _};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use tokio::sync::mpsc;
use tokio::time::Instant;

use crate::cgroup::rlimit;
use crate::cli::settings::{self, Reloader, Settings, list_attr};
use crate::cli::{self, LAUNCH_COMMAND};
use crate::clock::{Clock, SystemClock};
use crate::config::{self, Runtime};
use crate::daemon::{self, DOCKER_CALL_TIMEOUT, Readiness};
use crate::docker::{self, CallContext, Client};
use crate::launcher::{self, spec};
use crate::logger::{Attr, LogHandle};
use crate::metrics::Recorder;
use crate::processor::{self, Processor, devfs};
use crate::server::{self, App, MetricsServer};
use crate::version::{COMMIT, DATE, VERSION};

/// The time from the first shutdown signal to the exit: Docker's stop timeout, which the launcher
/// gives the daemon ([`spec::STOP_TIMEOUT`]).
pub const SHUTDOWN_BUDGET: Duration = spec::STOP_TIMEOUT;

/// The inside-container view of the host root, Go's `hostRootPath`: cgroup paths (and sysfs,
/// when mounted there) are joined against it.
const HOST_ROOT_PATH: &str = "/host";

/// The `component` attribute of the launcher's own log lines, Go's `launcherComponent`.
const LAUNCHER_COMPONENT: &str = "launcher";

/// How [`run_with`] ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Exit {
    /// The process exit code.
    pub code: i32,
    /// The shutdown deadline, once the daemon stopped: what is left of it bounds the runtime's
    /// shutdown in `main`.
    pub deadline: Option<Instant>,
}

impl Exit {
    fn code(code: i32) -> Self {
        Self {
            code,
            deadline: None,
        }
    }
}

/// Writes `text` to stdout or stderr, as Go's `fmt.Fprint` does, ignoring a write error.
fn print(stdout: bool, text: &str) {
    let _ = if stdout {
        io::stdout().write_all(text.as_bytes())
    } else {
        io::stderr().write_all(text.as_bytes())
    };
}

/// Go's `signal.NotifyContext`: `shutdown` registers the signals at once, as Go's call does, and
/// returns what resolves on the first; a root context is cancelled then, and the time it did
/// starts the shutdown budget.
fn watch_shutdown<F>(shutdown: impl FnOnce() -> F) -> (CallContext, Arc<OnceLock<Instant>>)
where
    F: Future<Output = ()> + Send + 'static,
{
    let shutdown = shutdown();
    let root = CallContext::new();
    let signalled = Arc::new(OnceLock::new());
    tokio::spawn({
        let root = root.clone();
        let signalled = Arc::clone(&signalled);
        async move {
            shutdown.await;
            let _ = signalled.set(Instant::now());
            root.cancel();
        }
    });
    (root, signalled)
}

/// Go's `startupSettings`: the config file merged under the flags, the result validated, and the
/// kernel checked, before anything uses them. The error is the line Go printed on stderr.
fn startup_settings(flags: &Settings, parsed: &cli::Parsed) -> Result<(Settings, Runtime), String> {
    let file = config::load_file(&parsed.args.config)
        .map_err(|error| format!("config file error: {error}"))?;
    let effective = Settings::merge(flags, &parsed.set, &file);
    let runtime = effective
        .validate()
        .map_err(|error| format!("invalid config: {error}"))?;
    // Device paths are resolved beneath /dev with openat2; without it no path can be contained,
    // so the daemon refuses to start rather than fall back.
    devfs::probe_openat2().map_err(|error| format!("unsupported kernel: {error}"))?;
    Ok((effective, runtime))
}

/// Go's `run`, on `args` (the program name first) and the environment `getenv`, shutting down
/// when what `shutdown` returns resolves; `shutdown` registers the signals, and is called where Go
/// calls `signal.NotifyContext`. `hangups` registers the `SIGHUP` channel; it is called once the
/// flags are parsed, before anything slow, so a `SIGHUP` during startup is queued for the reload
/// instead of terminating the process. A leading `launch` runs the launcher instead, which takes
/// no `SIGHUP`.
pub async fn run_with<I, T, F>(
    args: I,
    getenv: impl Fn(&str) -> Option<String>,
    shutdown: impl FnOnce() -> F,
    hangups: impl FnOnce() -> mpsc::Receiver<()>,
) -> Exit
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
    F: Future<Output = ()> + Send + 'static,
{
    let args: Vec<OsString> = args.into_iter().map(Into::into).collect();
    if args.get(1).is_some_and(|first| first == LAUNCH_COMMAND) {
        return run_launch(&args[2..], &getenv, shutdown).await;
    }
    let parsed = match cli::parse(&args) {
        Ok(parsed) => parsed,
        Err(exit) => {
            print(exit.stdout, &exit.message);
            return Exit::code(exit.code);
        }
    };
    let hangups = hangups();
    let flags = Settings::from_flags(&parsed);
    let (effective, runtime) = match startup_settings(&flags, &parsed) {
        Ok(settings) => settings,
        Err(message) => {
            print(false, &format!("{message}\n"));
            return Exit::code(1);
        }
    };
    let clock: Arc<dyn Clock> = Arc::new(SystemClock);
    let log = LogHandle::stdout(Arc::clone(&clock), &getenv);
    log.configure(
        &effective.log_format,
        &effective.log_level,
        effective.log_time,
    );
    let (store, publisher) = config::new_store(runtime);
    announce(&log, &parsed.args.config, &store.load());

    let (root, signalled) = watch_shutdown(shutdown);

    // API version negotiation runs lazily, on the first request, as moby's does.
    let docker = match docker::Settings::unix(&effective.docker_socket) {
        Ok(settings) => Arc::new(Client::new(settings)),
        Err(error) => {
            log.error("docker client init failed", &[Attr::error("err", &error)]);
            return Exit::code(1);
        }
    };
    let recorder = Recorder::new();
    let readiness = Readiness::default();

    // Started before the main loop, so the endpoints are reachable during the startup pass.
    let app = App::new(
        readiness.clone(),
        recorder.clone(),
        Arc::clone(&clock),
        log.clone(),
    );
    let Ok(server) = start_server(&root, &effective.metrics_addr, app, &log).await else {
        return Exit::code(1);
    };

    let processor = Processor::new(processor::Settings {
        inspector: Arc::clone(&docker),
        store,
        publisher,
        metrics: recorder.clone(),
        host_root: HOST_ROOT_PATH.to_owned(),
        proc_root: "/".to_owned(),
        call_timeout: Some(DOCKER_CALL_TIMEOUT),
        clock: Arc::clone(&clock),
        log: log.clone(),
    });

    tokio::spawn(settings::watch_sighup(
        root.cancellation(),
        hangups,
        reloader(&parsed, flags, effective, &processor, &root, &log),
    ));

    let run = daemon::run(
        &root,
        daemon::Options {
            docker,
            processor,
            metrics: recorder,
            readiness,
            call_timeout: DOCKER_CALL_TIMEOUT,
            clock,
            log: log.clone(),
        },
    );
    let budget = async {
        root.cancellation().cancelled().await;
        tokio::time::sleep_until(deadline(&signalled)).await;
    };
    tokio::select! {
        () = run => {}
        () = budget => {}
    }
    log.info("swarm-device-access shutting down", &[]);
    let deadline = deadline(&signalled);
    stop_server(server, deadline).await;
    Exit {
        code: 0,
        deadline: Some(deadline),
    }
}

/// The start log, the dry-run warning and the `RLIMIT_MEMLOCK` removal, in Go's order.
fn announce(log: &LogHandle, config_file: &str, current: &Runtime) {
    log.info(
        "swarm-device-access starting",
        &[
            Attr::string("version", VERSION),
            Attr::string("commit", COMMIT),
            Attr::string("date", DATE),
            Attr::string("config_file", config_file),
            Attr::bool("dry_run", current.dry_run),
            Attr::string("policy_mode", current.policy.mode().as_str()),
            list_attr("device_allow", current.policy.device_allow()),
            list_attr("device_deny", current.policy.device_deny()),
        ],
    );
    if current.dry_run {
        // Dry-run has no cgroup view and no saved ownership state, so it cannot even count what a
        // previous live run left behind.
        log.warn(
            "dry-run: grants left by a previous live run cannot be detected or cleaned in dry-run",
            &[],
        );
    }
    if let Err(error) = rlimit::remove_memlock() {
        log.warn(
            "could not remove RLIMIT_MEMLOCK; loading BPF device filters may fail",
            &[Attr::error("err", &error)],
        );
    }
}

/// Go's `startServers` for the one server left: the metrics server when an address is set. A
/// busy or invalid address is logged and fails the startup.
async fn start_server(
    root: &CallContext,
    address: &str,
    app: App,
    log: &LogHandle,
) -> Result<Option<MetricsServer>, ()> {
    if address.is_empty() {
        return Ok(None);
    }
    match server::start_metrics_server(&root.cancellation(), address, app).await {
        Ok(server) => Ok(Some(server)),
        Err(error) => {
            log.error(
                "could not start observability server",
                &[Attr::string(
                    "err",
                    format!("start metrics server: {error}"),
                )],
            );
            Err(())
        }
    }
}

/// The end of the shutdown budget: [`SHUTDOWN_BUDGET`] after the first signal.
fn deadline(signalled: &OnceLock<Instant>) -> Instant {
    signalled.get().copied().unwrap_or_else(Instant::now) + SHUTDOWN_BUDGET
}

/// Stops the metrics server, if one runs, within what is left of the budget.
async fn stop_server(server: Option<MetricsServer>, deadline: Instant) {
    if let Some(server) = server {
        let _ = tokio::time::timeout_at(deadline, server.stop()).await;
    }
}

/// The `SIGHUP` reloader: the config file re-read and merged under the flags, the logger
/// reconfigured, and the runtime published through the processor.
fn reloader(
    parsed: &cli::Parsed,
    flags: Settings,
    current: Settings,
    processor: &Arc<Processor<Arc<Client>>>,
    root: &CallContext,
    log: &LogHandle,
) -> Reloader {
    let configure = log.clone();
    let processor = Arc::clone(processor);
    let context = root.clone();
    Reloader {
        path: parsed.args.config.clone(),
        flags,
        cli_set: parsed.set.clone(),
        current,
        configure_logger: Box::new(move |format, level, include_time| {
            configure.configure(format, level, include_time);
        }),
        publish_and_reconcile: Box::new(move |runtime| {
            let processor = Arc::clone(&processor);
            let context = context.clone();
            Box::pin(async move { processor.publish_and_reconcile(&context, runtime).await })
        }),
        log: log.clone(),
    }
}

/// Go's `runLaunch`: the launcher, and its daemon's exit status, or 1 when the launcher itself
/// fails. It skips everything only the daemon needs: the config file, the openat2 probe,
/// `RLIMIT_MEMLOCK`, `SIGHUP` and the metrics server.
async fn run_launch<F>(
    arguments: &[OsString],
    getenv: &impl Fn(&str) -> Option<String>,
    shutdown: impl FnOnce() -> F,
) -> Exit
where
    F: Future<Output = ()> + Send + 'static,
{
    let launch = match cli::parse_launch(arguments) {
        Ok(launch) => launch,
        Err(exit) => {
            print(exit.stdout, &exit.message);
            return Exit::code(exit.code);
        }
    };
    let log = LogHandle::stdout(Arc::new(SystemClock), getenv);
    log.configure(&launch.args.log_format, &launch.args.log_level, false);
    log.info(
        "swarm-device-access launcher starting",
        &[
            Attr::string("component", LAUNCHER_COMPONENT),
            Attr::string("version", VERSION),
            Attr::string("commit", COMMIT),
            Attr::string("date", DATE),
            Attr::strings("daemon_args", &launch.daemon_args),
        ],
    );
    let (context, _) = watch_shutdown(shutdown);
    let client = match docker::Settings::unix(spec::CONTAINER_DOCKER_SOCKET) {
        Ok(settings) => Client::new(settings),
        Err(error) => {
            log.error(
                "docker client init failed",
                &[
                    Attr::string("component", LAUNCHER_COMPONENT),
                    Attr::error("err", &error),
                ],
            );
            return Exit::code(1);
        }
    };
    let options = launcher::Options {
        host_docker_socket: launch.args.host_docker_socket,
        dbus: launch.args.dbus,
        config_dir: launch.args.config_dir,
        host_network: launch.args.host_network,
        daemon_args: Some(launch.daemon_args),
    };
    match launcher::run(&context, client, options, &log).await {
        // Go's os.Exit(status): the kernel keeps the low byte.
        Ok(status) => Exit::code(i32::try_from(status & 0xff).unwrap_or(1)),
        Err(error) => {
            log.error(
                "launcher failed",
                &[
                    Attr::string("component", LAUNCHER_COMPONENT),
                    Attr::error("err", &error),
                ],
            );
            Exit::code(1)
        }
    }
}

/// Registers `SIGINT` and `SIGTERM` now, and returns what resolves on the first of them. The
/// handlers stay installed for the life of the process, so later signals are swallowed. Signals
/// cannot be watched when the runtime has no signal driver; then it never resolves.
pub fn shutdown_signal() -> impl Future<Output = ()> + Send + 'static {
    use tokio::signal::unix::{SignalKind, signal};
    let registered = (
        signal(SignalKind::interrupt()),
        signal(SignalKind::terminate()),
    );
    async move {
        let (Ok(mut interrupt), Ok(mut terminate)) = registered else {
            return std::future::pending().await;
        };
        tokio::select! {
            _ = interrupt.recv() => {}
            _ = terminate.recv() => {}
        }
    }
}

/// Registers `SIGHUP`, Go's `signal.Notify(sighup, syscall.SIGHUP)`, and forwards each one to a
/// channel that holds one pending signal, as Go's buffered channel does. The handler stays for
/// the life of the process, so a `SIGHUP` after the shutdown started is swallowed. Without a
/// signal driver the channel is closed at once and nothing is reloaded.
#[must_use]
pub fn hangup_channel() -> mpsc::Receiver<()> {
    use tokio::signal::unix::{SignalKind, signal};
    let (sender, receiver) = mpsc::channel(1);
    if let Ok(mut hangup) = signal(SignalKind::hangup()) {
        tokio::spawn(async move {
            while hangup.recv().await.is_some() {
                let _ = sender.try_send(());
            }
        });
    }
    receiver
}

#[cfg(test)]
mod tests;
