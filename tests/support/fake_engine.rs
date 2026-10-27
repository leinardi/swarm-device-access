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
//! It speaks just enough HTTP/1.1 for the client: keep-alive, `Content-Length` answers, and a
//! chunked body (an event stream, a chunked inspect) that ends with the terminating chunk, or is
//! cut without it where the capture cut it.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

use serde_json::Value;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

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
                    },
                    status: text("status"),
                    response_header: header_map(&exchange["responseHeader"]),
                    chunked: exchange["responseTransferEncoding"]
                        .as_array()
                        .is_some_and(|codings| codings.iter().any(|coding| coding == "chunked")),
                    body: text("responseBody").into_bytes(),
                    cut: exchange["cut"].as_bool().unwrap_or_default(),
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
            replay(&mut stream, &exchange, &request.method).await
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

/// Writes the captured answer; false when the connection is over.
async fn replay(stream: &mut UnixStream, exchange: &Exchange, method: &str) -> bool {
    let mut out = format!("HTTP/1.1 {}\r\n", exchange.status);
    for (name, values) in &exchange.response_header {
        if name == "Content-Length" && method != "HEAD" {
            continue;
        }
        for value in values {
            let _ = write!(out, "{name}: {value}\r\n");
        }
    }
    let bytes = if exchange.chunked {
        out.push_str("Transfer-Encoding: chunked\r\n\r\n");
        let mut bytes = out.into_bytes();
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
            for (name, value) in lines.filter_map(|line| line.split_once(':')) {
                let name = canonical_key(name.trim());
                let value = value.trim().to_owned();
                if name == "Host" {
                    host = value;
                } else {
                    header.entry(name).or_default().push(value);
                }
            }
            return Some(WireRequest {
                conn,
                method,
                uri,
                host,
                header,
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
