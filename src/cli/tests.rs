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

// Adapted from swarm-scheduler-exporter/src/cli/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The flag parsing: swarm-scheduler-exporter's double-dash cases, adapted to this daemon's
//! flags, and the Go tests of `launch_test.go`.

use super::*;

/// The program name, then `arguments`.
fn parse_args(arguments: &[&str]) -> Result<Parsed, Exit> {
    parse(std::iter::once("swarm-device-access").chain(arguments.iter().copied()))
}

/// The first line of an exit's text.
fn first_line(exit: &Exit) -> &str {
    exit.message.lines().next().unwrap_or_default()
}

/// The defaults are Go's, and nothing is set on the command line.
#[test]
fn defaults_are_go_defaults() {
    let parsed = parse_args(&[]).unwrap();
    let args = &parsed.args;
    assert_eq!(args.log_format, "text");
    assert_eq!(args.log_level, "info");
    assert!(!args.log_time);
    assert_eq!(args.docker_socket, "/var/run/docker.sock");
    assert!(!args.dry_run);
    assert_eq!(args.policy_mode, "opt-in");
    assert!(args.device_allow.is_empty() && args.device_deny.is_empty());
    assert_eq!(args.metrics_addr, "");
    assert_eq!(args.debug_addr, "");
    assert_eq!(args.config, "");
    assert!(parsed.set.is_empty());
}

/// Q1: a single-dash flag is refused with exit 2, naming its double-dash spelling when it is a
/// known flag; `-h` and `--help` print the help to stdout and exit 0.
#[test]
fn single_dash_flags_are_refused() {
    for (arguments, code, line) in [
        (
            &["-dry-run"][..],
            2,
            "error: single-dash long flags are not supported: '-dry-run'; use '--dry-run'",
        ),
        (
            &["-log-format=json"][..],
            2,
            "error: single-dash long flags are not supported: '-log-format'; use '--log-format'",
        ),
        (
            &["-help"][..],
            2,
            "error: single-dash long flags are not supported: '-help'; use '--help'",
        ),
        (
            &["-unknown"][..],
            2,
            "error: flag provided but not defined: '-unknown'",
        ),
        (
            &["-hx"][..],
            2,
            "error: flag provided but not defined: '-hx'",
        ),
        (&["-h"][..], 0, "Usage: swarm-device-access [OPTIONS]"),
        (&["--help"][..], 0, "Usage: swarm-device-access [OPTIONS]"),
    ] {
        let exit = parse_args(arguments).unwrap_err();
        assert_eq!(
            (exit.code, first_line(&exit)),
            (code, line),
            "{arguments:?}"
        );
        assert_eq!(exit.stdout, code == 0, "{arguments:?}");
    }
}

/// A value flag's value may start with a dash; the scan stops at `--`, leaving what follows to
/// clap, which refuses it as a positional (Q1).
#[test]
fn values_and_positionals() {
    let parsed = parse_args(&["--device-allow", "-dry-run", "--config", "--x"]).unwrap();
    assert_eq!(parsed.args.device_allow, ["-dry-run"]);
    assert_eq!(parsed.args.config, "--x");
    assert!(!parsed.args.dry_run);
    let exit = parse_args(&["--", "-dry-run"]).unwrap_err();
    assert_eq!(exit.code, 2);
    assert!(
        exit.message.contains("unexpected argument '-dry-run'"),
        "{exit:?}"
    );
    let exit = parse_args(&["extra"]).unwrap_err();
    assert_eq!(exit.code, 2);
    assert!(
        exit.message.contains("unexpected argument 'extra'"),
        "{exit:?}"
    );
}

/// Bools take `=value` with every `strconv.ParseBool` spelling, and nothing after a space; the
/// last value wins.
#[test]
fn bools_are_go_parse_bool() {
    for (value, want) in [
        ("1", true),
        ("t", true),
        ("T", true),
        ("TRUE", true),
        ("true", true),
        ("True", true),
        ("0", false),
        ("f", false),
        ("F", false),
        ("FALSE", false),
        ("false", false),
        ("False", false),
    ] {
        let parsed = parse_args(&[&format!("--dry-run={value}")]).unwrap();
        assert_eq!(parsed.args.dry_run, want, "{value}");
    }
    assert!(parse_args(&["--dry-run"]).unwrap().args.dry_run);
    assert!(
        !parse_args(&["--dry-run", "--dry-run=false"])
            .unwrap()
            .args
            .dry_run
    );
    let exit = parse_args(&["--dry-run=yes"]).unwrap_err();
    assert_eq!(exit.code, 2);
    assert!(
        exit.message
            .contains("strconv.ParseBool: parsing \"yes\": invalid syntax"),
        "{exit:?}"
    );
    assert_eq!(parse_args(&["--dry-run", "false"]).unwrap_err().code, 2);
}

/// The device lists repeat in order; a string flag's last value wins.
#[test]
fn lists_repeat_and_strings_take_the_last_value() {
    let parsed = parse_args(&[
        "--device-allow=/dev/nvidia*",
        "--device-allow",
        "/dev/dri/*",
        "--device-deny=",
        "--log-level=warn",
        "--log-level=debug",
    ])
    .unwrap();
    assert_eq!(parsed.args.device_allow, ["/dev/nvidia*", "/dev/dri/*"]);
    assert_eq!(parsed.args.device_deny, [""]);
    assert_eq!(parsed.args.log_level, "debug");
}

/// Go's `flag.Visit`: a flag given on the command line is set, even with its default value.
#[test]
fn explicitly_set_counts_a_default_value() {
    let parsed = parse_args(&[
        "--log-format=text",
        "--dry-run=false",
        "--device-deny=/dev/sda",
    ])
    .unwrap();
    let set: Vec<&str> = parsed.set.iter().map(String::as_str).collect();
    assert_eq!(set, ["device-deny", "dry-run", "log-format"]);
}

/// The value checks are left to the merged settings: an unknown log format parses.
#[test]
fn values_are_checked_after_the_merge() {
    let parsed = parse_args(&["--log-format=xml", "--policy-mode=bogus"]).unwrap();
    assert_eq!(parsed.args.log_format, "xml");
    assert_eq!(parsed.args.policy_mode, "bogus");
}

/// The launcher's arguments.
fn launch(arguments: &[&str]) -> Result<Launch, Exit> {
    parse_launch(arguments.iter().copied())
}

/// Go: `TestParseLaunchArgs_SplitsAtDoubleDash`. Everything after the first `--` goes to the
/// daemon verbatim, later `--` included.
#[test]
fn parse_launch_args_splits_at_double_dash() {
    let parsed = launch(&[
        "--dbus",
        "--host-network",
        "--config-dir",
        "/srv/sda",
        "--log-format",
        "json",
        "--",
        "-dry-run",
        "--device-allow",
        "/dev/sd*",
        "--",
        "-x",
    ])
    .unwrap();
    assert_eq!(parsed.args.host_docker_socket, "/var/run/docker.sock");
    assert!(parsed.args.dbus);
    assert_eq!(parsed.args.config_dir, "/srv/sda");
    assert!(parsed.args.host_network);
    assert_eq!(
        parsed.daemon_args,
        ["-dry-run", "--device-allow", "/dev/sd*", "--", "-x"]
    );
    assert_eq!(parsed.args.log_format, "json");
    assert_eq!(parsed.args.log_level, "info");
}

/// Go: `TestParseLaunchArgs_NoDaemonArgs`.
#[test]
fn parse_launch_args_no_daemon_args() {
    let parsed = launch(&[]).unwrap();
    assert!(parsed.daemon_args.is_empty());
    assert!(!parsed.args.dbus);
    assert_eq!(parsed.args.config_dir, "");
}

/// Go: `TestParseLaunchArgs_Rejects`. The host paths and the log flags are Go's errors with exit
/// 1; a daemon flag before `--` and a stray argument are clap's, with exit 2 (Q1).
#[test]
fn parse_launch_args_rejects() {
    for (arguments, code, want) in [
        (
            &["--config-dir", "etc/sda"][..],
            1,
            "launch: flag \"--config-dir\": \"etc/sda\" must be an absolute, clean path",
        ),
        (
            &["--config-dir", "/srv/../etc"][..],
            1,
            "launch: flag \"--config-dir\": \"/srv/../etc\" must be an absolute, clean path",
        ),
        (
            &["--config-dir", "/srv/sda/"][..],
            1,
            "launch: flag \"--config-dir\": \"/srv/sda/\" must be an absolute, clean path",
        ),
        (
            &["--host-docker-socket", "docker.sock"][..],
            1,
            "launch: flag \"--host-docker-socket\": \"docker.sock\" must be an absolute, clean \
             path",
        ),
        (
            &["--host-docker-socket="][..],
            1,
            "launch: flag \"--host-docker-socket\": \"\" must be an absolute, clean path",
        ),
        (
            &["--dbus", "--dry-run"][..],
            2,
            "error: unexpected argument '--dry-run' found",
        ),
        (
            &["--dbus", "-dry-run"][..],
            2,
            "error: flag provided but not defined: '-dry-run'",
        ),
        (
            &["extra", "--", "--dry-run"][..],
            2,
            "error: unexpected argument 'extra' found",
        ),
        (
            &["--log-format", "xml"][..],
            1,
            "launch: flag \"log-format\": unknown value \"xml\", must be one of [text json plain]",
        ),
        (
            &["--log-level", "trace"][..],
            1,
            "launch: flag \"log-level\": unknown value \"trace\", must be one of [debug info \
             warn error]",
        ),
    ] {
        let exit = launch(arguments).unwrap_err();
        assert_eq!(
            (exit.code, first_line(&exit)),
            (code, want),
            "{arguments:?}"
        );
        assert!(!exit.stdout, "{arguments:?}");
    }
}

/// Go: `TestParseLaunchArgs_Help`. The help names the usage and the launcher's flags, on stdout
/// with exit 0 (Q1).
#[test]
fn parse_launch_args_help() {
    let exit = launch(&["--help"]).unwrap_err();
    assert_eq!(exit.code, 0);
    assert!(exit.stdout);
    for want in [
        "launch [OPTIONS] -- [daemon flags]",
        "--host-docker-socket",
        "--config-dir",
    ] {
        assert!(exit.message.contains(want), "{want}: {}", exit.message);
    }
}
