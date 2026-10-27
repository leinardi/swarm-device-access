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

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;

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

/// A capture's request, and its response with whether the capture was cut before the end.
struct Capture {
    port: u16,
    request: Vec<u8>,
    response: Vec<u8>,
    truncated: bool,
}

fn parse(text: &[u8]) -> Capture {
    let mut lines = text.splitn(3, |byte| *byte == b'\n');
    let title = String::from_utf8(lines.next().unwrap().to_vec()).unwrap();
    let port = title
        .rsplit_once("(port ")
        .and_then(|(_, port)| port.strip_suffix(')'))
        .unwrap()
        .parse()
        .unwrap();
    let request_line = String::from_utf8(lines.next().unwrap().to_vec()).unwrap();
    let request = request_line
        .strip_prefix("### request: ")
        .unwrap()
        .replace("\\r\\n", "\r\n")
        .into_bytes();
    let rest = lines.next().unwrap();
    let end = b"[end: <nil>]\n\n";
    let (response, truncated) = if rest.ends_with(end) {
        (rest[..rest.len() - end.len()].to_vec(), false)
    } else {
        (rest.strip_suffix(b"\n").unwrap_or(rest).to_vec(), true)
    };
    Capture {
        port,
        request,
        response,
        truncated,
    }
}

/// The time of the capture's `Date`, so it compares exactly; the captures were taken over a
/// minute.
fn date_of(response: &[u8]) -> Timestamp {
    let text = String::from_utf8_lossy(response);
    text.split("\r\n")
        .find_map(|line| line.strip_prefix("Date: "))
        .map_or_else(capture_time, |date| {
            jiff::fmt::strtime::parse("%a, %d %b %Y %H:%M:%S GMT", date)
                .and_then(|parsed| parsed.to_datetime())
                .and_then(|civil| civil.to_zoned(jiff::tz::TimeZone::UTC))
                .unwrap()
                .timestamp()
        })
}

/// The Go server a capture was taken from, at the capture's time; `/metrics` answers 3300 bytes,
/// more than Go buffers before chunking.
fn app_for(capture: &Capture) -> App {
    let metrics = text_exposition("# HELP x y\n".repeat(300).leak().as_bytes());
    let mut app = app_with(metrics);
    match capture.port {
        18080 => {}
        18081 => app.readiness.set(false),
        other => panic!("no Go server on port {other}"),
    }
    app.clock = Arc::new(FakeClock::new(date_of(&capture.response)));
    app
}

/// 36 of the 41 captures are identical byte for byte, `Date` included; the other 5 are the Q11
/// and Q3 cases listed in [`DIFFERENT`].
#[tokio::test(start_paused = true)]
async fn the_go_wire_captures_are_reproduced() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/http");
    let mut names: Vec<_> = std::fs::read_dir(&directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "txt"))
        .collect();
    names.sort();
    assert_eq!(names.len(), 41);
    let mut outcomes = BTreeMap::new();
    let mut report = String::new();
    for path in names {
        let name = path.file_stem().unwrap().to_string_lossy().into_owned();
        let capture = parse(&std::fs::read(&path).unwrap());
        let response = exchange(app_for(&capture), &capture.request).await;
        let identical = if capture.truncated {
            response.starts_with(&capture.response)
        } else {
            response == capture.response
        };
        if !identical {
            let _ = writeln!(
                report,
                "{name} differs:\n--- Go\n{}\n--- Rust\n{}",
                String::from_utf8_lossy(&capture.response),
                String::from_utf8_lossy(&response)
            );
        }
        outcomes.insert(name, identical);
    }
    let different: Vec<&str> = outcomes
        .iter()
        .filter(|(_, identical)| !**identical)
        .map(|(name, _)| name.as_str())
        .collect();
    let mut expected: Vec<&str> = DIFFERENT.iter().map(|(name, _)| *name).collect();
    expected.sort_unstable();
    assert_eq!(different, expected, "{report}");
    let identical = outcomes.values().filter(|identical| **identical).count();
    assert_eq!(identical, 36);
}

/// The `/metrics` capture's head is Go's, chunked framing included.
#[tokio::test(start_paused = true)]
async fn the_metrics_head_is_go_s() {
    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/http");
    let capture = parse(&std::fs::read(directory.join("go-get-metrics.txt")).unwrap());
    let response = exchange(app_for(&capture), &capture.request).await;
    let head_end = |bytes: &[u8]| {
        bytes
            .windows(4)
            .position(|window| window == b"\r\n\r\n")
            .unwrap()
    };
    let go_head = &capture.response[..head_end(&capture.response)];
    assert_eq!(&response[..head_end(&response)], go_head);
    assert!(response[head_end(&response)..].starts_with(b"\r\n\r\n800\r\n"));
}
