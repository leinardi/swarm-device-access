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

//! Go's `bufio.Scanner` with `ScanLines` and the default buffer, which the cgroup code reads
//! `/proc/<pid>/cgroup`, `/proc/<pid>/mountinfo` and `devices.list` with: lines split at `\n`, one
//! trailing `\r` dropped, no empty token after a final newline, and the scan failing with
//! `bufio.Scanner: token too long` on a line of 64 KiB or more, after the lines before it.

use std::fmt;

/// `bufio.MaxScanTokenSize`: a line this long, without its newline, does not fit the buffer.
pub const MAX_SCAN_TOKEN_SIZE: usize = 64 * 1024;

/// `bufio.ErrTooLong`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TokenTooLong;

impl fmt::Display for TokenTooLong {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("bufio.Scanner: token too long")
    }
}

impl std::error::Error for TokenTooLong {}

/// The lines of `data`, as `Scanner.Scan` with `ScanLines` yields them; after an error, nothing.
#[must_use]
pub fn lines(data: &[u8]) -> Lines<'_> {
    Lines { rest: data }
}

/// The iterator [`lines`] returns.
#[derive(Debug, Clone)]
pub struct Lines<'data> {
    rest: &'data [u8],
}

impl<'data> Iterator for Lines<'data> {
    type Item = Result<&'data [u8], TokenTooLong>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let (line, consumed) = match self.rest.iter().position(|byte| *byte == b'\n') {
            // The line and its newline fit the buffer.
            Some(newline) if newline < MAX_SCAN_TOKEN_SIZE => (&self.rest[..newline], newline + 1),
            // The final line, without a newline, fits.
            None if self.rest.len() < MAX_SCAN_TOKEN_SIZE => (self.rest, self.rest.len()),
            Some(_) | None => {
                self.rest = &[];
                return Some(Err(TokenTooLong));
            }
        };
        self.rest = &self.rest[consumed..];
        Some(Ok(line.strip_suffix(b"\r").unwrap_or(line)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan(data: &[u8]) -> (Vec<String>, Option<TokenTooLong>) {
        let mut tokens = Vec::new();
        for line in lines(data) {
            match line {
                Ok(line) if line.len() > 20 => tokens.push(format!("<{} bytes>", line.len())),
                Ok(line) => tokens.push(String::from_utf8(line.to_vec()).unwrap()),
                Err(error) => return (tokens, Some(error)),
            }
        }
        (tokens, None)
    }

    /// What Go 1.26's `bufio.Scanner` yields for the same inputs (a scratch Go program).
    #[test]
    fn lines_split_as_go_s_scanner() {
        let long = "a".repeat(MAX_SCAN_TOKEN_SIZE - 1);
        let longer = "a".repeat(MAX_SCAN_TOKEN_SIZE);
        let cases: [(String, &[&str], bool); 10] = [
            ("a\r\nb\r\r\nc\r".to_owned(), &["a", "b\r", "c"], false),
            ("a\nb\n".to_owned(), &["a", "b"], false),
            (String::new(), &[], false),
            ("\n".to_owned(), &[""], false),
            ("\n\nx".to_owned(), &["", "", "x"], false),
            (format!("{long}\nz"), &["<65535 bytes>", "z"], false),
            (long.clone(), &["<65535 bytes>"], false),
            (longer.clone(), &[], true),
            (format!("{longer}\nz"), &[], true),
            (format!("ok\n{longer}"), &["ok"], true),
        ];
        for (data, tokens, too_long) in cases {
            let (got, error) = scan(data.as_bytes());
            assert_eq!(got, tokens, "{:?}", &data[..data.len().min(20)]);
            assert_eq!(error.is_some(), too_long);
        }
        assert_eq!(TokenTooLong.to_string(), "bufio.Scanner: token too long");
    }
}
