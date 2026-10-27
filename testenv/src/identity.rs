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
//! Also here, since it needs no daemon either: whether an ELF binary is static, which a binary
//! copied into a `FROM scratch` image has to be.

use swarm_device_access::go_compat::quote;

use crate::Error;

/// The argument the check runs the daemon with.
pub const IDENTITY_FLAG: &str = "-help";

/// The exit code the Rust daemon refuses a single-dash long flag with: clap's usage error.
pub const RUST_REFUSAL_EXIT_CODE: i64 = 2;

/// What the Rust daemon writes to stderr for [`IDENTITY_FLAG`], byte for byte
/// (`src/cli.rs`, `refuse_single_dash`); stdout stays empty.
pub const RUST_REFUSAL: &str =
    "error: single-dash long flags are not supported: '-help'; use '--help'\n";

/// Checks the answer `subject` (an image or a binary, as the message names it) gave to
/// [`IDENTITY_FLAG`]: exit [`RUST_REFUSAL_EXIT_CODE`], [`RUST_REFUSAL`] on stderr and nothing on
/// stdout.
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
    if exit_code == RUST_REFUSAL_EXIT_CODE && stderr == RUST_REFUSAL && stdout.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "{subject} is not the Rust daemon: {IDENTITY_FLAG} exited {exit_code} with stdout {} and \
         stderr {}, want exit {RUST_REFUSAL_EXIT_CODE} with {} on stderr alone (a Go build \
         prints its usage and exits 0)",
        excerpt(stdout),
        excerpt(stderr),
        quote::quote(RUST_REFUSAL)
    )))
}

/// The exit code the Go daemon answers [`IDENTITY_FLAG`] with: its flag package prints the usage
/// and exits 0.
pub const GO_HELP_EXIT_CODE: i64 = 0;

/// How the Go daemon's usage starts on stderr (`run` in `cmd/swarm-device-access/main.go`).
pub const GO_USAGE_PREFIX: &str = "Usage: swarm-device-access [flags]\n";

/// The identity check inverted, for the Go image the cross-language upgrade test needs: the
/// answer `subject` gave to [`IDENTITY_FLAG`] must be the Go daemon's (exit
/// [`GO_HELP_EXIT_CODE`], its usage on stderr, nothing on stdout). The Rust refusal, or anything
/// else, is refused, so the Rust image can never stand in for the Go one.
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
    if exit_code == GO_HELP_EXIT_CODE && stderr.starts_with(GO_USAGE_PREFIX) && stdout.is_empty() {
        return Ok(());
    }
    Err(Error::new(format!(
        "{subject} is not the Go daemon: {IDENTITY_FLAG} exited {exit_code} with stdout {} and \
         stderr {}, want exit {GO_HELP_EXIT_CODE} with stderr starting {} (the Rust daemon \
         refuses the flag with exit {RUST_REFUSAL_EXIT_CODE})",
        excerpt(stdout),
        excerpt(stderr),
        quote::quote(GO_USAGE_PREFIX)
    )))
}

/// The ELF program header type of the dynamic loader's path: a binary that has one needs a loader
/// (and its libraries) that a `FROM scratch` image does not have.
const PT_INTERP: u32 = 3;

/// Whether the ELF binary `bytes` is static: it has no `PT_INTERP` program header, as a static or
/// static-pie binary has none.
///
/// # Errors
///
/// `bytes` is not a 64-bit little-endian ELF file (the two host architectures the suite builds
/// for), or its program headers lie outside it.
pub fn is_static_elf(bytes: &[u8]) -> Result<bool, Error> {
    let not_elf = || Error::new("not a 64-bit little-endian ELF file");
    if bytes.get(..4) != Some(b"\x7fELF".as_slice())
        || bytes.get(4) != Some(&2)
        || bytes.get(5) != Some(&1)
    {
        return Err(not_elf());
    }
    let table = read_u64(bytes, 0x20).ok_or_else(not_elf)?;
    let entry_size = read_u16(bytes, 0x36).ok_or_else(not_elf)?;
    let entries = read_u16(bytes, 0x38).ok_or_else(not_elf)?;
    let table = usize::try_from(table).map_err(|_| not_elf())?;
    for index in 0..usize::from(entries) {
        let offset = index
            .checked_mul(usize::from(entry_size))
            .and_then(|relative| relative.checked_add(table))
            .ok_or_else(not_elf)?;
        let kind = read_u32(bytes, offset)
            .ok_or_else(|| Error::new(format!("program header {index} lies outside the file")))?;
        if kind == PT_INTERP {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The first line of `text`, quoted, with the byte count when there is more.
fn excerpt(text: &str) -> String {
    let first = text.lines().next().unwrap_or_default();
    if first.len() + 1 >= text.len() {
        return quote::quote(text);
    }
    format!(
        "{} (first line of {} bytes)",
        quote::quote(first),
        text.len()
    )
}

fn read_u16(bytes: &[u8], offset: usize) -> Option<u16> {
    let end = offset.checked_add(2)?;
    bytes
        .get(offset..end)?
        .try_into()
        .ok()
        .map(u16::from_le_bytes)
}

fn read_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    let end = offset.checked_add(4)?;
    bytes
        .get(offset..end)?
        .try_into()
        .ok()
        .map(u32::from_le_bytes)
}

fn read_u64(bytes: &[u8], offset: usize) -> Option<u64> {
    let end = offset.checked_add(8)?;
    bytes
        .get(offset..end)?
        .try_into()
        .ok()
        .map(u64::from_le_bytes)
}

#[cfg(test)]
mod tests {
    use super::{
        RUST_REFUSAL, RUST_REFUSAL_EXIT_CODE, check_go_identity, check_identity, is_static_elf,
    };

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
        let message = error.to_string();
        assert!(
            message.starts_with(concat!(
                "image go:x is not the Rust daemon: -help exited 0 with stdout \"\" and stderr ",
                "\"Usage: swarm-device-access [flags]\" (first line of 1285 bytes)"
            )),
            "{message}"
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
        assert!(
            error
                .to_string()
                .starts_with("image rust:x is not the Go daemon: -help exited 2"),
            "{error}"
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

    /// A minimal ELF64 header with `kinds` as its program header types.
    fn elf(kinds: &[u32]) -> Vec<u8> {
        let mut bytes = vec![0_u8; 64];
        bytes[..6].copy_from_slice(b"\x7fELF\x02\x01");
        bytes[0x20..0x28].copy_from_slice(&64_u64.to_le_bytes());
        bytes[0x36..0x38].copy_from_slice(&56_u16.to_le_bytes());
        let count = u16::try_from(kinds.len()).expect("too many headers");
        bytes[0x38..0x3a].copy_from_slice(&count.to_le_bytes());
        for kind in kinds {
            let mut header = vec![0_u8; 56];
            header[..4].copy_from_slice(&kind.to_le_bytes());
            bytes.extend(header);
        }
        bytes
    }

    #[test]
    fn a_loader_makes_a_binary_dynamic() {
        assert!(is_static_elf(&elf(&[6, 1, 1])).expect("parse"));
        assert!(!is_static_elf(&elf(&[6, 3, 1])).expect("parse"));
        assert!(is_static_elf(&elf(&[])).expect("parse"));
    }

    #[test]
    fn what_is_not_an_elf64_file_is_refused() {
        assert!(is_static_elf(b"#!/bin/sh\n").is_err());
        assert!(is_static_elf(&[]).is_err());
        let mut thirty_two_bit = elf(&[1]);
        thirty_two_bit[4] = 1;
        assert!(is_static_elf(&thirty_two_bit).is_err());
        let mut truncated = elf(&[1, 1]);
        truncated.truncate(64 + 56);
        assert!(is_static_elf(&truncated).is_err());
    }

    /// The real-file control: this test binary, linked against glibc, names its loader.
    #[cfg(target_env = "gnu")]
    #[test]
    fn this_glibc_test_binary_is_dynamic() {
        let path = std::env::current_exe().expect("current exe");
        let bytes = std::fs::read(&path).expect("read the test binary");
        assert!(!is_static_elf(&bytes).expect("parse"), "{}", path.display());
    }
}
