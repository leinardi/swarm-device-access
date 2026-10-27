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

//! End-to-end parity of the binary with the Go 1.x release (`v1.0.3`): the command line's exit
//! codes and texts, and one daemon run on a scripted Engine (startup, a `SIGHUP` reload that
//! applies, one that is rejected, a scrape and a `SIGTERM`), compared by its log lines, the
//! requests it sent the Engine and the `sda_` families it exposed.
//!
//! The Go side is in `tests/testdata/parity`, written by [`capture_go_parity`] from the Go binary
//! named by `SDA_GO_BINARY` (an ignored test: `make` never runs it). Each run masks what differs
//! between any two runs (the version stamps, the temporary directory, the metrics port, the
//! events `since`). The log lines and requests are compared as sorted lists, since the daemon's
//! tasks interleave them differently from run to run; the startup lines up to the metrics server
//! and the last line, and the requests up to the first container list, are compared by position
//! as well. Two kinds of line depend on the host or the timing and are left out of the lists:
//! the systemd watcher's retries, and the `RLIMIT_MEMLOCK` warning, which only a host that
//! refuses both the probe's map and a raised limit logs; when the run logs it, it is checked to
//! carry one of cilium's errors.

use std::convert::Infallible;
use std::fmt::Write as _;
use std::io::{BufRead as _, BufReader, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::task::{Context, Poll};
use std::time::Duration;

use tokio::time::Instant;

use bytes::Bytes;
use http_body_util::Full;
use hyper::body::{Body, Frame, Incoming, SizeHint};
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;

/// The Rust binary.
const RUST_BINARY: &str = env!("CARGO_BIN_EXE_swarm-device-access");

/// How long the run waits for a log line or the exit.
const WAIT: Duration = Duration::from_secs(10);

/// The container the list returns, and the one whose start event the stream delivers.
const LISTED: &str = "1111111111111111111111111111111111111111111111111111111111111111";
const STARTED: &str = "2222222222222222222222222222222222222222222222222222222222222222";

/// The startup config, the reload that applies (with a restart-only change) and the one that is
/// rejected.
const CONFIG_START: &str = "policy-mode: all\ndevice-deny:\n  - /dev/sda\n";
const CONFIG_RELOAD: &str = "policy-mode: opt-in\nlog-level: info\nmetrics-addr: 127.0.0.1:1\n";
const CONFIG_REJECTED: &str = "log-level: loud\n";

fn testdata(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/parity")
        .join(name)
}

/// The arguments in the binary's spelling: Go's single dash, or the Rust double dash (Q1).
fn spelled(args: &[&str], go: bool) -> Vec<String> {
    args.iter()
        .map(|arg| match arg.strip_prefix("--") {
            Some(flag) if go => format!("-{flag}"),
            _ => (*arg).to_owned(),
        })
        .collect()
}

/// A fresh temporary directory.
fn temp_dir() -> PathBuf {
    static COUNT: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "sda-parity-{}-{}",
        std::process::id(),
        COUNT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir)
        .unwrap_or_else(|error| panic!("create {}: {error}", dir.display()));
    dir
}

// ---------------------------------------------------------------------------------------------
// The command line.

/// How a command-line case compares with Go's answer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Identical: exit code, stdout and stderr.
    Same,
    /// Q1: the launcher names a flag by its double-dash spelling, and what Go's flag package
    /// refused after parsing (exit 1) is clap's usage error (exit 2).
    Q1,
    /// Q6: a YAML syntax error is refused alike, with the same text up to `yaml: `.
    Q6Syntax,
    /// Q6: saphyr-parser refuses a tab between `:` and a plain value, which yaml.v3 reads as
    /// separation; the config is refused (exit 1) where Go went on to its next check.
    Q6Tab,
}

/// The command-line cases, in the Rust spelling, run in the repository root.
const CLI_CASES: &[(&[&str], Kind)] = &[
    (&["--log-format=xml", "--policy-mode=bogus"], Kind::Same),
    (&["--log-format=xml"], Kind::Same),
    (&["--policy-mode=bogus"], Kind::Same),
    (&["--device-allow=relative"], Kind::Same),
    (&["--device-allow=/dev/["], Kind::Same),
    (
        &["--device-deny=/etc/x", "--device-allow=/dev/["],
        Kind::Same,
    ),
    (&["--log-level=loud"], Kind::Same),
    (&["--config=/nonexistent"], Kind::Same),
    (
        &["--config=tests/testdata/parity/unknown-key.conf"],
        Kind::Same,
    ),
    (
        &["--config=tests/testdata/parity/bad-syntax.conf"],
        Kind::Q6Syntax,
    ),
    (
        &[
            "--config=tests/testdata/parity/valid.conf",
            "--log-format=xml",
        ],
        Kind::Same,
    ),
    (
        &[
            "--config=tests/testdata/parity/tab-value.conf",
            "--log-format=xml",
        ],
        Kind::Q6Tab,
    ),
    (&["launch", "--config-dir=rel"], Kind::Q1),
    (&["launch", "--config-dir=/srv/../etc"], Kind::Q1),
    (&["launch", "--host-docker-socket="], Kind::Q1),
    (&["launch", "--log-format=xml"], Kind::Same),
    (
        &["launch", "--log-level=trace", "--log-format=xml"],
        Kind::Same,
    ),
    (&["launch", "extra"], Kind::Q1),
    (&["launch", "--dry-run"], Kind::Q1),
];

/// One case's exit code and output, as recorded.
fn cli_outcome(binary: &str, args: &[String]) -> String {
    let output = Command::new(binary)
        .args(args)
        .env_clear()
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .stdin(Stdio::null())
        .output()
        .unwrap_or_else(|error| panic!("run {binary}: {error}"));
    format!(
        "### {}\nexit={:?}\n--- stdout\n{}--- stderr\n{}",
        args.join(" "),
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Every case of [`CLI_CASES`] run by `binary`.
fn cli_outcomes(binary: &str, go: bool) -> Vec<String> {
    CLI_CASES
        .iter()
        .map(|(args, _)| cli_outcome(binary, &spelled(args, go)))
        .collect()
}

/// The Go outcome with its arguments respelled, and the flag names in the launcher's path errors
/// (Q1), so it compares with the Rust one.
fn as_rust(go: &str, args: &[&str]) -> String {
    let (_, rest) = go.split_once('\n').unwrap_or_default();
    format!("### {}\n{rest}", args.join(" "))
}

/// The command line: every case Go and Rust answer alike is identical, exit code and texts; the
/// Q1 and Q6 cases differ only as those entries say.
#[test]
fn the_command_line_answers_as_go_s() {
    let golden = std::fs::read_to_string(testdata("go-cli.txt"))
        .unwrap_or_else(|error| panic!("read go-cli.txt: {error}"));
    let go: Vec<&str> = golden.split_inclusive("\n#END\n").collect();
    assert_eq!(
        go.len(),
        CLI_CASES.len(),
        "regenerate with capture_go_parity"
    );
    for ((args, kind), (go, rust)) in CLI_CASES
        .iter()
        .zip(go.iter().zip(cli_outcomes(RUST_BINARY, false)))
    {
        let go = as_rust(go.trim_end_matches("#END\n"), args);
        match kind {
            Kind::Same => {
                assert_eq!(rust, go, "{args:?}");
                continue;
            }
            Kind::Q6Syntax => {
                let prefix =
                    |text: &str| text.split_once("yaml: ").map(|(head, _)| head.to_owned());
                assert_eq!(prefix(&rust), prefix(&go), "{args:?}");
                assert!(prefix(&rust).is_some(), "{rust}");
                continue;
            }
            Kind::Q6Tab => {
                assert!(
                    go.contains("exit=Some(1)") && go.contains("invalid config"),
                    "{go}"
                );
                assert!(rust.contains("exit=Some(1)"), "{rust}");
                assert!(
                    rust.contains(
                        "config file error: parse config file \"tests/testdata/parity/\
                         tab-value.conf\": yaml: line 1: ':' must be followed by a valid YAML \
                         whitespace"
                    ),
                    "{rust}"
                );
                continue;
            }
            Kind::Q1 => {}
        }
        let go_named = go
            .replace("\"-config-dir\"", "\"--config-dir\"")
            .replace("\"-host-docker-socket\"", "\"--host-docker-socket\"");
        if go_named.contains("must be an absolute, clean path") {
            assert_eq!(rust, go_named, "{args:?}");
        } else {
            assert!(rust.contains("exit=Some(2)"), "{args:?}: {rust}");
            assert!(
                rust.contains("error: unexpected argument"),
                "{args:?}: {rust}"
            );
            assert!(go.contains("exit=Some(1)"), "{args:?}: {go}");
        }
    }
}

// ---------------------------------------------------------------------------------------------
// The scripted Engine.

/// An event stream: one start event, then nothing until the client goes away.
struct Events(Option<Bytes>);

impl Body for Events {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.0.take() {
            Some(event) => Poll::Ready(Some(Ok(Frame::data(event)))),
            None => Poll::Pending,
        }
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

/// Either a whole answer or the event stream.
enum Answer {
    Full(Full<Bytes>),
    Events(Events),
}

impl Body for Answer {
    type Data = Bytes;
    type Error = Infallible;

    fn poll_frame(
        self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Infallible>>> {
        match self.get_mut() {
            Self::Full(full) => Pin::new(full).poll_frame(context),
            Self::Events(events) => Pin::new(events).poll_frame(context),
        }
    }

    fn is_end_stream(&self) -> bool {
        match self {
            Self::Full(full) => full.is_end_stream(),
            Self::Events(events) => events.is_end_stream(),
        }
    }

    fn size_hint(&self) -> SizeHint {
        match self {
            Self::Full(full) => full.size_hint(),
            Self::Events(events) => events.size_hint(),
        }
    }
}

fn inspect(id: &str, name: &str) -> String {
    format!(
        "{{\"Id\":\"{id}\",\"Name\":\"/{name}\",\"Image\":\"sha256:{id}\",\
         \"State\":{{\"Status\":\"running\",\"Running\":true,\"Pid\":1,\
         \"StartedAt\":\"2026-10-27T10:00:00.000000000Z\"}},\
         \"Config\":{{\"Labels\":{{\"swarm-device-access.enable\":\"true\"}}}},\
         \"HostConfig\":{{}},\"Mounts\":[]}}"
    )
}

/// The `since` of an events request, masked.
fn masked_uri(uri: &str) -> String {
    match uri.split_once("since=") {
        Some((head, tail)) => {
            let rest = tail.find('&').map_or("", |end| &tail[end..]);
            format!("{head}since=<since>{rest}")
        }
        None => uri.to_owned(),
    }
}

/// Answers one request and records it.
fn answer(request: &Request<Incoming>, log: &Mutex<Vec<String>>) -> Response<Answer> {
    let uri = request.uri().to_string();
    let agent = request
        .headers()
        .get("user-agent")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default();
    log.lock()
        .unwrap_or_else(PoisonError::into_inner)
        .push(format!(
            "{} {} ua={}",
            request.method(),
            masked_uri(&uri),
            // Without the build's architecture (Go's GOARCH name in both clients), so the
            // captures hold on any host.
            agent.rsplit_once('/').map_or(agent, |(head, _)| head)
        ));
    let path = request.uri().path();
    let (content_type, body) = if path == "/_ping" {
        (
            "text/plain; charset=utf-8",
            Answer::Full(Full::new(Bytes::from_static(b"OK"))),
        )
    } else if path.ends_with("/events") {
        let event = format!(
            "{{\"status\":\"start\",\"id\":\"{STARTED}\",\"from\":\"busybox\",\
             \"Type\":\"container\",\"Action\":\"start\",\"Actor\":{{\"ID\":\"{STARTED}\",\
             \"Attributes\":{{}}}},\"scope\":\"local\",\"time\":1793000000,\
             \"timeNano\":1793000000000000000}}\n"
        );
        (
            "application/json",
            Answer::Events(Events(Some(Bytes::from(event)))),
        )
    } else if path.ends_with("/containers/json") {
        let list = format!(
            "[{{\"Id\":\"{LISTED}\",\"Names\":[\"/parity-listed\"],\"State\":\"running\",\
             \"Labels\":{{\"swarm-device-access.enable\":\"true\"}}}}]"
        );
        (
            "application/json",
            Answer::Full(Full::new(Bytes::from(list))),
        )
    } else if path.contains(LISTED) {
        let body = inspect(LISTED, "parity-listed");
        (
            "application/json",
            Answer::Full(Full::new(Bytes::from(body))),
        )
    } else if path.contains(STARTED) {
        let body = inspect(STARTED, "parity-started");
        (
            "application/json",
            Answer::Full(Full::new(Bytes::from(body))),
        )
    } else {
        let body = Bytes::from_static(b"{\"message\":\"not found\"}");
        return Response::builder()
            .status(404)
            .header("Api-Version", "1.56")
            .header("Content-Type", "application/json")
            .body(Answer::Full(Full::new(body)))
            .unwrap_or_else(|error| panic!("response: {error}"));
    };
    Response::builder()
        .header("Api-Version", "1.56")
        .header("Content-Type", content_type)
        .body(body)
        .unwrap_or_else(|error| panic!("response: {error}"))
}

/// The scripted Engine on `socket`, on a runtime of its own, recording every request.
fn start_engine(socket: &Path) -> (tokio::runtime::Runtime, Arc<Mutex<Vec<String>>>) {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
        .unwrap_or_else(|error| panic!("runtime: {error}"));
    let log = Arc::new(Mutex::new(Vec::new()));
    let listener = runtime
        .block_on(async { tokio::net::UnixListener::bind(socket) })
        .unwrap_or_else(|error| panic!("bind {}: {error}", socket.display()));
    let served = Arc::clone(&log);
    runtime.spawn(async move {
        while let Ok((stream, _)) = listener.accept().await {
            let log = Arc::clone(&served);
            tokio::spawn(async move {
                let service = hyper::service::service_fn(move |request: Request<Incoming>| {
                    let response = answer(&request, &log);
                    async move { Ok::<_, Infallible>(response) }
                });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    (runtime, log)
}

// ---------------------------------------------------------------------------------------------
// The daemon run.

/// What one run produced, masked.
#[derive(Debug, Default)]
struct Run {
    logs: Vec<String>,
    memlock: Vec<String>,
    requests: Vec<String>,
    metrics: Vec<String>,
    exit: Option<i32>,
}

/// The daemon's output lines as they come.
struct Output {
    lines: Arc<Mutex<Vec<String>>>,
}

impl Output {
    fn follow(child: &mut Child) -> Self {
        let lines = Arc::new(Mutex::new(Vec::new()));
        for stream in [
            child
                .stdout
                .take()
                .map(|out| Box::new(out) as Box<dyn std::io::Read + Send>),
            child
                .stderr
                .take()
                .map(|err| Box::new(err) as Box<dyn std::io::Read + Send>),
        ]
        .into_iter()
        .flatten()
        {
            let lines = Arc::clone(&lines);
            std::thread::spawn(move || {
                for line in BufReader::new(stream).lines().map_while(Result::ok) {
                    lines
                        .lock()
                        .unwrap_or_else(PoisonError::into_inner)
                        .push(line);
                }
            });
        }
        Self { lines }
    }

    fn snapshot(&self) -> Vec<String> {
        self.lines
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Waits until `count` lines contain `text`.
    fn wait_for(&self, text: &str, count: usize) {
        let start = Instant::now();
        while self
            .snapshot()
            .iter()
            .filter(|line| line.contains(text))
            .count()
            < count
        {
            assert!(
                start.elapsed() < WAIT,
                "never logged {text:?} {count} times:\n{}",
                self.snapshot().join("\n")
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

fn signal(child: &Child, name: &str) {
    let sent = Command::new("kill")
        .args([name, &child.id().to_string()])
        .status()
        .unwrap_or_else(|error| panic!("kill: {error}"));
    assert!(sent.success(), "kill {name}");
}

/// The body of a raw HTTP/1.1 answer, its chunked framing removed.
fn body_of(answer: &str) -> String {
    let (head, mut rest) = answer.split_once("\r\n\r\n").unwrap_or_default();
    if !head.contains("Transfer-Encoding: chunked") {
        return rest.to_owned();
    }
    let mut body = String::new();
    while let Some((size, tail)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size.trim(), 16).unwrap_or_default();
        if size == 0 || tail.len() < size {
            break;
        }
        body.push_str(&tail[..size]);
        rest = tail[size..].trim_start_matches("\r\n");
    }
    body
}

/// `GET path` on the metrics server, the answer's body.
fn get(port: u16, path: &str) -> String {
    let mut stream =
        TcpStream::connect(("127.0.0.1", port)).unwrap_or_else(|error| panic!("connect: {error}"));
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n"
    )
    .unwrap_or_else(|error| panic!("write: {error}"));
    let mut answer = String::new();
    stream
        .read_to_string(&mut answer)
        .unwrap_or_else(|error| panic!("read: {error}"));
    body_of(&answer)
}

/// Masks what differs between two runs of the same binary.
fn mask(line: &str, dir: &Path, port: u16) -> String {
    let mut masked = line
        .replace(&dir.display().to_string(), "<dir>")
        .replace(&format!(":{port}"), ":<port>");
    for key in ["version", "commit", "date"] {
        let marker = format!(" {key}=");
        if let Some(start) = masked.find(&marker) {
            let value_start = start + marker.len();
            let end = masked[value_start..]
                .find(' ')
                .map_or(masked.len(), |end| value_start + end);
            masked.replace_range(value_start..end, "<stamp>");
        }
    }
    masked
}

/// The samples whose value is a time: how long the dry-run passes took, and when the last
/// container was processed.
const TIMED_SAMPLES: [&str; 2] = [
    "sda_apply_duration_seconds_sum ",
    "sda_last_event_timestamp_seconds ",
];

/// The `sda_` families of a scrape, values included but those of [`TIMED_SAMPLES`], which are
/// masked once checked to be positive numbers.
fn sda_families(scrape: &str) -> Vec<String> {
    scrape
        .lines()
        .filter(|line| {
            line.starts_with("sda_")
                || line.starts_with("# HELP sda_")
                || line.starts_with("# TYPE sda_")
        })
        .map(|line| {
            let timed = TIMED_SAMPLES
                .iter()
                .find_map(|name| line.strip_prefix(name).map(|value| (name, value)));
            match timed {
                Some((name, value)) => {
                    let value: f64 = value
                        .parse()
                        .unwrap_or_else(|error| panic!("{line}: {error}"));
                    assert!(value > 0.0, "{line}");
                    format!("{name}<time>")
                }
                None => line.to_owned(),
            }
        })
        .collect()
}

/// The systemd watcher's retries, whose count depends on how long the run took.
const RETRY_LINE: &str = "systemd reload watcher still unavailable";

/// The `RLIMIT_MEMLOCK` warning, which depends on the host's privileges and limits.
const MEMLOCK_LINE: &str = "level=WARN msg=\"could not remove RLIMIT_MEMLOCK; loading BPF device \
                            filters may fail\" err=";

/// The beginnings of cilium's `RemoveMemlock` errors.
const MEMLOCK_ERRORS: [&str; 4] = [
    "\"failed to set memlock rlimit: ",
    "\"unexpected error detecting memory cgroup accounting: ",
    "\"lowering memlock rlimit: ",
    "\"restoring old memlock rlimit: ",
];

/// The startup lines Go writes in a fixed order, up to the metrics server's.
fn startup(logs: &[String]) -> &[String] {
    let end = logs
        .iter()
        .position(|line| line.contains("metrics server listening"))
        .map_or(0, |index| index + 1);
    &logs[..end]
}

/// The requests Go sends in a fixed order: the pings, the subscription and the first list.
const ORDERED_REQUESTS: usize = 4;

/// The scenario, run by `binary` (`go` for Go's single-dash flags).
fn run_daemon(binary: &str, go: bool) -> Run {
    let dir = temp_dir();
    let socket = dir.join("docker.sock");
    let config = dir.join("config.yaml");
    let write_config = |text: &str| {
        std::fs::write(&config, text).unwrap_or_else(|error| panic!("write config: {error}"));
    };
    write_config(CONFIG_START);
    let (engine, requests) = start_engine(&socket);
    let port = TcpListener::bind("127.0.0.1:0")
        .and_then(|listener| listener.local_addr())
        .map_or_else(
            |error| panic!("free port: {error}"),
            |address| address.port(),
        );
    let args = spelled(
        &[
            "--dry-run",
            "--log-level=debug",
            &format!("--docker-socket={}", socket.display()),
            &format!("--config={}", config.display()),
            &format!("--metrics-addr=127.0.0.1:{port}"),
        ],
        go,
    );
    let mut child = Command::new(binary)
        .args(&args)
        .env_clear()
        .env("TZ", "UTC")
        .env(
            "DBUS_SYSTEM_BUS_ADDRESS",
            format!("unix:path={}", dir.join("no-bus").display()),
        )
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|error| panic!("spawn {binary}: {error}"));
    let output = Output::follow(&mut child);

    output.wait_for(STARTED, 1);
    output.wait_for("systemd reload handling unavailable", 1);
    // Negative window: the startup pass and the start event settle before the reload.
    std::thread::sleep(Duration::from_millis(500));
    write_config(CONFIG_RELOAD);
    signal(&child, "-HUP");
    output.wait_for("config reloaded", 1);
    std::thread::sleep(Duration::from_millis(500));
    write_config(CONFIG_REJECTED);
    signal(&child, "-HUP");
    output.wait_for("config reload rejected", 1);
    let scrape = get(port, "/metrics");
    signal(&child, "-TERM");
    let start = Instant::now();
    let exit = loop {
        if let Some(status) = child
            .try_wait()
            .unwrap_or_else(|error| panic!("wait: {error}"))
        {
            break status.code();
        }
        assert!(start.elapsed() < WAIT, "the daemon did not exit");
        std::thread::sleep(Duration::from_millis(20));
    };
    output.wait_for("shutting down", 1);
    // Negative window: the readers drain what the daemon wrote before it exited.
    std::thread::sleep(Duration::from_millis(200));
    drop(engine);
    let _ = std::fs::remove_dir_all(&dir);
    let requests = requests
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    let (memlock, logs): (Vec<String>, Vec<String>) = output
        .snapshot()
        .iter()
        .filter(|line| !line.contains(RETRY_LINE))
        .map(|line| mask(line, &dir, port))
        .partition(|line| line.starts_with(MEMLOCK_LINE));
    Run {
        logs,
        memlock,
        requests: requests.iter().map(|line| mask(line, &dir, port)).collect(),
        metrics: sda_families(&scrape),
        exit,
    }
}

fn sorted(lines: &[String]) -> Vec<String> {
    let mut sorted = lines.to_vec();
    sorted.sort();
    sorted
}

fn read_lines(name: &str) -> Vec<String> {
    std::fs::read_to_string(testdata(name))
        .unwrap_or_else(|error| panic!("read {name}: {error}; regenerate with capture_go_parity"))
        .lines()
        .map(str::to_owned)
        .collect()
}

/// One daemon run on the scripted Engine logs the lines Go's did, sends the requests Go's sent
/// and exposes the `sda_` families Go's exposed, and exits 0.
#[test]
fn a_daemon_run_matches_go_s() {
    let rust = run_daemon(RUST_BINARY, false);
    let go_logs = read_lines("go-logs.txt");
    assert_eq!(rust.exit, Some(0), "{}", rust.logs.join("\n"));
    assert!(!startup(&go_logs).is_empty());
    assert_eq!(startup(&rust.logs), startup(&go_logs), "the startup lines");
    assert_eq!(rust.logs.last(), go_logs.last(), "the last line");
    assert_eq!(sorted(&rust.logs), sorted(&go_logs), "the log lines");
    assert!(rust.memlock.len() <= 1, "{:?}", rust.memlock);
    for line in &rust.memlock {
        let err = &line[MEMLOCK_LINE.len()..];
        assert!(
            MEMLOCK_ERRORS.iter().any(|prefix| err.starts_with(prefix)),
            "{line}"
        );
    }
    let go_requests = read_lines("go-requests.txt");
    assert_eq!(
        rust.requests.get(..ORDERED_REQUESTS),
        go_requests.get(..ORDERED_REQUESTS),
        "the Engine requests' order"
    );
    assert_eq!(
        sorted(&rust.requests),
        sorted(&go_requests),
        "the Engine requests"
    );
    assert_eq!(
        rust.metrics,
        read_lines("go-metrics.txt"),
        "the sda_ families"
    );
}

/// Writes the Go side from the binary `SDA_GO_BINARY` names, the `v1.0.3` release.
#[test]
#[ignore = "captures the Go goldens: SDA_GO_BINARY=<swarm-device-access v1.0.3> cargo test --test parity -- --ignored"]
fn capture_go_parity() {
    let binary =
        std::env::var("SDA_GO_BINARY").unwrap_or_else(|error| panic!("SDA_GO_BINARY: {error}"));
    let mut cli = String::new();
    for outcome in cli_outcomes(&binary, true) {
        let _ = writeln!(cli, "{outcome}#END");
    }
    let run = run_daemon(&binary, true);
    assert_eq!(run.exit, Some(0), "{}", run.logs.join("\n"));
    std::fs::create_dir_all(testdata(""))
        .unwrap_or_else(|error| panic!("create testdata: {error}"));
    for (name, text) in [
        ("go-cli.txt", cli),
        ("go-logs.txt", run.logs.join("\n") + "\n"),
        ("go-requests.txt", run.requests.join("\n") + "\n"),
        ("go-metrics.txt", run.metrics.join("\n") + "\n"),
    ] {
        std::fs::write(testdata(name), text)
            .unwrap_or_else(|error| panic!("write {name}: {error}"));
    }
}
