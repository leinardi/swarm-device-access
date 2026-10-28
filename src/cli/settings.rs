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

//! The daemon's settings and their reload, Go's `config.go`: what the flags and the config file
//! control, merged with the command line over the file over the default, validated before
//! anything uses them, and re-read on `SIGHUP`, where only the hot-reloadable part is applied, all
//! or nothing.
//!
//! Q2: a non-empty `debug-addr` fails validation, so it refuses the startup and rejects a reload.

use std::collections::BTreeSet;
use std::fmt;
use std::future::Future;
use std::pin::Pin;

use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::Parsed;
use crate::config::{self, FileSchema, LoadError, Runtime};
use crate::go_compat::errors;
use crate::logger::{Attr, LogHandle};
use crate::policy::Global;
use crate::server;
use crate::task::finish;

/// Go's `errDryRunEnable`.
const DRY_RUN_ENABLE: &str = "dry-run cannot be enabled on a running daemon; to stop enforcing, \
                              restart the daemon live with a narrower policy, or restart the \
                              affected containers";

/// Every value the flags and the config file control, Go's `settings`. `None` for a device list
/// is Go's nil slice: never given.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Settings {
    pub log_format: String,
    pub log_level: String,
    pub log_time: bool,
    pub docker_socket: String,
    pub metrics_addr: String,
    pub debug_addr: String,
    pub dry_run: bool,
    pub policy_mode: String,
    pub device_allow: Option<Vec<String>>,
    pub device_deny: Option<Vec<String>>,
}

/// Settings that failed validation: the errors of every check, joined as Go's `errors.Join`
/// joins them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvalidSettings(String);

impl fmt::Display for InvalidSettings {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl std::error::Error for InvalidSettings {}

impl Settings {
    /// Go's `flagSettings`: the parsed flags, defaults included; a device list not given on the
    /// command line is nil.
    #[must_use]
    pub fn from_flags(parsed: &Parsed) -> Self {
        let args = &parsed.args;
        let list =
            |name: &str, values: &[String]| parsed.set.contains(name).then(|| values.to_vec());
        Self {
            log_format: args.log_format.clone(),
            log_level: args.log_level.clone(),
            log_time: args.log_time,
            docker_socket: args.docker_socket.clone(),
            metrics_addr: args.metrics_addr.clone(),
            debug_addr: args.debug_addr.clone(),
            dry_run: args.dry_run,
            policy_mode: args.policy_mode.clone(),
            device_allow: list("device-allow", &args.device_allow),
            device_deny: list("device-deny", &args.device_deny),
        }
    }

    /// Go's `mergeSettings`: a flag given on the command line wins, then a key set in the file,
    /// then the flag's default (whatever `defaults` holds). It is pure, so a reload that drops a
    /// key from the file falls back to the command line or the default, not to what an earlier
    /// file said.
    #[must_use]
    pub fn merge(defaults: &Self, cli_set: &BTreeSet<String>, file: &FileSchema) -> Self {
        let mut merged = defaults.clone();
        let from_file = |name: &str| !cli_set.contains(name);
        let string = |name: &str, value: &str, target: &mut String| {
            if from_file(name) && !value.is_empty() {
                value.clone_into(target);
            }
        };
        string("log-format", &file.log_format, &mut merged.log_format);
        string("log-level", &file.log_level, &mut merged.log_level);
        string(
            "docker-socket",
            &file.docker_socket,
            &mut merged.docker_socket,
        );
        string("metrics-addr", &file.metrics_addr, &mut merged.metrics_addr);
        string("debug-addr", &file.debug_addr, &mut merged.debug_addr);
        string("policy-mode", &file.policy_mode, &mut merged.policy_mode);
        if let Some(value) = file.log_time.filter(|_| from_file("log-time")) {
            merged.log_time = value;
        }
        if let Some(value) = file.dry_run.filter(|_| from_file("dry-run")) {
            merged.dry_run = value;
        }
        if let Some(value) = file
            .device_allow
            .as_ref()
            .filter(|_| from_file("device-allow"))
        {
            merged.device_allow = Some(value.clone());
        }
        if let Some(value) = file
            .device_deny
            .as_ref()
            .filter(|_| from_file("device-deny"))
        {
            merged.device_deny = Some(value.clone());
        }
        merged
    }

    /// Go's `validate`, which runs before anything (the logger included) is configured from the
    /// settings: the enumerated values, the policy and, Q2, `debug-addr`, every failure joined;
    /// on success the hot-reloadable part the processor reads, Go's `runtime`.
    ///
    /// # Errors
    ///
    /// [`InvalidSettings`] with the text of every check that failed.
    pub fn validate(&self) -> Result<Runtime, InvalidSettings> {
        let enums = config::validate_enums(
            &self.log_format,
            &self.log_level,
            &self.policy_mode,
            "setting",
        );
        let policy = Global::new(
            &self.policy_mode,
            self.device_allow.clone(),
            self.device_deny.clone(),
        );
        let debug = server::refuse_debug_addr(&self.debug_addr);
        let joined = errors::join([
            enums.err().map(|error| error.to_string()),
            policy.as_ref().err().map(ToString::to_string),
            debug.err().map(|error| error.to_string()),
        ]);
        match (joined, policy) {
            (None, Ok(policy)) => Ok(Runtime {
                dry_run: self.dry_run,
                policy,
            }),
            (Some(text), _) => Err(InvalidSettings(text)),
            (None, Err(error)) => Err(InvalidSettings(error.to_string())),
        }
    }

    /// Go's `coldChanges`: the restart-only settings that differ between `self` and `next`.
    #[must_use]
    pub fn cold_changes(&self, next: &Self) -> Vec<&'static str> {
        [
            ("docker-socket", &self.docker_socket, &next.docker_socket),
            ("metrics-addr", &self.metrics_addr, &next.metrics_addr),
            ("debug-addr", &self.debug_addr, &next.debug_addr),
        ]
        .into_iter()
        .filter(|(_, current, next)| current != next)
        .map(|(key, _, _)| key)
        .collect()
    }
}

/// A device list as Go's slog writes a `[]string`: `nil` stays nil.
#[must_use]
pub fn list_attr(key: &str, values: Option<&[String]>) -> Attr {
    match values {
        Some(values) => Attr::strings(key, values),
        None => Attr::nil_strings(key),
    }
}

/// Reconfigures the logger: Go's `logger.Configure`.
pub type ConfigureLogger = Box<dyn Fn(&str, &str, bool) + Send + Sync>;

/// Publishes a runtime and reconciles every container, returning its generation: Go's
/// `Processor.PublishAndReconcile`.
pub type Publish = Box<dyn Fn(Runtime) -> Pin<Box<dyn Future<Output = u64> + Send>> + Send + Sync>;

/// Why a reload was rejected, with Go's text.
#[derive(Debug)]
pub enum ReloadError {
    /// The file could not be read or was refused.
    Load(LoadError),
    /// The merged settings failed validation.
    Invalid(InvalidSettings),
    /// The reload would turn dry-run on.
    DryRunEnable,
    /// The runtime shut down before the config-loading task started; nothing was applied.
    // Unreachable in production: runtime shutdown drops watch_sighup before cancelling queued loads.
    Canceled,
}

impl fmt::Display for ReloadError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Load(error) => write!(formatter, "config reload failed: {error}"),
            Self::Invalid(error) => write!(formatter, "config reload: invalid setting: {error}"),
            Self::DryRunEnable => write!(formatter, "config reload: {DRY_RUN_ENABLE}"),
            Self::Canceled => formatter.write_str("config reload failed: context canceled"),
        }
    }
}

impl std::error::Error for ReloadError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Load(error) => Some(error),
            Self::Invalid(error) => Some(error),
            Self::DryRunEnable | Self::Canceled => None,
        }
    }
}

/// Applies a re-read config file on `SIGHUP`, Go's `reloader`.
pub struct Reloader {
    /// The config file.
    pub path: String,
    /// The flags as parsed, defaults included.
    pub flags: Settings,
    /// The flags given on the command line.
    pub cli_set: BTreeSet<String>,
    /// What is in force: the startup settings, then those of the last reload that succeeded.
    pub current: Settings,
    pub configure_logger: ConfigureLogger,
    pub publish_and_reconcile: Publish,
    pub log: LogHandle,
}

impl fmt::Debug for Reloader {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Reloader")
            .field("path", &self.path)
            .field("current", &self.current)
            .finish_non_exhaustive()
    }
}

impl Reloader {
    /// Go's `reload`: re-reads the file and applies the hot-reloadable settings (logging,
    /// dry-run, policy). On any error nothing is applied: the store, the logger and
    /// [`Reloader::current`] keep their values. A changed restart-only setting is logged and
    /// ignored. Dry-run may be turned off but not on: turning it on would leave every grant in
    /// place while no longer enforcing a narrower policy.
    ///
    /// # Errors
    ///
    /// [`ReloadError`] when the file cannot be loaded, the merged settings are invalid, or the
    /// reload would turn dry-run on, or the runtime shuts down before loading starts.
    pub async fn reload(&mut self) -> Result<(), ReloadError> {
        let path = self.path.clone();
        let file = finish(tokio::task::spawn_blocking(move || {
            config::load_file(&path)
        }))
        .await
        .ok_or(ReloadError::Canceled)?
        .map_err(ReloadError::Load)?;
        let mut next = Settings::merge(&self.flags, &self.cli_set, &file);
        let runtime = next.validate().map_err(ReloadError::Invalid)?;
        if next.dry_run && !self.current.dry_run {
            return Err(ReloadError::DryRunEnable);
        }
        for key in self.current.cold_changes(&next) {
            self.log.info(
                "config reload: setting requires a restart; ignored",
                &[Attr::string("key", key)],
            );
        }
        next.docker_socket.clone_from(&self.current.docker_socket);
        next.metrics_addr.clone_from(&self.current.metrics_addr);
        next.debug_addr.clone_from(&self.current.debug_addr);

        (self.configure_logger)(&next.log_format, &next.log_level, next.log_time);
        let generation = (self.publish_and_reconcile)(runtime).await;
        self.log.info(
            "config reloaded",
            &[
                Attr::uint("generation", generation),
                Attr::bool("dry_run", next.dry_run),
                Attr::string("policy_mode", next.policy_mode.clone()),
                list_attr("device_allow", next.device_allow.as_deref()),
                list_attr("device_deny", next.device_deny.as_deref()),
            ],
        );
        self.current = next;
        Ok(())
    }
}

/// Go's `watchSIGHUP`: until `cancel`, reloads the config on each `SIGHUP` that `hangups`
/// delivers. The channel holds one pending signal, as Go's buffered channel does, so signals that
/// arrive during a reload coalesce into one more.
pub async fn watch_sighup(
    cancel: CancellationToken,
    mut hangups: mpsc::Receiver<()>,
    mut reloader: Reloader,
) {
    loop {
        tokio::select! {
            () = cancel.cancelled() => return,
            received = hangups.recv() => {
                if received.is_none() {
                    return;
                }
                reloader.log.info(
                    "SIGHUP received; reloading config",
                    &[Attr::string("config_file", reloader.path.clone())],
                );
                if let Err(error) = reloader.reload().await {
                    reloader.log.error(
                        "config reload rejected; keeping previous config",
                        &[Attr::error("err", &error)],
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests;
