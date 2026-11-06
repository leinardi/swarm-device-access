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

//! Text helpers shared by the error messages.

/// `text` between double quotes with its quotes, backslashes and control characters escaped: the
/// `{:?}` form, which is what stands for Go's `%q` in the daemon's messages (the two differ only
/// for characters Go prints as `\x..` or `\u....` and Rust as `\u{..}`).
#[must_use]
pub fn quote(text: &str) -> String {
    format!("{text:?}")
}

/// The text of an I/O error: the standard library's, `No such file or directory (os error 2)`.
#[must_use]
pub fn io_error_text(error: &std::io::Error) -> String {
    error.to_string()
}

/// The text of an `errno`, as the standard library words it.
#[must_use]
pub fn errno_string(errno: i32) -> String {
    std::io::Error::from_raw_os_error(errno).to_string()
}

/// The longest line, without its newline, [`lines`] yields: longer data is an error, never a
/// silently shortened line (the cgroup and `/proc` data these read decide device grants).
pub const MAX_LINE: usize = 64 * 1024;

/// A line of [`MAX_LINE`] bytes or more.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LineTooLong;

impl std::fmt::Display for LineTooLong {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "line longer than {MAX_LINE} bytes")
    }
}

impl std::error::Error for LineTooLong {}

/// The lines of `data` as bytes, each without its `\n` and one trailing `\r`; the first line that
/// is too long, with its newline, ends the iteration with an error.
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
    type Item = Result<&'data [u8], LineTooLong>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.rest.is_empty() {
            return None;
        }
        let (line, consumed) = match self.rest.iter().position(|byte| *byte == b'\n') {
            Some(newline) if newline < MAX_LINE => (&self.rest[..newline], newline + 1),
            None if self.rest.len() < MAX_LINE => (self.rest, self.rest.len()),
            Some(_) | None => {
                self.rest = &[];
                return Some(Err(LineTooLong));
            }
        };
        self.rest = &self.rest[consumed..];
        Some(Ok(line.strip_suffix(b"\r").unwrap_or(line)))
    }
}

#[cfg(test)]
mod tests {
    use super::{LineTooLong, MAX_LINE, lines};

    fn scan(data: &[u8]) -> (Vec<String>, Option<LineTooLong>) {
        let mut seen = Vec::new();
        for line in lines(data) {
            match line {
                Ok(line) if line.len() > 20 => seen.push(format!("<{} bytes>", line.len())),
                Ok(line) => seen.push(String::from_utf8(line.to_vec()).unwrap()),
                Err(error) => return (seen, Some(error)),
            }
        }
        (seen, None)
    }

    #[test]
    fn lines_split_and_refuse_a_line_that_is_too_long() {
        let long = "a".repeat(MAX_LINE - 1);
        let longer = "a".repeat(MAX_LINE);
        let cases: [(String, &[&str], bool); 9] = [
            ("a\r\nb\r\r\nc\r".to_owned(), &["a", "b\r", "c"], false),
            ("a\nb\n".to_owned(), &["a", "b"], false),
            (String::new(), &[], false),
            ("\n\nx".to_owned(), &["", "", "x"], false),
            (format!("{long}\nz"), &["<65535 bytes>", "z"], false),
            (long.clone(), &["<65535 bytes>"], false),
            (longer.clone(), &[], true),
            (format!("{longer}\nz"), &[], true),
            (format!("ok\n{longer}"), &["ok"], true),
        ];
        for (data, want, too_long) in cases {
            let (got, error) = scan(data.as_bytes());
            assert_eq!(got, want, "{:?}", &data[..data.len().min(20)]);
            assert_eq!(error.is_some(), too_long);
        }
    }
}
