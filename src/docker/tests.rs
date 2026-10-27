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

// Adapted from swarm-scheduler-exporter/src/docker/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The Docker client against a fake daemon: ports of the Go client tests, Q5, and one test per
//! S1 negotiation case.

use std::fmt::Write as _;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use http_body_util::BodyExt as _;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

use super::client::Client;
use super::{ErrorKind, Settings};
use crate::test_support::TempDir;

/// A request the fake daemon received.
#[derive(Debug, Clone)]
struct Recorded {
    connection: usize,
    method: String,
    target: String,
    head: String,
}

impl Recorded {
    fn header(&self, name: &str) -> Vec<String> {
        self.head
            .lines()
            .skip(1)
            .filter_map(|line| line.split_once(": "))
            .filter(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.to_owned())
            .collect()
    }
}

/// What the fake daemon does with a request.
#[derive(Debug, Clone)]
enum Reply {
    /// A response with a `Content-Length` (no body written for `HEAD`).
    Respond {
        status: u16,
        headers: Vec<(&'static str, String)>,
        body: Vec<u8>,
    },
    /// The connection closed before any response.
    Close,
    /// The connection closed after the response, unannounced, as an idle timeout closes it.
    RespondThenClose(Box<Reply>),
    /// `Reply` after a delay.
    After(Duration, Box<Reply>),
}

fn ok(headers: &[(&'static str, &str)], body: &str) -> Reply {
    respond(200, headers, body)
}

fn respond(status: u16, headers: &[(&'static str, &str)], body: &str) -> Reply {
    Reply::Respond {
        status,
        headers: headers
            .iter()
            .map(|(name, value)| (*name, (*value).to_owned()))
            .collect(),
        body: body.as_bytes().to_vec(),
    }
}

type Script = Arc<dyn Fn(&Recorded) -> Reply + Send + Sync>;

/// A daemon on a unix socket answering by script, recording every request.
struct FakeDaemon {
    requests: Arc<Mutex<Vec<Recorded>>>,
    socket: String,
    _dir: TempDir,
}

impl FakeDaemon {
    fn new(script: impl Fn(&Recorded) -> Reply + Send + Sync + 'static) -> Self {
        let dir = TempDir::new();
        let socket = dir.path().join("docker.sock");
        let listener = UnixListener::bind(&socket).unwrap();
        let script: Script = Arc::new(script);
        let requests = Arc::new(Mutex::new(Vec::new()));
        let recorded = Arc::clone(&requests);
        tokio::spawn(async move {
            let connections = AtomicUsize::new(0);
            loop {
                let id = connections.fetch_add(1, Ordering::SeqCst);
                let script = Arc::clone(&script);
                let recorded = Arc::clone(&recorded);
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    serve(stream, id, &script, &recorded).await;
                });
            }
        });
        Self {
            requests,
            socket: socket.display().to_string(),
            _dir: dir,
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.requests.lock().unwrap().clone()
    }

    /// `METHOD target` of every request, in order.
    fn wire(&self) -> Vec<String> {
        self.requests()
            .iter()
            .map(|request| format!("{} {}", request.method, request.target))
            .collect()
    }

    fn client(&self) -> Client {
        Client::new(Settings::unix(&self.socket).unwrap())
    }
}

async fn serve(
    mut stream: UnixStream,
    connection: usize,
    script: &Script,
    recorded: &Mutex<Vec<Recorded>>,
) {
    let mut buffer = Vec::new();
    loop {
        let head = loop {
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).into_owned();
                buffer.drain(..end + 4);
                break head;
            }
            let mut chunk = [0_u8; 4096];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(read) => buffer.extend_from_slice(&chunk[..read]),
            }
        };
        let mut request_line = head.lines().next().unwrap_or_default().split(' ');
        let request = Recorded {
            connection,
            method: request_line.next().unwrap_or_default().to_owned(),
            target: request_line.next().unwrap_or_default().to_owned(),
            head: head.clone(),
        };
        recorded.lock().unwrap().push(request.clone());
        let mut reply = script(&request);
        let mut close_after = false;
        loop {
            match reply {
                Reply::After(delay, then) => {
                    tokio::time::sleep(delay).await;
                    reply = *then;
                }
                Reply::RespondThenClose(then) => {
                    close_after = true;
                    reply = *then;
                }
                Reply::Respond { .. } | Reply::Close => break,
            }
        }
        match reply {
            Reply::Respond {
                status,
                headers,
                body,
            } => {
                let mut out = format!("HTTP/1.1 {status} {}\r\n", reason(status));
                for (name, value) in &headers {
                    write!(out, "{name}: {value}\r\n").unwrap();
                }
                write!(out, "Content-Length: {}\r\n\r\n", body.len()).unwrap();
                let mut bytes = out.into_bytes();
                if request.method != "HEAD" {
                    bytes.extend_from_slice(&body);
                }
                if stream.write_all(&bytes).await.is_err() {
                    return;
                }
                let _ = stream.flush().await;
                if close_after {
                    let _ = stream.shutdown().await;
                    return;
                }
            }
            Reply::Close => return,
            Reply::After(..) | Reply::RespondThenClose(_) => unreachable!(),
        }
    }
}

fn reason(status: u16) -> &'static str {
    hyper::StatusCode::from_u16(status)
        .ok()
        .and_then(|status| status.canonical_reason())
        .unwrap_or("Unknown")
}

/// A daemon that pings with `Api-Version` (or none) and lists no container.
fn engine(api_version: Option<&'static str>) -> impl Fn(&Recorded) -> Reply + Send + Sync {
    move |request| {
        let mut headers = vec![("Content-Type", "application/json")];
        if let Some(version) = api_version {
            headers.push(("Api-Version", version));
        }
        if request.target == "/_ping" {
            ok(&headers, "OK")
        } else {
            ok(&headers, "[]")
        }
    }
}

async fn list_containers(client: &Client) -> Result<String, super::Error> {
    let response = client.get("/containers/json", "", &[]).await?;
    let body = response.into_body().collect().await?.to_bytes();
    Ok(String::from_utf8(body.to_vec()).unwrap())
}

/// Two container lists, and the wire they produce.
async fn two_calls(daemon: &FakeDaemon) -> (Vec<String>, String) {
    let client = daemon.client();
    for _ in 0..2 {
        assert_eq!(list_containers(&client).await.unwrap(), "[]");
    }
    (daemon.wire(), client.client_version().await)
}

/// The socket the Go daemon passes as `unix://<socket>`, and the version before any request.
#[tokio::test]
async fn settings_name_the_socket() {
    let settings = Settings::unix("/var/run/docker.sock").unwrap();
    assert_eq!(settings.daemon_host(), "unix:///var/run/docker.sock");
    assert_eq!(settings.socket(), "/var/run/docker.sock");
    assert_eq!(
        Settings::unix("").unwrap_err().to_string(),
        "unable to parse docker host `unix://`"
    );
    assert_eq!(Client::new(settings).client_version().await, "1.56");
}

/// The requests the Go daemon wrote over its socket (`tests/testdata/engine_wire`): the
/// `api.moby.localhost` host, moby's `User-Agent`, no `Accept-Encoding`, `Accept` values as
/// separate lines in moby's order, and the ping's connection reused.
#[tokio::test]
async fn requests_match_the_go_wire() {
    let daemon = FakeDaemon::new(engine(Some("1.56")));
    let client = daemon.client();
    assert_eq!(list_containers(&client).await.unwrap(), "[]");
    let response = client
        .get("/events", "since=1", &super::events::EVENTS_ACCEPT)
        .await
        .unwrap();
    drop(response);
    let requests = daemon.requests();
    assert_eq!(
        daemon.wire(),
        [
            "HEAD /_ping",
            "GET /v1.56/containers/json",
            "GET /v1.56/events?since=1"
        ]
    );
    let user_agent = format!("moby-client/v0.6.0 linux/{}", super::client::go_arch());
    assert_eq!(
        requests[0].head,
        format!("HEAD /_ping HTTP/1.1\r\nHost: api.moby.localhost\r\nUser-Agent: {user_agent}")
    );
    assert_eq!(
        requests[1].head,
        format!(
            "GET /v1.56/containers/json HTTP/1.1\r\nHost: api.moby.localhost\r\nUser-Agent: \
             {user_agent}"
        )
    );
    assert_eq!(
        requests[2].head,
        format!(
            "GET /v1.56/events?since=1 HTTP/1.1\r\nHost: api.moby.localhost\r\nUser-Agent: \
             {user_agent}\r\nAccept: application/jsonl\r\nAccept: \
             application/x-ndjson;q=0.9\r\nAccept: application/json-seq;q=0.5"
        )
    );
    assert_eq!(requests[2].header("accept").len(), 3);
    // One connection for the ping and the list: the ping's was reused.
    assert_eq!(requests[0].connection, requests[1].connection);
}

/// moby's `GOARCH` names, which its `User-Agent` carries.
#[test]
fn the_user_agent_names_go_s_architecture() {
    let arch = super::client::go_arch();
    assert!(!arch.is_empty());
    if cfg!(target_arch = "x86_64") {
        assert_eq!(arch, "amd64");
    }
    if cfg!(target_arch = "aarch64") {
        assert_eq!(arch, "arm64");
    }
}

/// moby: a `HEAD` 200 is used as is, with no `GET`, and the version is negotiated once.
#[tokio::test]
async fn ping_head_200_negotiates_down() {
    let daemon = FakeDaemon::new(engine(Some("1.51")));
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(
        wire,
        [
            "HEAD /_ping",
            "GET /v1.51/containers/json",
            "GET /v1.51/containers/json"
        ]
    );
    assert_eq!(version, "1.51");
}

/// moby: no `Api-Version` means 1.56.
#[tokio::test]
async fn ping_without_api_version_is_1_56() {
    let daemon = FakeDaemon::new(engine(None));
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(
        wire,
        [
            "HEAD /_ping",
            "GET /v1.56/containers/json",
            "GET /v1.56/containers/json"
        ]
    );
    assert_eq!(version, "1.56");
}

/// moby: 1.39 is below the minimum; moby's error is discarded, the request goes out at 1.56,
/// and the next request pings again.
#[tokio::test]
async fn ping_below_minimum_is_not_refused_and_pings_again() {
    let daemon = FakeDaemon::new(engine(Some("1.39")));
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(
        wire,
        [
            "HEAD /_ping",
            "GET /v1.56/containers/json",
            "HEAD /_ping",
            "GET /v1.56/containers/json"
        ]
    );
    assert_eq!(version, "1.56");
}

/// moby: a `HEAD` 405, as the Go daemon's capture against such an Engine shows, falls back to
/// `GET /_ping`.
#[tokio::test]
async fn ping_head_405_falls_back_to_get() {
    let daemon = FakeDaemon::new(|request| {
        if request.method == "HEAD" {
            respond(405, &[], "")
        } else {
            engine(Some("1.51"))(request)
        }
    });
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(
        wire,
        [
            "HEAD /_ping",
            "GET /_ping",
            "GET /v1.51/containers/json",
            "GET /v1.51/containers/json"
        ]
    );
    assert_eq!(version, "1.51");
    // The `GET`'s short body is drained, as moby's `ensureReaderClosed` does, so the
    // connection is reused.
    let requests = daemon.requests();
    assert_eq!(requests[1].connection, requests[2].connection);
}

/// moby: a `HEAD` whose connection closes without an answer falls back to `GET /_ping`.
#[tokio::test]
async fn ping_head_closed_falls_back_to_get() {
    let daemon = FakeDaemon::new(|request| {
        if request.method == "HEAD" {
            Reply::Close
        } else {
            engine(Some("1.51"))(request)
        }
    });
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(wire[..2], ["HEAD /_ping", "GET /_ping"]);
    assert_eq!(
        wire[wire.len() - 2..],
        ["GET /v1.51/containers/json", "GET /v1.51/containers/json"]
    );
    assert_eq!(version, "1.51");
}

/// moby: `HEAD` and `GET` both 500; the request still goes out at 1.56 and the next one pings
/// again.
#[tokio::test]
async fn ping_both_failing_still_sends_and_pings_again() {
    let daemon = FakeDaemon::new(|request| {
        if request.target == "/_ping" {
            respond(500, &[("Api-Version", "1.51")], "boom")
        } else {
            engine(Some("1.51"))(request)
        }
    });
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(
        wire,
        [
            "HEAD /_ping",
            "GET /_ping",
            "GET /v1.56/containers/json",
            "HEAD /_ping",
            "GET /_ping",
            "GET /v1.56/containers/json",
        ]
    );
    assert_eq!(version, "1.56");
}

/// moby: a daemon above the maximum negotiates 1.56, once.
#[tokio::test]
async fn ping_above_maximum_is_1_56() {
    let daemon = FakeDaemon::new(engine(Some("1.60")));
    let (wire, version) = two_calls(&daemon).await;
    assert_eq!(
        wire,
        [
            "HEAD /_ping",
            "GET /v1.56/containers/json",
            "GET /v1.56/containers/json"
        ]
    );
    assert_eq!(version, "1.56");
}

/// The explicit ping the Go daemon sends before every subscription: unversioned, and no
/// negotiation of its own; a failing `GET` is moby's response error.
#[tokio::test]
async fn the_explicit_ping_does_not_negotiate() {
    let daemon = FakeDaemon::new(engine(Some("1.51")));
    let client = daemon.client();
    assert_eq!(client.ping().await.unwrap(), "1.51");
    assert_eq!(client.client_version().await, "1.56");
    assert_eq!(daemon.wire(), ["HEAD /_ping"]);

    let failing = FakeDaemon::new(|_| respond(500, &[("Content-Type", "text/plain")], "down"));
    let error = failing.client().ping().await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Response);
    assert_eq!(error.to_string(), "Error response from daemon: down");
    assert_eq!(failing.wire(), ["HEAD /_ping", "GET /_ping"]);
}

/// Concurrent first requests share one ping (moby sends one each), whether it negotiates or
/// fails; after a failed one, the next request pings again.
#[tokio::test]
async fn concurrent_first_requests_share_one_ping() {
    let slow_ping = |version: Option<&'static str>, status: u16| {
        move |request: &Recorded| {
            if request.target == "/_ping" {
                let mut headers = Vec::new();
                if let Some(version) = version {
                    headers.push(("Api-Version", version));
                }
                Reply::After(
                    Duration::from_millis(100),
                    Box::new(respond(status, &headers, "OK")),
                )
            } else {
                ok(&[], "[]")
            }
        }
    };
    for (version, status, pings) in [(Some("1.51"), 200, 1), (None, 500, 2)] {
        let daemon = FakeDaemon::new(slow_ping(version, status));
        let client = Arc::new(daemon.client());
        let mut calls = Vec::new();
        for _ in 0..8 {
            let client = Arc::clone(&client);
            calls.push(tokio::spawn(async move {
                list_containers(&client).await.unwrap()
            }));
        }
        for call in calls {
            call.await.unwrap();
        }
        let wire = daemon.wire();
        let ping_requests = wire.iter().filter(|line| line.ends_with("/_ping")).count();
        assert_eq!(ping_requests, pings, "{status}: {wire:?}");
        let want = if status == 200 {
            "GET /v1.51/containers/json"
        } else {
            "GET /v1.56/containers/json"
        };
        assert_eq!(
            wire.iter().filter(|line| *line == want).count(),
            8,
            "{wire:?}"
        );
        if status != 200 {
            list_containers(&client).await.unwrap();
            let wire = daemon.wire();
            assert_eq!(
                wire[wire.len() - 3..],
                ["HEAD /_ping", "GET /_ping", "GET /v1.56/containers/json"]
            );
        }
    }
}

/// A connection the daemon closed while idle is replaced without failing the request, as Go's
/// transport replaces it.
#[tokio::test]
async fn idle_connection_closed_by_the_daemon_is_replaced() {
    let daemon =
        FakeDaemon::new(|request| Reply::RespondThenClose(Box::new(engine(Some("1.56"))(request))));
    let client = daemon.client();
    assert_eq!(list_containers(&client).await.unwrap(), "[]");
    // Let the close reach the client, as it would after an idle timeout.
    tokio::time::sleep(Duration::from_millis(50)).await;
    assert_eq!(list_containers(&client).await.unwrap(), "[]");
    let requests = daemon.requests();
    assert_eq!(
        daemon.wire(),
        [
            "HEAD /_ping",
            "GET /v1.56/containers/json",
            "GET /v1.56/containers/json"
        ]
    );
    assert_ne!(requests[1].connection, requests[2].connection);
}

/// moby's wording of the failures, and the status of a response error.
#[tokio::test]
async fn errors_are_worded_as_moby() {
    // A socket file nothing listens on.
    let refused = TempDir::new();
    let path = refused.path().join("docker.sock");
    drop(UnixListener::bind(&path).unwrap());
    let host = format!("unix://{}", path.display());
    let client = Client::new(Settings::unix(&path.display().to_string()).unwrap());
    assert_eq!(
        list_containers(&client).await.unwrap_err().to_string(),
        format!("Cannot connect to the Docker daemon at {host}. Is the docker daemon running?")
    );

    // A socket path that does not exist.
    let missing = TempDir::new();
    let path = missing.path().join("docker.sock");
    let host = format!("unix://{}", path.display());
    let client = Client::new(Settings::unix(&path.display().to_string()).unwrap());
    let error = list_containers(&client).await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::ConnectionFailed);
    assert_eq!(
        error.to_string(),
        format!(
            "failed to connect to the docker API at {host}; check if the path is correct and if \
             the daemon is running: dial unix {}: connect: no such file or directory",
            path.display()
        )
    );

    // Response errors: a JSON message, a plain body, an empty body, a JSON body without message.
    let daemon = FakeDaemon::new(|request| match request.target.as_str() {
        "/_ping" => ok(&[("Api-Version", "1.56")], "OK"),
        "/v1.56/json" => respond(
            404,
            &[("Content-Type", "application/json")],
            "{\"message\":\" No such container: c \"}",
        ),
        "/v1.56/plain" => respond(500, &[("Content-Type", "text/plain")], " oops\n"),
        "/v1.56/empty" => respond(404, &[], ""),
        _ => respond(500, &[("Content-Type", "application/json")], "{}"),
    });
    let client = daemon.client();
    let failure = |path: &'static str| {
        let client = &client;
        async move { client.get(path, "", &[]).await.unwrap_err() }
    };
    let error = failure("/json").await;
    assert_eq!(error.kind(), ErrorKind::Response);
    assert_eq!(error.status(), Some(hyper::StatusCode::NOT_FOUND));
    assert!(error.is_not_found());
    assert_eq!(
        error.to_string(),
        "Error response from daemon: No such container: c"
    );
    assert_eq!(
        failure("/plain").await.to_string(),
        "Error response from daemon: oops"
    );
    let socket = daemon.socket.replace('/', "%2F");
    assert_eq!(
        failure("/empty").await.to_string(),
        format!(
            "request returned 404 Not Found for API route and version \
             http://{socket}/v1.56/empty, check if the server supports the requested API version"
        )
    );
    assert_eq!(
        failure("/nomessage").await.to_string(),
        "Error response from daemon: API returned a 500 (Internal Server Error) but provided no \
         error-message"
    );
}
