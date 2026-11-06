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

//! What adopting docker-engine changed in the daemon's client, each against its parent: the 1 MiB
//! event record bound and the yield between reads (swarm-scheduler-exporter's 2379192), and the
//! causes kept behind moby's unchanged error texts. The transport, negotiation, wording, event
//! decoding and context tests are docker-engine's; the wire is `tests/engine_wire.rs`.

use std::collections::VecDeque;
use std::error::Error as _;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::task::{Context, Poll};
use std::time::Duration;

use bytes::Bytes;
use hyper::body::Frame;
use jiff::Timestamp;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{UnixListener, UnixStream};

use super::types::Event;
use super::{Client, DockerApi as _, Error, EventStream as _, Filters, Settings, StreamErrorKind};
use docker_engine::JsonStream;

/// One more byte than an event record may hold.
const OVERSIZED: usize = 1024 * 1024 + 1;

/// A directory of its own under the temporary directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "sda-docker-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn socket(&self) -> String {
        self.0.join("docker.sock").display().to_string()
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A request's method and target, once its head is read; `None` when the client is gone.
async fn read_head(stream: &mut UnixStream) -> Option<(String, String)> {
    let mut head = Vec::new();
    let mut byte = [0_u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if stream.read(&mut byte).await.ok()? == 0 {
            return None;
        }
        head.push(byte[0]);
    }
    let line = String::from_utf8_lossy(&head).into_owned();
    let mut words = line.split(' ');
    Some((words.next()?.to_owned(), words.next()?.to_owned()))
}

/// An Engine on `socket` that answers the ping with API 1.56, a container's inspect with
/// `inspect`, and the events with a chunked body of `events`, held open after it.
fn engine(socket: &str, inspect: &'static str, events: Vec<u8>) -> tokio::task::JoinHandle<()> {
    let listener = UnixListener::bind(socket).unwrap();
    let events = Arc::new(events);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let events = Arc::clone(&events);
            tokio::spawn(async move {
                while let Some((method, target)) = read_head(&mut stream).await {
                    if target == "/_ping" {
                        let body = if method == "HEAD" { "" } else { "OK" };
                        let answer = format!(
                            "HTTP/1.1 200 OK\r\nApi-Version: 1.56\r\nContent-Length: 2\r\n\r\n{body}"
                        );
                        let _ = stream.write_all(answer.as_bytes()).await;
                    } else if target.starts_with("/v1.56/events") {
                        let _ = stream
                            .write_all(
                                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                                  Transfer-Encoding: chunked\r\n\r\n",
                            )
                            .await;
                        for piece in events.chunks(64 * 1024) {
                            let mut chunk = format!("{:x}\r\n", piece.len()).into_bytes();
                            chunk.extend_from_slice(piece);
                            chunk.extend_from_slice(b"\r\n");
                            if stream.write_all(&chunk).await.is_err() {
                                return;
                            }
                        }
                        // Held open: the record never completes.
                        let mut sink = Vec::new();
                        let _ = stream.read_to_end(&mut sink).await;
                        return;
                    } else {
                        let answer = format!(
                            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\n\
                             Content-Length: {}\r\n\r\n{inspect}",
                            inspect.len()
                        );
                        let _ = stream.write_all(answer.as_bytes()).await;
                    }
                }
            });
        }
    })
}

fn client(socket: &str) -> Client {
    Client::new(Settings::unix(socket).unwrap())
}

/// Every error of `error`'s source chain, from the outermost, as its text.
fn chain(error: &Error) -> Vec<String> {
    let mut texts = vec![error.to_string()];
    let mut source = error.source();
    while let Some(cause) = source {
        texts.push(cause.to_string());
        source = cause.source();
    }
    texts
}

/// An event record that grows past 1 MiB on a stream that stays open ends the stream with a
/// decode error, where it was buffered for as long as the daemon kept sending.
#[tokio::test]
async fn an_oversized_event_record_ends_the_stream_with_a_decode_error() {
    let dir = Scratch::new();
    let socket = dir.socket();
    let mut record = b"{\"Type\":\"".to_vec();
    record.resize(OVERSIZED + 64 * 1024, b'a');
    let _engine = engine(&socket, "{}", record);
    let mut stream = client(&socket)
        .events(Timestamp::UNIX_EPOCH, &Filters::new())
        .await
        .unwrap();
    let ended = tokio::time::timeout(Duration::from_secs(10), stream.next())
        .await
        .expect("the oversized record was buffered: the stream did not end within 10s")
        .unwrap_err();
    assert_eq!(ended.kind(), StreamErrorKind::Decode);
    assert_eq!(ended.to_string(), "event JSON exceeds 1048576 bytes");
}

/// A body whose frames are all ready at once, as hyper hands over what it already read.
struct Ready(VecDeque<Bytes>);

impl hyper::body::Body for Ready {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        Poll::Ready(
            self.get_mut()
                .0
                .pop_front()
                .map(|data| Ok(Frame::data(data))),
        )
    }
}

/// The event stream yields to the runtime before each read, so a burst of ready records does not
/// keep the daemon's other tasks (the coordinator, the shutdown watch) from running on its
/// worker; it ran through every ready frame without yielding.
#[tokio::test(flavor = "current_thread")]
async fn the_event_stream_yields_between_records() {
    let records = ["{\"Action\":\"start\"}\n", "{\"Action\":\"die\"}\n"];
    let mut stream = JsonStream::new(
        Ready(records.iter().map(|record| Bytes::from(*record)).collect()),
        Some("application/json"),
    );
    for action in ["start", "die"] {
        let ran = Arc::new(AtomicBool::new(false));
        let other = tokio::spawn({
            let ran = Arc::clone(&ran);
            async move { ran.store(true, Ordering::SeqCst) }
        });
        let event: Event = stream.next().await.unwrap();
        assert_eq!(event.action, action);
        assert!(
            ran.load(Ordering::SeqCst),
            "reading the {action} record never yielded to the other task"
        );
        other.await.unwrap();
    }
}

/// The moby text of a request to a socket path that does not exist, the dial's text, and the
/// error.
async fn missing_socket() -> (String, String, Error) {
    let dir = Scratch::new();
    let socket = dir.socket();
    let error = client(&socket).container_list().await.unwrap_err();
    let dial = format!("dial unix {socket}: connect: no such file or directory");
    let text = format!(
        "failed to connect to the docker API at unix://{socket}; check if the path is correct \
         and if the daemon is running: {dial}"
    );
    (text, dial, error)
}

/// A failed dial keeps moby's text and now carries the system error as its cause.
#[tokio::test]
async fn a_failed_dial_keeps_the_system_error_as_its_cause() {
    let (text, _, error) = missing_socket().await;
    assert_eq!(error.to_string(), text);
    let mut cause = error.source();
    let mut system = None;
    while let Some(found) = cause {
        system = found.downcast_ref::<std::io::Error>().or(system);
        cause = found.source();
    }
    assert_eq!(
        system.map(std::io::Error::kind),
        Some(std::io::ErrorKind::NotFound),
        "no system error in {:?}",
        chain(&error)
    );
}

/// The transport's failure (`SendError`) and the dial's (`ConnectError`) are errors of their own
/// in that chain, each with the dial's text, before the system error.
#[tokio::test]
async fn the_transport_and_dial_failures_are_errors_in_the_chain() {
    let (text, dial, error) = missing_socket().await;
    assert_eq!(
        chain(&error),
        [
            text,
            dial.clone(),
            dial,
            "No such file or directory (os error 2)".to_owned()
        ]
    );
}

/// An answer that is not the JSON asked for keeps serde's text and now carries serde's error as
/// its cause.
#[tokio::test]
async fn a_decode_failure_keeps_serde_s_error_as_its_cause() {
    let dir = Scratch::new();
    let socket = dir.socket();
    let _engine = engine(&socket, "{\"Id\":5}", Vec::new());
    let error = client(&socket).container_inspect("c1").await.unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid type: integer `5`, expected a string at line 1 column 7"
    );
    let cause = error.source();
    assert!(
        cause.is_some_and(<dyn std::error::Error>::is::<serde_json::Error>),
        "no serde_json cause: {:?}",
        chain(&error)
    );
}
