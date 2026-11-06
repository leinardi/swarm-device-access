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

//! The identity rule the suite applies before any test: the image (and the binary) under test
//! must be the Rust daemon, never a Go build standing in for it. Go's `flag` package takes
//! `-help`, prints its usage to stderr and exits 0; the Rust daemon refuses every single-dash long
//! flag with exit 2 and a refusal naming the `--` spelling (Q1 in `docs/rust-port.md`). So the
//! answer to `-help` tells the two apart, and anything but the Rust answer fails the run: a
//! mismatch is never a skip.
//!
//! Inverted ([`check_go_identity`]), the same answer proves that the Go image the cross-language
//! upgrade test runs is Go: never a Go image standing in for Rust, nor the reverse.
//!
//! The flag, the matching of a whole answer, the excerpt and the ELF check are testenv-core's
//! `identity`; the two answers and the messages are this suite's.

use go_compat::quote;
use testenv_core::Error;
use testenv_core::identity::{Answer, IDENTITY_FLAG, Quoting, Text, excerpt};

/// The exit code the Rust daemon refuses a single-dash long flag with: clap's usage error.
pub const RUST_REFUSAL_EXIT_CODE: i64 = 2;

/// What the Rust daemon writes to stderr for [`IDENTITY_FLAG`], byte for byte
/// (`src/cli.rs`, `refuse_single_dash`); stdout stays empty.
pub const RUST_REFUSAL: &str =
    "error: single-dash long flags are not supported: '-help'; use '--help'\n";

/// The Rust daemon's whole answer: exit [`RUST_REFUSAL_EXIT_CODE`], nothing on stdout, exactly
/// [`RUST_REFUSAL`] on stderr.
pub const RUST_ANSWER: Answer = Answer {
    exit_code: RUST_REFUSAL_EXIT_CODE,
    stdout: Text::Empty,
    stderr: Text::Exact(RUST_REFUSAL),
};

/// Checks the answer `subject` (an image or a binary, as the message names it) gave to
/// [`IDENTITY_FLAG`]: [`RUST_ANSWER`].
///
/// # Errors
///
/// Any other answer, naming `subject`, what it answered and what the Rust daemon answers.
pub fn check_identity(
    subject: &str,
    exit_code: i64,
    stdout: &str,
    stderr: &str,
) -> Result<(), Error> {
    if RUST_ANSWER.matches(Some(exit_code), stdout, stderr) {
        return Ok(());
    }
    Err(Error::new(format!(
        "{subject} is not the Rust daemon: {IDENTITY_FLAG} exited {exit_code} with stdout {} and \
         stderr {}, want exit {RUST_REFUSAL_EXIT_CODE} with {} on stderr alone (a Go build \
         prints its usage and exits 0)",
        excerpt(stdout, Quoting::Go),
        excerpt(stderr, Quoting::Go),
        quote::quote(RUST_REFUSAL)
    )))
}

/// The exit code the Go daemon answers [`IDENTITY_FLAG`] with: its flag package prints the usage
/// and exits 0.
pub const GO_HELP_EXIT_CODE: i64 = 0;

/// How the Go daemon's usage starts on stderr (`run` in `cmd/swarm-device-access/main.go`).
pub const GO_USAGE_PREFIX: &str = "Usage: swarm-device-access [flags]\n";

/// The Go daemon's whole answer: exit [`GO_HELP_EXIT_CODE`], nothing on stdout, its usage on
/// stderr ([`GO_USAGE_PREFIX`], then the flags).
pub const GO_ANSWER: Answer = Answer {
    exit_code: GO_HELP_EXIT_CODE,
    stdout: Text::Empty,
    stderr: Text::Prefix(GO_USAGE_PREFIX),
};

/// The identity check inverted, for the Go image the cross-language upgrade test needs: the
/// answer `subject` gave to [`IDENTITY_FLAG`] must be [`GO_ANSWER`]. The Rust refusal, or
/// anything else, is refused, so the Rust image can never stand in for the Go one.
///
/// # Errors
///
/// Any other answer, naming `subject`, what it answered and what the Go daemon answers.
pub fn check_go_identity(
    subject: &str,
    exit_code: i64,
    stdout: &str,
    stderr: &str,
) -> Result<(), Error> {
    if GO_ANSWER.matches(Some(exit_code), stdout, stderr) {
        return Ok(());
    }
    Err(Error::new(format!(
        "{subject} is not the Go daemon: {IDENTITY_FLAG} exited {exit_code} with stdout {} and \
         stderr {}, want exit {GO_HELP_EXIT_CODE} with stderr starting {} (the Rust daemon \
         refuses the flag with exit {RUST_REFUSAL_EXIT_CODE})",
        excerpt(stdout, Quoting::Go),
        excerpt(stderr, Quoting::Go),
        quote::quote(GO_USAGE_PREFIX)
    )))
}

#[cfg(test)]
mod tests {
    use super::{RUST_REFUSAL, RUST_REFUSAL_EXIT_CODE, check_go_identity, check_identity};

    /// What the Go daemon answers to `-help`, captured from `make go-build`'s binary (go1.26.10,
    /// at the commit before this harness): `dist/swarm-device-access -help`, which exited 0 with
    /// these two outputs.
    const GO_EXIT_CODE: i64 = 0;
    const GO_STDOUT: &str = include_str!("../testdata/go_dash_help.stdout");
    const GO_STDERR: &str = include_str!("../testdata/go_dash_help.stderr");

    /// The constant is the daemon's own refusal, as its parser produces it.
    #[test]
    fn the_refusal_is_the_daemons() {
        let Err(refused) = swarm_device_access::cli::parse(["swarm-device-access", "-help"]) else {
            panic!("the daemon accepted -help");
        };
        assert_eq!(i64::from(refused.code), RUST_REFUSAL_EXIT_CODE);
        assert!(!refused.stdout, "the refusal goes to stdout");
        assert_eq!(refused.message, RUST_REFUSAL);
    }

    #[test]
    fn the_rust_answer_is_accepted() {
        check_identity("image x", RUST_REFUSAL_EXIT_CODE, "", RUST_REFUSAL)
            .expect("the Rust answer was refused");
    }

    /// The negative control: the Go daemon's real answer is refused, naming the image.
    #[test]
    fn the_go_answer_is_refused() {
        assert!(GO_STDERR.starts_with("Usage: swarm-device-access [flags]\n"));
        let error = check_identity("image go:x", GO_EXIT_CODE, GO_STDOUT, GO_STDERR)
            .expect_err("the Go answer was accepted");
        assert_eq!(
            error.to_string(),
            concat!(
                "image go:x is not the Rust daemon: -help exited 0 with stdout \"\" and stderr ",
                "\"Usage: swarm-device-access [flags]\" (first line of 1285 bytes), want exit 2 ",
                "with \"error: single-dash long flags are not supported: '-help'; use '--help'\\n\" ",
                "on stderr alone (a Go build prints its usage and exits 0)"
            )
        );
    }

    /// Every part of the answer counts: the code, the text and an empty stdout.
    #[test]
    fn a_partial_match_is_refused() {
        for (name, exit_code, stdout, stderr) in [
            ("exit 0", 0, "", RUST_REFUSAL),
            ("exit 1", 1, "", RUST_REFUSAL),
            (
                "Go's own usage error",
                2,
                "",
                "flag provided but not defined: -x\n",
            ),
            ("no newline", 2, "", RUST_REFUSAL.trim_end()),
            ("output on stdout", 2, "Usage:\n", RUST_REFUSAL),
            ("nothing", 2, "", ""),
            ("the refusal twice", 2, "", &RUST_REFUSAL.repeat(2)),
        ] {
            assert!(
                check_identity("image x", exit_code, stdout, stderr).is_err(),
                "{name} was accepted"
            );
        }
    }

    /// The inverted check accepts the Go daemon's real answer.
    #[test]
    fn the_go_answer_is_accepted_as_go() {
        check_go_identity("image go:x", GO_EXIT_CODE, GO_STDOUT, GO_STDERR)
            .expect("the Go answer was refused as Go");
    }

    /// The inverted check's negative control: the Rust refusal is refused, naming the image, and
    /// so is a partial match of the Go answer.
    #[test]
    fn the_rust_answer_is_refused_as_go() {
        let error = check_go_identity("image rust:x", RUST_REFUSAL_EXIT_CODE, "", RUST_REFUSAL)
            .expect_err("the Rust answer was accepted as Go");
        assert_eq!(
            error.to_string(),
            concat!(
                "image rust:x is not the Go daemon: -help exited 2 with stdout \"\" and stderr ",
                "\"error: single-dash long flags are not supported: '-help'; use '--help'\\n\", ",
                "want exit 0 with stderr starting \"Usage: swarm-device-access [flags]\\n\" (the ",
                "Rust daemon refuses the flag with exit 2)"
            )
        );
        for (name, exit_code, stdout, stderr) in [
            ("exit 2", 2, GO_STDOUT, GO_STDERR),
            ("usage on stdout", 0, GO_STDERR, ""),
            ("output on both", 0, "x\n", GO_STDERR),
            ("another usage", 0, "", "Usage of swarm-device-access:\n"),
            ("nothing", 0, "", ""),
        ] {
            assert!(
                check_go_identity("image x", exit_code, stdout, stderr).is_err(),
                "{name} was accepted as Go"
            );
        }
    }
}
