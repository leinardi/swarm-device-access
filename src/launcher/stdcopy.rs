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

//! moby's `stdcopy.StdCopy` (api v1.56.0), which the Go launcher copies the daemon's attached
//! output with: 8-byte frame headers (the stream, three zero bytes, a big-endian length), stdin
//! and stdout frames to one writer, stderr frames to the other, a system-error frame the error,
//! any other stream refused. The end of the input before a complete header or frame ends the
//! copy without an error, as in moby.

use std::io;

use tokio::io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};

const PREFIX_LEN: usize = 8;
const SIZE_INDEX: usize = 4;
/// moby's `startingBufLen`.
const STARTING_BUF_LEN: usize = 32 * 1024 + PREFIX_LEN + 1;

const STDIN: u8 = 0;
const STDOUT: u8 = 1;
const STDERR: u8 = 2;
const SYSTEMERR: u8 = 3;

/// Copies the multiplexed `source` to `stdout` and `stderr` until it ends. Each frame is
/// written whole and flushed before the next is read, as Go's unbuffered writes are.
///
/// # Errors
///
/// moby's error text: the source's or a writer's error, `error from daemon in stream:
/// <message>` for a system-error frame, `unrecognized stream: <n>`, or `short write`.
pub async fn std_copy<Source, Stdout, Stderr>(
    stdout: &mut Stdout,
    stderr: &mut Stderr,
    source: &mut Source,
) -> Result<u64, String>
where
    Source: AsyncRead + Unpin + ?Sized,
    Stdout: AsyncWrite + Unpin + ?Sized,
    Stderr: AsyncWrite + Unpin + ?Sized,
{
    let mut buffer = vec![0_u8; STARTING_BUF_LEN];
    let mut filled = 0;
    let mut written: u64 = 0;
    loop {
        if !fill(source, &mut buffer, &mut filled, PREFIX_LEN).await? {
            return Ok(written);
        }
        let stream = buffer[0];
        if !matches!(stream, STDIN | STDOUT | STDERR | SYSTEMERR) {
            return Err(format!("unrecognized stream: {stream}"));
        }
        let size_bytes = [
            buffer[SIZE_INDEX],
            buffer[SIZE_INDEX + 1],
            buffer[SIZE_INDEX + 2],
            buffer[SIZE_INDEX + 3],
        ];
        let frame_size = usize::try_from(u32::from_be_bytes(size_bytes))
            .map_err(|_| "frame too large".to_owned())?;
        let frame_end = frame_size + PREFIX_LEN;
        if frame_end > buffer.len() {
            buffer.resize(frame_end + 1, 0);
        }
        if !fill(source, &mut buffer, &mut filled, frame_end).await? {
            return Ok(written);
        }
        let payload = &buffer[PREFIX_LEN..frame_end];
        if stream == SYSTEMERR {
            return Err(format!(
                "error from daemon in stream: {}",
                String::from_utf8_lossy(payload)
            ));
        }
        let wrote = if stream == STDERR {
            write_frame(stderr, payload).await
        } else {
            write_frame(stdout, payload).await
        };
        wrote.map_err(|error| write_error_text(&error))?;
        written += u64::try_from(frame_size).unwrap_or(u64::MAX);
        buffer.copy_within(frame_end..filled, 0);
        filled -= frame_end;
    }
}

/// Reads until `buffer` holds `want` bytes; false when the source ended first.
async fn fill<Source: AsyncRead + Unpin + ?Sized>(
    source: &mut Source,
    buffer: &mut [u8],
    filled: &mut usize,
    want: usize,
) -> Result<bool, String> {
    while *filled < want {
        match source.read(&mut buffer[*filled..]).await {
            Ok(0) => return Ok(false),
            Ok(read) => *filled += read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error.to_string()),
        }
    }
    Ok(true)
}

async fn write_frame<Writer: AsyncWrite + Unpin + ?Sized>(
    writer: &mut Writer,
    payload: &[u8],
) -> io::Result<()> {
    writer.write_all(payload).await?;
    writer.flush().await
}

/// A writer that took less than the frame is Go's `io.ErrShortWrite`.
fn write_error_text(error: &io::Error) -> String {
    if error.kind() == io::ErrorKind::WriteZero {
        "short write".to_owned()
    } else {
        error.to_string()
    }
}

#[cfg(test)]
mod tests;
