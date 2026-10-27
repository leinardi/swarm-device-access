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

//! moby's `StdCopy` behaviour, frame by frame.

use std::io;
use std::pin::Pin;
use std::task::{Context, Poll};

use tokio::io::AsyncWrite;

use super::std_copy;

fn frame(stream: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![stream, 0, 0, 0];
    out.extend_from_slice(&u32::try_from(payload.len()).unwrap().to_be_bytes());
    out.extend_from_slice(payload);
    out
}

async fn copy(input: &[u8]) -> (Result<u64, String>, Vec<u8>, Vec<u8>) {
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    let mut source = input;
    let copied = std_copy(&mut stdout, &mut stderr, &mut source).await;
    (copied, stdout, stderr)
}

/// Stdin and stdout frames go to stdout, stderr frames to stderr, in order, and the count is
/// what was written.
#[tokio::test]
async fn demultiplexes_by_stream() {
    let input = [
        frame(1, b"out\n"),
        frame(2, b"err\n"),
        frame(0, b"in\n"),
        frame(1, b""),
    ]
    .concat();
    let (copied, stdout, stderr) = copy(&input).await;
    assert_eq!(copied, Ok(11));
    assert_eq!(stdout, b"out\nin\n");
    assert_eq!(stderr, b"err\n");
}

/// A frame larger than the starting buffer is copied whole.
#[tokio::test]
async fn copies_a_frame_larger_than_the_buffer() {
    let payload = vec![b'x'; 100 * 1024];
    let (copied, stdout, _) = copy(&frame(1, &payload)).await;
    assert_eq!(copied, Ok(100 * 1024));
    assert_eq!(stdout, payload);
}

/// The end of the input inside a header or a frame ends the copy without an error.
#[tokio::test]
async fn a_truncated_header_or_frame_ends_cleanly() {
    let whole = frame(1, b"done\n");
    let mut truncated_frame = frame(1, b"lost line\n");
    truncated_frame.truncate(12);
    for tail in [&[1_u8, 0, 0][..], &truncated_frame[..]] {
        let input = [&whole[..], tail].concat();
        let (copied, stdout, _) = copy(&input).await;
        assert_eq!(copied, Ok(5));
        assert_eq!(stdout, b"done\n");
    }
}

/// A system-error frame is the daemon's error, and an unknown stream is refused.
#[tokio::test]
async fn system_errors_and_unknown_streams_fail() {
    let (copied, _, _) = copy(&frame(3, b"boom")).await;
    assert_eq!(copied, Err("error from daemon in stream: boom".to_owned()));
    let (copied, _, _) = copy(&frame(7, b"x")).await;
    assert_eq!(copied, Err("unrecognized stream: 7".to_owned()));
}

/// A writer that takes nothing more is Go's `io.ErrShortWrite`.
#[tokio::test]
async fn a_short_write_fails() {
    struct Full;
    impl AsyncWrite for Full {
        fn poll_write(
            self: Pin<&mut Self>,
            _context: &mut Context<'_>,
            _data: &[u8],
        ) -> Poll<io::Result<usize>> {
            Poll::Ready(Ok(0))
        }
        fn poll_flush(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
        fn poll_shutdown(self: Pin<&mut Self>, _context: &mut Context<'_>) -> Poll<io::Result<()>> {
            Poll::Ready(Ok(()))
        }
    }
    let input = frame(1, b"line\n");
    let mut source = &input[..];
    let copied = std_copy(&mut Full, &mut Vec::new(), &mut source).await;
    assert_eq!(copied, Err("short write".to_owned()));
}
