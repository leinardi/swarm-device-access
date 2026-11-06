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

// Adapted from swarm-scheduler-exporter/src/server/tests/wire.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The 41 Go 1.26 wire captures of 1.x's metrics server (`internal/observability/http.go`),
//! replayed against the server, byte for byte. Each capture holds a header line naming the Go
//! server's port (18080 ready, 18081 not ready), the request with `\r\n` written out, then the
//! response as read and `[end: <nil>]` once the server closed; the `/metrics` capture was cut at
//! 1200 bytes, before its end.

use std::path::Path;

use go_http::testing::{self, Capture};

use super::*;

/// The captures that differ, each with its Q entry; every other one must be identical.
const DIFFERENT: [(&str, &str); 5] = [
    // Go's body starts with its `go_*` families (Q3), and its chunks follow the encoder's
    // writes; the head, with `Transfer-Encoding: chunked`, is the same.
    ("go-get-metrics", "Q3 body; Q11 chunk boundaries"),
    // hyper answers a malformed request with its own 400: no reason in the status line, no body.
    ("go-malformed", "Q11 malformed request"),
    ("go-bad-header", "Q11 malformed request"),
    // hyper answers an unknown version with a 400 where Go writes a 505.
    ("go-bad-version", "Q11 malformed request"),
    // hyper serves an HTTP/1.1 request without `Host`; Go refuses it with a 400.
    ("go-get-nohost", "Q11 malformed request"),
];

/// The Go server a capture was taken from, at the capture's time (the captures were taken over
/// a minute); `/metrics` answers 3300 bytes, more than Go buffers before chunking.
fn server_for(capture: &Capture) -> Server {
    let metrics = text_exposition("# HELP x y\n".repeat(300).leak().as_bytes());
    let ready = match capture.port {
        18080 => true,
        18081 => false,
        other => panic!("no Go server on port {other}"),
    };
    let clock = Arc::new(FakeClock::new(capture.date().unwrap_or_else(capture_time)));
    server(
        mux_with(ready, move |_| metrics.clone()),
        clock,
        &capture_logs(Level::INFO).0,
    )
}

/// The `go-*.txt` captures, sorted by name; the directory's README is not one.
fn captures() -> Vec<Capture> {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/http");
    let mut paths: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .collect();
    paths.sort();
    paths
        .iter()
        .map(|path| {
            let name = path.file_stem().unwrap().to_string_lossy();
            Capture::parse(&name, &std::fs::read(path).unwrap()).unwrap()
        })
        .collect()
}

/// 36 of the 41 captures are identical byte for byte, `Date` included; the other 5 are the Q11
/// and Q3 cases listed in [`DIFFERENT`].
#[tokio::test(start_paused = true)]
async fn the_go_wire_captures_are_reproduced() {
    let captures = captures();
    assert_eq!(captures.len(), 41);
    let replay = testing::replay(&captures, TIMEOUTS, server_for).await;
    let mut expected: Vec<&str> = DIFFERENT.iter().map(|(name, _)| *name).collect();
    expected.sort_unstable();
    let mut different = replay.different.clone();
    different.sort_unstable();
    assert_eq!(different, expected, "{}", replay.report);
    assert_eq!(replay.identical.len(), 36);
}

/// The `/metrics` capture's head is Go's, chunked framing included.
#[tokio::test(start_paused = true)]
async fn the_metrics_head_is_go_s() {
    let captures = captures();
    let capture = captures
        .iter()
        .find(|capture| capture.name == "go-get-metrics")
        .unwrap();
    let response =
        testing::exchange(Arc::new(server_for(capture)), TIMEOUTS, &capture.request).await;
    let head = testing::head(&response).unwrap();
    assert_eq!(head, testing::head(&capture.response).unwrap());
    assert!(response[head.len()..].starts_with(b"\r\n\r\n800\r\n"));
}
