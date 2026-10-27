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

// Adapted from swarm-scheduler-exporter/src/docker/events/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

use std::collections::VecDeque;
use std::pin::Pin;
use std::task::{Context, Poll};

use hyper::body::Frame;

use super::*;
use crate::docker::types::Event;

/// A body that yields scripted chunks, then ends, or fails with a body error.
#[derive(Debug)]
struct ChunkBody {
    chunks: VecDeque<Bytes>,
    failure: Option<String>,
}

impl hyper::body::Body for ChunkBody {
    type Data = Bytes;
    type Error = Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _context: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Error>>> {
        if let Some(chunk) = self.chunks.pop_front() {
            return Poll::Ready(Some(Ok(Frame::data(chunk))));
        }
        Poll::Ready(self.failure.take().map(|message| Err(Error::body(message))))
    }
}

fn stream(content_type: Option<&str>, chunks: &[&[u8]]) -> JsonStream<ChunkBody> {
    JsonStream::new(
        ChunkBody {
            chunks: chunks
                .iter()
                .map(|chunk| Bytes::copy_from_slice(chunk))
                .collect(),
            failure: None,
        },
        content_type,
    )
}

fn cut_stream(content_type: Option<&str>, chunks: &[&[u8]]) -> JsonStream<ChunkBody> {
    let mut stream = stream(content_type, chunks);
    stream.body.failure = Some("error reading a body from connection".to_owned());
    stream
}

/// Every event until the end, and how it ended.
async fn drain(mut stream: JsonStream<ChunkBody>) -> (Vec<String>, StreamError) {
    let mut actors = Vec::new();
    loop {
        match stream.next::<Event>().await {
            Ok(event) => actors.push(event.actor.id),
            Err(end) => return (actors, end),
        }
    }
}

const FIRST: &[u8] = br#"{"Type":"service","Action":"update","Actor":{"ID":"svc-a"},"time":1,"timeNano":1000000001}"#;
const SECOND: &[u8] = br#"{"Type":"node","Action":"update","Actor":{"ID":"node-b"}}"#;

const PLAIN_TYPES: [Option<&str>; 5] = [
    Some("application/json"),
    Some("application/x-ndjson"),
    Some("application/jsonl"),
    None,
    Some("text/plain"),
];

fn lines(values: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        out.extend_from_slice(value);
        out.push(b'\n');
    }
    out
}

#[tokio::test]
async fn plain_types_decode_records_and_end_with_a_clean_eof() {
    for content_type in PLAIN_TYPES {
        let body = lines(&[FIRST, SECOND]);
        let (actors, end) = drain(stream(content_type, &[&body])).await;
        assert_eq!(actors, ["svc-a", "node-b"], "{content_type:?}");
        assert_eq!(end, StreamError::eof(), "{content_type:?}");
        assert!(end.is_eof());
        assert_eq!(end.to_string(), "EOF");
    }
}

#[tokio::test]
async fn plain_types_read_values_split_across_chunks() {
    for content_type in PLAIN_TYPES {
        let (head, tail) = FIRST.split_at(17);
        let (actors, end) = drain(stream(
            content_type,
            &[head, tail, b"\n", SECOND, b"\r\n \t"],
        ))
        .await;
        assert_eq!(actors, ["svc-a", "node-b"], "{content_type:?}");
        assert!(end.is_eof(), "{content_type:?}");
    }
}

#[tokio::test]
async fn a_record_cut_mid_object_is_an_unexpected_eof() {
    for content_type in PLAIN_TYPES
        .into_iter()
        .chain([Some(MEDIA_TYPE_JSON_SEQUENCE)])
    {
        let mut body = lines(&[FIRST]);
        body.extend_from_slice(&SECOND[..20]);
        let (actors, end) = drain(stream(content_type, &[&body])).await;
        assert_eq!(actors, ["svc-a"], "{content_type:?}");
        assert_eq!(end, StreamError::unexpected_eof(), "{content_type:?}");
        assert!(!end.is_eof());
        assert_eq!(end.to_string(), "unexpected EOF");
    }
}

/// A body error is never a clean close, wherever it happens: hyper's error for a chunked body
/// the peer cut is not `is_incomplete_message`, so the error itself is what counts.
#[tokio::test]
async fn a_body_error_is_an_unexpected_eof_between_and_inside_values() {
    for content_type in PLAIN_TYPES
        .into_iter()
        .chain([Some(MEDIA_TYPE_JSON_SEQUENCE)])
    {
        let body = lines(&[FIRST]);
        let (actors, end) = drain(cut_stream(content_type, &[&body])).await;
        assert_eq!(actors, ["svc-a"], "{content_type:?}");
        assert_eq!(
            end.kind(),
            StreamErrorKind::UnexpectedEof,
            "{content_type:?}"
        );
        assert_eq!(end.to_string(), "error reading a body from connection");

        let (actors, end) = drain(cut_stream(content_type, &[&FIRST[..9]])).await;
        assert!(actors.is_empty());
        assert_eq!(end.kind(), StreamErrorKind::UnexpectedEof);
    }
}

#[tokio::test]
async fn an_empty_body_is_a_clean_eof() {
    for content_type in PLAIN_TYPES
        .into_iter()
        .chain([Some(MEDIA_TYPE_JSON_SEQUENCE)])
    {
        let (actors, end) = drain(stream(content_type, &[])).await;
        assert!(actors.is_empty());
        assert!(end.is_eof(), "{content_type:?}");
    }
}

#[tokio::test]
async fn null_is_the_zero_event_and_garbage_a_decode_error() {
    let (actors, end) = drain(stream(Some("application/jsonl"), &[b"null\n", b"{]"])).await;
    assert_eq!(actors, [""]);
    assert_eq!(end.kind(), StreamErrorKind::Decode);
    let (_, end) = drain(stream(Some("application/jsonl"), &[br#"{"Type":5}"#])).await;
    assert_eq!(end.kind(), StreamErrorKind::Decode);
}

fn record_separated(values: &[&[u8]]) -> Vec<u8> {
    let mut out = Vec::new();
    for value in values {
        out.push(RS);
        out.extend_from_slice(value);
        out.push(b'\n');
    }
    out
}

#[tokio::test]
async fn json_seq_drops_record_separators() {
    let body = record_separated(&[FIRST, SECOND]);
    let decoder = stream(Some(MEDIA_TYPE_JSON_SEQUENCE), &[&body]);
    assert!(decoder.filters_rs());
    let (actors, end) = drain(decoder).await;
    assert_eq!(actors, ["svc-a", "node-b"]);
    assert!(end.is_eof());
}

#[tokio::test]
async fn json_seq_drops_a_separator_split_from_its_record() {
    // The second record's RS ends the first chunk; its JSON starts the next.
    let mut first = record_separated(&[FIRST]);
    first.push(RS);
    let mut second = SECOND.to_vec();
    second.push(b'\n');
    let (head, tail) = second.split_at(10);
    let (actors, end) = drain(stream(
        Some(MEDIA_TYPE_JSON_SEQUENCE),
        &[&first, head, &[RS], tail],
    ))
    .await;
    assert_eq!(actors, ["svc-a", "node-b"]);
    assert!(end.is_eof());
}

#[tokio::test]
async fn json_seq_with_a_separator_only_tail_is_a_clean_eof() {
    let body = record_separated(&[FIRST]);
    let (actors, end) = drain(stream(
        Some(MEDIA_TYPE_JSON_SEQUENCE),
        &[&body, &[RS], &[RS, RS]],
    ))
    .await;
    assert_eq!(actors, ["svc-a"]);
    assert!(end.is_eof());
}

#[tokio::test]
async fn json_seq_separators_between_whitespace_are_dropped() {
    let mut body = b" \x1e \n\x1e\t".to_vec();
    body.extend_from_slice(FIRST);
    body.extend_from_slice(b"\r\n \x1e ");
    let (actors, end) = drain(stream(Some(MEDIA_TYPE_JSON_SEQUENCE), &[&body])).await;
    assert_eq!(actors, ["svc-a"]);
    assert!(end.is_eof());
}

/// moby compares the media type exactly: a parameter, or any other type, means the plain
/// decoder, for which a separator is not JSON.
#[tokio::test]
async fn only_the_exact_json_seq_type_filters_separators() {
    for content_type in [
        Some("application/json-seq; charset=utf-8"),
        Some("application/jsonl"),
        None,
    ] {
        let decoder = stream(content_type, &[&record_separated(&[FIRST])]);
        assert!(!decoder.filters_rs());
        let (actors, end) = drain(decoder).await;
        assert!(actors.is_empty(), "{content_type:?}");
        assert_eq!(end.kind(), StreamErrorKind::Decode, "{content_type:?}");
    }
}

#[test]
fn events_accept_is_mobys() {
    assert_eq!(
        EVENTS_ACCEPT,
        [
            "application/jsonl",
            "application/x-ndjson;q=0.9",
            "application/json-seq;q=0.5",
        ]
    );
}
