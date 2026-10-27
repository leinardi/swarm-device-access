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

// Adapted from swarm-scheduler-exporter/tests/support/fake_engine.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! A fake Docker Engine on a unix socket that replays the engine-wire captures
//! (`tests/testdata/engine_wire`): each request is answered with the next captured exchange of
//! the same method and URI, as the real daemon answered the Go client, and recorded, so a test
//! can compare what the Rust client sent with what the Go client sent.
//!
//! It speaks just enough HTTP/1.1 for the client: keep-alive, request bodies with a
//! `Content-Length`, `Content-Length` answers, a chunked body (an event stream, a chunked
//! inspect) that ends with the terminating chunk, or is cut without it where the capture cut it,
//! and the attach's `101` followed by the bytes the daemon sent on the upgraded connection.
//!
//! An answer the capture got only later (the launcher's wait, answered once the daemon was
//! removed, and the attach stream, which ends with the daemon) can be held: its head goes out at
//! once, and the rest once a given later request has been answered.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::watch;

use swarm_device_access::docker::{Client, Settings};

/// The negotiated API version whose captures the tests replay.
pub const CAPTURE_DIR: &str = "api-1.56";

/// One request as it went over the wire: what the captures record of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireRequest {
    /// The client connection it came on, numbered from 1 in order of first use.
    pub conn: usize,
    pub method: String,
    pub uri: String,
    pub host: String,
    /// The other headers, keys as Go canonicalizes them, values in the order the lines came.
    pub header: BTreeMap<String, Vec<String>>,
    /// The body, empty when there is none.
    pub body: String,
}

/// One captured exchange: the request and the answer the daemon gave.
#[derive(Debug, Clone)]
pub struct Exchange {
    pub request: WireRequest,
    /// `200 OK`, as Go's `Response.Status`.
    pub status: String,
    pub response_header: BTreeMap<String, Vec<String>>,
    pub chunked: bool,
    pub body: Vec<u8>,
    /// The capture closed the connection while the body streamed.
    pub cut: bool,
    /// For an upgrade, what the daemon sent on the upgraded connection.
    pub upgraded: Option<Vec<u8>>,
}

/// The exchanges of one capture file, in order.
#[derive(Debug, Clone)]
pub struct Capture {
    pub exchanges: Vec<Exchange>,
}

impl Capture {
    /// Reads `name` from the captures of [`CAPTURE_DIR`].
    #[must_use]
    pub fn load(name: &str) -> Self {
        let path: PathBuf = [
            env!("CARGO_MANIFEST_DIR"),
            "tests/testdata/engine_wire",
            CAPTURE_DIR,
            name,
        ]
        .iter()
        .collect();
        Self::read(&path)
    }

    fn read(path: &Path) -> Self {
        let text = std::fs::read_to_string(path)
            .unwrap_or_else(|error| panic!("capture {}: {error}", path.display()));
        let exchanges = text
            .lines()
            .map(|line| {
                let exchange: Value = serde_json::from_str(line)
                    .unwrap_or_else(|error| panic!("capture {}: {error}", path.display()));
                assert!(
                    exchange.get("responseBodyBase64").is_none(),
                    "a binary body is not replayed: {line}"
                );
                let text = |key: &str| exchange[key].as_str().unwrap_or_default().to_owned();
                Exchange {
                    request: WireRequest {
                        conn: usize::try_from(exchange["conn"].as_u64().unwrap_or_default())
                            .unwrap_or_default(),
                        method: text("method"),
                        uri: text("uri"),
                        host: text("host"),
                        header: header_map(&exchange["header"]),
                        body: text("body"),
                    },
                    status: text("status"),
                    response_header: header_map(&exchange["responseHeader"]),
                    chunked: exchange["responseTransferEncoding"]
                        .as_array()
                        .is_some_and(|codings| codings.iter().any(|coding| coding == "chunked")),
                    body: text("responseBody").into_bytes(),
                    cut: exchange["cut"].as_bool().unwrap_or_default(),
                    upgraded: exchange["upgraded"]
                        .as_bool()
                        .unwrap_or_default()
                        .then(|| decode_base64(&text("upgradedFromServerBase64"))),
                }
            })
            .collect();
        Self { exchanges }
    }

    /// The captured requests, in order.
    #[must_use]
    pub fn requests(&self) -> Vec<WireRequest> {
        self.exchanges
            .iter()
            .map(|exchange| exchange.request.clone())
            .collect()
    }

    /// The URI of the `index`-th captured request (from 0) whose method is `method` and whose URI
    /// starts with `prefix`.
    #[must_use]
    pub fn uri(&self, method: &str, prefix: &str, index: usize) -> String {
        self.exchanges
            .iter()
            .map(|exchange| &exchange.request)
            .filter(|request| request.method == method && request.uri.starts_with(prefix))
            .nth(index)
            .unwrap_or_else(|| panic!("no request {index} {method} {prefix}*"))
            .uri
            .clone()
    }
}

fn header_map(value: &Value) -> BTreeMap<String, Vec<String>> {
    value
        .as_object()
        .map(|header| {
            header
                .iter()
                .map(|(key, values)| {
                    let values = values
                        .as_array()
                        .map(|values| {
                            values
                                .iter()
                                .map(|value| value.as_str().unwrap_or_default().to_owned())
                                .collect()
                        })
                        .unwrap_or_default();
                    (key.clone(), values)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Go's `base64.StdEncoding.DecodeString` of a capture's bytes.
fn decode_base64(text: &str) -> Vec<u8> {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = Vec::new();
    let mut bits: u32 = 0;
    let mut count = 0;
    for byte in text.bytes().filter(|byte| *byte != b'=') {
        let value = ALPHABET
            .iter()
            .position(|letter| *letter == byte)
            .unwrap_or_else(|| panic!("not base64: {text}"));
        bits = (bits << 6) | u32::try_from(value).unwrap_or_default();
        count += 6;
        if count >= 8 {
            count -= 8;
            out.push(u8::try_from((bits >> count) & 0xff).unwrap_or_default());
        }
    }
    out
}

/// Answers held until a later request was answered.
#[derive(Debug)]
struct Hold {
    /// The URIs (by substring) whose answer is held.
    held: Vec<String>,
    /// The URI (by substring) whose answer releases them.
    trigger: String,
    released: watch::Sender<bool>,
}

#[derive(Debug, Default)]
struct State {
    /// Which captured exchanges have been served.
    served: Vec<bool>,
    requests: Vec<WireRequest>,
    unexpected: Vec<String>,
}

#[derive(Debug)]
struct Shared {
    capture: Capture,
    state: Mutex<State>,
    connections: AtomicUsize,
    hold: Option<Hold>,
}

/// The fake engine, serving until it is dropped.
#[derive(Debug)]
pub struct FakeEngine {
    shared: Arc<Shared>,
    socket: PathBuf,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for FakeEngine {
    fn drop(&mut self) {
        self.server.abort();
        let _ = std::fs::remove_dir_all(self.socket.parent().unwrap_or(&self.socket));
    }
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    // A test's panic poisons it; the recording is still what it was.
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl FakeEngine {
    /// Replays `capture` on a fresh socket.
    pub fn start(capture: Capture) -> Self {
        Self::serve(capture, None)
    }

    /// Replays `capture`, holding the rest of the answers to requests whose URI contains one of
    /// `held` until the answer to a request whose URI contains `trigger` has gone out; a held
    /// upgraded connection is then closed.
    pub fn start_holding(capture: Capture, held: &[&str], trigger: &str) -> Self {
        Self::serve(
            capture,
            Some(Hold {
                held: held.iter().map(|uri| (*uri).to_owned()).collect(),
                trigger: trigger.to_owned(),
                released: watch::Sender::new(false),
            }),
        )
    }

    fn serve(capture: Capture, hold: Option<Hold>) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "sda-fake-engine-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap_or_else(|error| panic!("fake engine: {error}"));
        let socket = dir.join("docker.sock");
        let listener = UnixListener::bind(&socket)
            .unwrap_or_else(|error| panic!("fake engine: bind: {error}"));
        let shared = Arc::new(Shared {
            state: Mutex::new(State {
                served: vec![false; capture.exchanges.len()],
                ..State::default()
            }),
            capture,
            connections: AtomicUsize::new(0),
            hold,
        });
        let server_shared = Arc::clone(&shared);
        let server = tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let conn = server_shared.connections.fetch_add(1, Ordering::SeqCst) + 1;
                tokio::spawn(serve_connection(Arc::clone(&server_shared), stream, conn));
            }
        });
        Self {
            shared,
            socket,
            server,
        }
    }

    /// A Docker client on the fake's socket, as the daemon builds it from `--docker-socket`.
    #[must_use]
    pub fn client(&self) -> Client {
        let socket = self.socket.display().to_string();
        Client::new(Settings::unix(&socket).unwrap_or_else(|error| panic!("{error}")))
    }

    /// Every request so far, in arrival order.
    #[must_use]
    pub fn requests(&self) -> Vec<WireRequest> {
        lock(&self.shared.state).requests.clone()
    }

    /// The requests the capture had no answer for.
    #[must_use]
    pub fn unexpected_requests(&self) -> Vec<String> {
        lock(&self.shared.state).unexpected.clone()
    }
}

async fn serve_connection(shared: Arc<Shared>, mut stream: UnixStream, conn: usize) {
    let mut buffer = Vec::new();
    while let Some(request) = read_request(&mut stream, &mut buffer, conn).await {
        let exchange = {
            let mut state = lock(&shared.state);
            state.requests.push(request.clone());
            let next = shared
                .capture
                .exchanges
                .iter()
                .enumerate()
                .find(|(index, exchange)| {
                    !state.served[*index]
                        && exchange.request.method == request.method
                        && exchange.request.uri == request.uri
                })
                .map(|(index, exchange)| (index, exchange.clone()));
            if let Some((index, _)) = next {
                state.served[index] = true;
            } else {
                state
                    .unexpected
                    .push(format!("{} {}", request.method, request.uri));
            }
            next.map(|(_, exchange)| exchange)
        };
        let keep_open = if let Some(exchange) = exchange {
            let release = shared.hold.as_ref().and_then(|hold| {
                hold.held
                    .iter()
                    .any(|held| request.uri.contains(held.as_str()))
                    .then(|| hold.released.subscribe())
            });
            let open = replay(&mut stream, &exchange, &request.method, release).await;
            if let Some(hold) = &shared.hold
                && request.uri.contains(hold.trigger.as_str())
            {
                hold.released.send_replace(true);
            }
            open
        } else {
            let body = br#"{"message":"page not found"}"#;
            let head = format!(
                "HTTP/1.1 404 Not Found\r\nContent-Type: application/json\r\n\
                 Content-Length: {}\r\n\r\n",
                body.len()
            );
            let mut out = head.into_bytes();
            out.extend_from_slice(body);
            stream.write_all(&out).await.is_ok()
        };
        if !keep_open {
            return;
        }
    }
}

/// Waits until the held answers are released.
async fn released(release: &mut watch::Receiver<bool>) {
    let _ = release.wait_for(|released| *released).await;
}

/// Writes the captured answer, the part after the head once `release` (if any) says so; false
/// when the connection is over.
async fn replay(
    stream: &mut UnixStream,
    exchange: &Exchange,
    method: &str,
    mut release: Option<watch::Receiver<bool>>,
) -> bool {
    let mut out = format!("HTTP/1.1 {}\r\n", exchange.status);
    for (name, values) in &exchange.response_header {
        if name == "Content-Length" && method != "HEAD" {
            continue;
        }
        for value in values {
            let _ = write!(out, "{name}: {value}\r\n");
        }
    }
    if let Some(upgraded) = &exchange.upgraded {
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        bytes.extend_from_slice(upgraded);
        if stream.write_all(&bytes).await.is_ok()
            && let Some(release) = &mut release
        {
            released(release).await;
        }
        let _ = stream.shutdown().await;
        return false;
    }
    let bytes = if exchange.chunked {
        out.push_str("Transfer-Encoding: chunked\r\n\r\n");
        let mut bytes = out.into_bytes();
        if let Some(release) = &mut release {
            if stream.write_all(&bytes).await.is_err() {
                return false;
            }
            released(release).await;
            bytes.clear();
        }
        // One chunk per line, as the daemon flushes each event.
        for line in exchange.body.split_inclusive(|byte| *byte == b'\n') {
            bytes.extend_from_slice(format!("{:x}\r\n", line.len()).as_bytes());
            bytes.extend_from_slice(line);
            bytes.extend_from_slice(b"\r\n");
        }
        if !exchange.cut {
            bytes.extend_from_slice(b"0\r\n\r\n");
        }
        bytes
    } else {
        if method != "HEAD" {
            let _ = write!(out, "Content-Length: {}\r\n", exchange.body.len());
        }
        out.push_str("\r\n");
        let mut bytes = out.into_bytes();
        if method != "HEAD" {
            bytes.extend_from_slice(&exchange.body);
        }
        bytes
    };
    if stream.write_all(&bytes).await.is_err() {
        return false;
    }
    if exchange.cut {
        let _ = stream.shutdown().await;
        return false;
    }
    true
}

async fn read_request(
    stream: &mut UnixStream,
    buffer: &mut Vec<u8>,
    conn: usize,
) -> Option<WireRequest> {
    loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            let text = String::from_utf8_lossy(&buffer[..end]).into_owned();
            buffer.drain(..end + 4);
            let mut lines = text.split("\r\n");
            let mut request_line = lines.next()?.split(' ');
            let method = request_line.next()?.to_owned();
            let uri = request_line.next()?.to_owned();
            let mut host = String::new();
            let mut header: BTreeMap<String, Vec<String>> = BTreeMap::new();
            let mut length = 0;
            for (name, value) in lines.filter_map(|line| line.split_once(':')) {
                let name = canonical_key(name.trim());
                let value = value.trim().to_owned();
                if name == "Host" {
                    host = value;
                } else {
                    if name == "Content-Length" {
                        length = value.parse().unwrap_or_default();
                    }
                    header.entry(name).or_default().push(value);
                }
            }
            while buffer.len() < length {
                let mut chunk = [0_u8; 4096];
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return None,
                    Ok(read) => buffer.extend_from_slice(&chunk[..read]),
                }
            }
            let body = String::from_utf8_lossy(&buffer[..length]).into_owned();
            buffer.drain(..length);
            return Some(WireRequest {
                conn,
                method,
                uri,
                host,
                header,
                body,
            });
        }
        let mut chunk = [0_u8; 4096];
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => buffer.extend_from_slice(&chunk[..read]),
        }
    }
}

/// Go's `textproto.CanonicalMIMEHeaderKey`, as the captures record header names.
fn canonical_key(name: &str) -> String {
    let mut upper = true;
    name.chars()
        .map(|character| {
            let mapped = if upper {
                character.to_ascii_uppercase()
            } else {
                character.to_ascii_lowercase()
            };
            upper = character == '-';
            mapped
        })
        .collect()
}
