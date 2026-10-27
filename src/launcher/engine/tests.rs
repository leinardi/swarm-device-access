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

//! The decoding of the wait answer, as moby's wait goroutine reads it. The requests are checked
//! on the wire by `tests/launcher_wire.rs`.

use super::{Decoded, WaitExitError, WaitResponse, decode_wait};

/// A complete answer decodes at once, without waiting for the end of the body.
#[test]
fn decode_wait_answer() {
    match decode_wait(b"{\"StatusCode\":3,\"Error\":{\"Message\":\"oops\"}}\ntrailing") {
        Decoded::Value(response) => assert_eq!(
            response,
            WaitResponse {
                status_code: 3,
                error: Some(WaitExitError {
                    message: "oops".to_owned(),
                }),
            }
        ),
        Decoded::Incomplete | Decoded::Syntax | Decoded::Other(_) => panic!("not decoded"),
    }
    assert!(matches!(
        decode_wait(b"{\"StatusCode\":null,\"Error\":null}"),
        Decoded::Value(WaitResponse {
            status_code: 0,
            error: None
        })
    ));
}

/// An answer cut short waits for more; one that is not JSON is a syntax error, reported by its
/// text; a wrong type is the decoder's error.
#[test]
fn decode_wait_failures() {
    assert!(matches!(decode_wait(b""), Decoded::Incomplete));
    assert!(matches!(decode_wait(b"{\"StatusCo"), Decoded::Incomplete));
    assert!(matches!(decode_wait(b"proxy error"), Decoded::Syntax));
    // Q10: serde's wording, where Go has encoding/json's.
    assert!(matches!(
        decode_wait(b"{\"StatusCode\":\"x\"}"),
        Decoded::Other(text)
            if text == "invalid type: string \"x\", expected i64 at line 1 column 17"
    ));
}
