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

// Adapted from swarm-scheduler-exporter/src/cli.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The command line: Go's two flag sets, the daemon's and `launch`'s, double-dash only (Q1).
//!
//! Go's `flag` read `-name` and `--name` alike. Here only `--name` is accepted: [`parse`] and
//! [`parse_launch`] refuse every single-dash argument longer than one character before clap sees
//! it (clap alone would read `-unknown` as the short cluster `-u` and print the help for `-hx`),
//! and a positional argument is clap's error; both exit 2. `-h` and `--help` print the help to
//! stdout and exit 0. Kept from Go: the names, defaults and help texts, last value wins,
//! `--dry-run=false` and every `strconv.ParseBool` spelling, the repeatable device lists, which
//! flags were given on the command line (Go's `flag.Visit`, which counts a value equal to the
//! default), and the `launch` split at the first `--`, after which everything goes to the daemon
//! verbatim. The values themselves are checked later, as Go checks them: the daemon's after the
//! config file is merged in ([`settings`]), the launcher's in [`parse_launch`], with exit 1.

pub mod settings;

use std::collections::BTreeSet;
use std::ffi::OsString;

use clap::parser::ValueSource;
use clap::{ArgAction, CommandFactory as _, FromArgMatches as _, Parser};

use crate::config;
use crate::go_compat::parse_bool::parse_bool;
use crate::go_compat::{filepath, quote};
use crate::launcher::spec::DEFAULT_HOST_DOCKER_SOCKET;

/// The first argument that selects the launcher, Go's `launchCommand`.
pub const LAUNCH_COMMAND: &str = "launch";

/// The help of `--policy-mode`, Go's.
const POLICY_MODE_HELP: &str = "Container processing mode: \"opt-in\" processes only containers \
                                with label swarm-device-access.enable=true; \"all\" processes all \
                                containers unless label swarm-device-access.enable=false.";

/// The daemon's flags.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(
    name = "swarm-device-access",
    bin_name = "swarm-device-access",
    // Last value wins, as with Go's flag package.
    args_override_self = true,
    disable_version_flag = true,
    // Go's -help text replaces clap's.
    disable_help_flag = true,
    // The help is the flag list only, as Go's was; not this type's doc comment.
    about = None,
    long_about = None,
    override_usage = "swarm-device-access [OPTIONS]\n       \
                      swarm-device-access launch [OPTIONS] -- [daemon flags]  (see launch --help)"
)]
pub struct Args {
    #[arg(
        long,
        default_value = "text",
        allow_hyphen_values = true,
        value_name = "string",
        help = "Either json, text or plain"
    )]
    pub log_format: String,

    #[arg(
        long,
        default_value = "info",
        allow_hyphen_values = true,
        value_name = "string",
        help = "Either debug, info, warn or error"
    )]
    pub log_level: String,

    #[arg(
        long,
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_value = "false",
        default_missing_value = "true",
        value_parser = parse_bool,
        value_name = "BOOL",
        hide_default_value = true,
        help = "Include timestamp in logs"
    )]
    pub log_time: bool,

    #[arg(
        long,
        default_value = "/var/run/docker.sock",
        allow_hyphen_values = true,
        value_name = "string",
        help = "Path to the Docker UNIX socket"
    )]
    pub docker_socket: String,

    #[arg(
        long,
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_value = "false",
        default_missing_value = "true",
        value_parser = parse_bool,
        value_name = "BOOL",
        hide_default_value = true,
        help = "Log device rules that would be applied without writing to the cgroup"
    )]
    pub dry_run: bool,

    #[arg(
        long,
        default_value = "opt-in",
        allow_hyphen_values = true,
        value_name = "string",
        help = POLICY_MODE_HELP
    )]
    pub policy_mode: String,

    /// The allow globs, in the order given.
    #[arg(
        long,
        action = ArgAction::Append,
        allow_hyphen_values = true,
        value_name = "value",
        help = "Glob pattern for /dev/... paths to allow (repeatable). Empty means allow all."
    )]
    pub device_allow: Vec<String>,

    /// The deny globs, in the order given.
    #[arg(
        long,
        action = ArgAction::Append,
        allow_hyphen_values = true,
        value_name = "value",
        help = "Glob pattern for /dev/... paths to deny (repeatable; takes priority over \
                --device-allow)."
    )]
    pub device_deny: Vec<String>,

    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        allow_hyphen_values = true,
        value_name = "string",
        help = "Address for the Prometheus metrics + health endpoints (e.g. :9090). Empty = \
                disabled."
    )]
    pub metrics_addr: String,

    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        allow_hyphen_values = true,
        value_name = "string",
        help = "Removed in 2.0, with the pprof debug server: any value but the empty one is \
                refused."
    )]
    pub debug_addr: String,

    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        allow_hyphen_values = true,
        value_name = "string",
        help = "Path to a YAML config file. CLI flags override file values. Reload with SIGHUP."
    )]
    pub config: String,

    #[arg(short = 'h', long, action = ArgAction::Help, help = "Display help message")]
    help: Option<bool>,
}

/// The daemon's flags that take a separate value: the argument after one of them, without `=`,
/// is its value, even when it starts with a dash (`--device-allow -dry-run`).
const VALUE_FLAGS: [&str; 9] = [
    "log-format",
    "log-level",
    "docker-socket",
    "policy-mode",
    "device-allow",
    "device-deny",
    "metrics-addr",
    "debug-addr",
    "config",
];

/// The daemon's flags that take no separate value.
const BOOL_FLAGS: [&str; 3] = ["log-time", "dry-run", "help"];

/// The launcher's flags.
#[derive(Debug, Clone, PartialEq, Eq, Parser)]
#[command(
    name = "swarm-device-access launch",
    bin_name = "swarm-device-access launch",
    args_override_self = true,
    disable_version_flag = true,
    disable_help_flag = true,
    about = None,
    long_about = None,
    override_usage = "swarm-device-access launch [OPTIONS] -- [daemon flags]"
)]
pub struct LaunchArgs {
    #[arg(
        long,
        default_value = "text",
        allow_hyphen_values = true,
        value_name = "string",
        help = "Launcher log format: json, text or plain"
    )]
    pub log_format: String,

    #[arg(
        long,
        default_value = "info",
        allow_hyphen_values = true,
        value_name = "string",
        help = "Launcher log level: debug, info, warn or error"
    )]
    pub log_level: String,

    #[arg(
        long,
        default_value = DEFAULT_HOST_DOCKER_SOCKET,
        allow_hyphen_values = true,
        value_name = "string",
        help = "Host path of the Docker socket bound into the daemon"
    )]
    pub host_docker_socket: String,

    #[arg(
        long,
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_value = "false",
        default_missing_value = "true",
        value_parser = parse_bool,
        value_name = "BOOL",
        hide_default_value = true,
        help = "Bind the host's systemd DBus socket into the daemon, enabling daemon-reload \
                handling"
    )]
    pub dbus: bool,

    #[arg(
        long,
        default_value = "",
        hide_default_value = true,
        allow_hyphen_values = true,
        value_name = "string",
        help = "Host directory bound read-only at /etc/swarm-device-access in the daemon. Empty = \
                none."
    )]
    pub config_dir: String,

    #[arg(
        long,
        action = ArgAction::Set,
        num_args = 0..=1,
        require_equals = true,
        default_value = "false",
        default_missing_value = "true",
        value_parser = parse_bool,
        value_name = "BOOL",
        hide_default_value = true,
        help = "Run the daemon in the host network (needed for --metrics-addr); default is no \
                network"
    )]
    pub host_network: bool,

    #[arg(short = 'h', long, action = ArgAction::Help, help = "Display help message")]
    help: Option<bool>,
}

/// The launcher's flags that take a separate value.
const LAUNCH_VALUE_FLAGS: [&str; 4] = [
    "log-format",
    "log-level",
    "host-docker-socket",
    "config-dir",
];

/// The launcher's flags that take no separate value.
const LAUNCH_BOOL_FLAGS: [&str; 3] = ["dbus", "host-network", "help"];

/// Why parsing stopped: the text to print, where, and the exit code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exit {
    /// The exit code: 0 after the help, 2 for a parse error, 1 for a launch flag Go refused after
    /// parsing.
    pub code: i32,
    /// Whether the text goes to stdout (the help) rather than stderr.
    pub stdout: bool,
    /// The text, newline included.
    pub message: String,
}

/// The daemon's flags, and the names of those given on the command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Parsed {
    /// The flags, defaults included.
    pub args: Args,
    /// The long names of the flags given on the command line, Go's `flag.Visit`.
    pub set: BTreeSet<String>,
}

/// The launcher's flags and what goes to the daemon.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Launch {
    /// The launcher's flags.
    pub args: LaunchArgs,
    /// Everything after the first `--`, verbatim; empty without one.
    pub daemon_args: Vec<String>,
}

/// Parses the daemon's arguments, the program name first.
///
/// # Errors
///
/// [`Exit`] after the help (code 0), or for a single-dash flag, an unknown flag, a positional
/// argument or an invalid bool (code 2).
pub fn parse<I, T>(arguments: I) -> Result<Parsed, Exit>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let arguments: Vec<OsString> = arguments.into_iter().map(Into::into).collect();
    refuse_single_dash(&arguments, &VALUE_FLAGS, &BOOL_FLAGS)?;
    let matches = Args::command()
        .try_get_matches_from(&arguments)
        .map_err(|error| clap_exit(&error))?;
    let set = Args::command()
        .get_arguments()
        .filter(|arg| matches.value_source(arg.get_id().as_str()) == Some(ValueSource::CommandLine))
        .filter_map(|arg| arg.get_long().map(str::to_owned))
        .collect();
    let args = Args::from_arg_matches(&matches).map_err(|error| clap_exit(&error))?;
    Ok(Parsed { args, set })
}

/// Parses the arguments after `launch`: Go's `parseLaunchArgs`. Everything after the first `--`
/// goes to the daemon verbatim; the launcher's own log flags and host paths are checked here.
///
/// # Errors
///
/// [`Exit`] after the help (code 0), for a parse error (code 2), or for a log flag Go's
/// `ValidateEnums` refuses or a host path that is not absolute and clean (code 1, as Go's).
pub fn parse_launch<I, T>(arguments: I) -> Result<Launch, Exit>
where
    I: IntoIterator<Item = T>,
    T: Into<OsString>,
{
    let mut arguments: Vec<OsString> = std::iter::once(OsString::from(LAUNCH_COMMAND))
        .chain(arguments.into_iter().map(Into::into))
        .collect();
    let daemon_args = match arguments.iter().position(|argument| argument == "--") {
        Some(separator) => {
            let daemon: Vec<String> = arguments[separator + 1..]
                .iter()
                .map(|argument| argument.to_string_lossy().into_owned())
                .collect();
            arguments.truncate(separator);
            daemon
        }
        None => Vec::new(),
    };
    refuse_single_dash(&arguments, &LAUNCH_VALUE_FLAGS, &LAUNCH_BOOL_FLAGS)?;
    let args = LaunchArgs::try_parse_from(&arguments).map_err(|error| clap_exit(&error))?;
    validate_launch(&args).map_err(|message| Exit {
        code: 1,
        stdout: false,
        message: format!("{message}\n"),
    })?;
    Ok(Launch { args, daemon_args })
}

/// Go's `launchSettings.validate`: the log flags, then the Docker socket's host path, then the
/// config directory's when one is given.
fn validate_launch(args: &LaunchArgs) -> Result<(), String> {
    config::validate_enums(&args.log_format, &args.log_level, "", "flag")
        .map_err(|error| format!("launch: {error}"))?;
    check_host_path("host-docker-socket", &args.host_docker_socket)?;
    if !args.config_dir.is_empty() {
        check_host_path("config-dir", &args.config_dir)?;
    }
    Ok(())
}

/// Go's `checkHostPath`: a host path Docker would resolve differently from what the operator
/// reads, a relative one or one with `.` or `..` elements, is refused. The flag is named by its
/// `--` spelling (Q1).
fn check_host_path(name: &str, path: &str) -> Result<(), String> {
    if path.starts_with('/') && filepath::clean(path) == path {
        return Ok(());
    }
    Err(format!(
        "launch: flag {}: {} must be an absolute, clean path",
        quote::quote(&format!("--{name}")),
        quote::quote(path)
    ))
}

/// clap's error or help, as an [`Exit`].
fn clap_exit(error: &clap::Error) -> Exit {
    Exit {
        code: error.exit_code(),
        stdout: !error.use_stderr(),
        message: error.render().to_string(),
    }
}

/// The single-dash pre-scan: every argument up to `--` that starts with one dash and is longer
/// than one character is refused, naming the `--` spelling when it is a known flag. The value
/// after a value flag without `=` is skipped, since it may start with a dash.
fn refuse_single_dash(
    arguments: &[OsString],
    value_flags: &[&str],
    bool_flags: &[&str],
) -> Result<(), Exit> {
    let refuse = |message: String| Exit {
        code: 2,
        stdout: false,
        message: format!("error: {message}\n"),
    };
    let mut skip_value = false;
    for argument in arguments.iter().skip(1) {
        if skip_value {
            skip_value = false;
            continue;
        }
        let argument = argument.to_string_lossy();
        if argument == "--" {
            break;
        }
        if let Some(rest) = argument.strip_prefix("--") {
            skip_value = !rest.contains('=') && value_flags.contains(&rest);
            continue;
        }
        let Some(rest) = argument.strip_prefix('-') else {
            continue;
        };
        let name = rest.split_once('=').map_or(rest, |(name, _)| name);
        if value_flags.contains(&name) || bool_flags.contains(&name) {
            return Err(refuse(format!(
                "single-dash long flags are not supported: '-{name}'; use '--{name}'"
            )));
        }
        // -h is the only short flag; anything longer, a value after = included (clap would run
        // -h=false as the help), is a Go-style flag.
        if rest.chars().count() > 1 {
            return Err(refuse(format!("flag provided but not defined: '-{name}'")));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
