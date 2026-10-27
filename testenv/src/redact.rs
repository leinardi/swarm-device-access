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
// Adapted from swarm-scheduler-exporter/testenv/src/redact.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The harness's log (the sweep's decisions), with secrets kept out of it. Built on the daemon's
//! slog-compatible logger: Go's text handler at debug, with the time, wrapped in a handler that
//! rewrites attributes before they are written.
//!
//! What is redacted, as in swarm-scheduler-exporter's Go harness: an attribute keyed `secret`, a
//! [`Secret`] value whatever its key, and the values (not the names) of an `env` list of
//! `KEY=value` entries. Attributes nested in a group are not inspected, as Go's handler did not
//! inspect them. This harness logs no secret today; the rule stays so that a later one (a registry
//! credential, say) cannot reach the log by accident.

use std::fmt;
use std::sync::Arc;

use swarm_device_access::clock::SystemClock;
use swarm_device_access::go_compat::local_zone::local_zone;
use swarm_device_access::logger::{Attr, Config, Format, Level, Logger, Sink, Value};

/// What a redacted value is written as.
pub const REDACTED: &str = "[redacted]";

/// A value that must never reach a log or an error message. `Display` and `Debug` both write
/// [`REDACTED`], and as a log value it becomes [`REDACTED`] before any handler sees it.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    /// Wraps `value`.
    #[must_use]
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    /// The value itself, for the one place that needs it.
    #[must_use]
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(REDACTED)
    }
}

impl From<Secret> for Value {
    fn from(_: Secret) -> Self {
        Self::Str(REDACTED.to_owned())
    }
}

/// `KEY=value` entries with every value replaced by [`REDACTED`]; an entry without `=` is
/// replaced whole.
#[must_use]
pub fn redact_env(env: &[String]) -> Vec<String> {
    env.iter()
        .map(|entry| match entry.split_once('=') {
            Some((key, _)) => format!("{key}={REDACTED}"),
            None => REDACTED.to_owned(),
        })
        .collect()
}

/// A logger that redacts as the module describes. Clones are cheap and share the sink.
#[derive(Debug, Clone)]
pub struct RedactingLogger {
    inner: Logger,
}

impl RedactingLogger {
    /// Go's text handler at debug, with record times in the local zone, writing to `sink`.
    #[must_use]
    pub fn new(sink: Sink) -> Self {
        Self {
            inner: Logger::new(
                Config {
                    format: Format::Text,
                    level: Level::DEBUG,
                    include_time: true,
                },
                sink,
                local_zone(|key| std::env::var(key).ok()),
                Arc::new(SystemClock),
            ),
        }
    }

    /// `Logger.With`, redacting `attrs` first.
    #[must_use]
    pub fn with_attrs(&self, attrs: &[Attr]) -> Self {
        Self {
            inner: self.inner.with_attrs(&redact_attrs(attrs)),
        }
    }

    /// `Logger.WithGroup`.
    #[must_use]
    pub fn with_group(&self, name: &str) -> Self {
        Self {
            inner: self.inner.with_group(name),
        }
    }

    /// Logs at debug.
    pub fn debug(&self, message: &str, attrs: &[Attr]) {
        self.inner.debug(message, &redact_attrs(attrs));
    }

    /// Logs at info.
    pub fn info(&self, message: &str, attrs: &[Attr]) {
        self.inner.info(message, &redact_attrs(attrs));
    }

    /// Logs at warn.
    pub fn warn(&self, message: &str, attrs: &[Attr]) {
        self.inner.warn(message, &redact_attrs(attrs));
    }

    /// Logs at error.
    pub fn error(&self, message: &str, attrs: &[Attr]) {
        self.inner.error(message, &redact_attrs(attrs));
    }
}

fn redact_attrs(attrs: &[Attr]) -> Vec<Attr> {
    attrs.iter().map(redact_attr).collect()
}

fn redact_attr(attr: &Attr) -> Attr {
    if attr.key == "secret" {
        return Attr::string(attr.key.clone(), REDACTED);
    }
    match &attr.value {
        Value::Strings(entries) if attr.key == "env" => {
            Attr::new(attr.key.clone(), Value::Strings(redact_env(entries)))
        }
        Value::Str(_)
        | Value::Int(_)
        | Value::Uint(_)
        | Value::Float(_)
        | Value::Bool(_)
        | Value::Duration(_)
        | Value::Time(_)
        | Value::Strings(_)
        | Value::NilStrings
        | Value::Stringer(_)
        | Value::Error(_)
        | Value::Nil
        | Value::Group(_) => attr.clone(),
    }
}

#[cfg(test)]
mod tests {
    use swarm_device_access::logger::{Attr, MemorySink};

    use super::{REDACTED, RedactingLogger, Secret, redact_env};

    /// swarm-scheduler-exporter's Go harness: `TestRedactingLogger` (this project's Go suite
    /// has none).
    #[test]
    fn redacting_logger_keeps_secrets_out() {
        let sink = MemorySink::default();
        let log = RedactingLogger::new(Box::new(sink.clone()))
            .with_attrs(&[Attr::string("secret", "token-a")]);
        log.info(
            "bring-up",
            &[
                Attr::new("token", Secret::new("token-b")),
                Attr::strings("env", ["DOCKER_AUTH=token-c", "token-d"]),
                Attr::strings("nodes", ["manager-0", "worker-0"]),
            ],
        );
        let logged = sink.contents();

        for leaked in ["token-a", "token-b", "token-c", "token-d"] {
            assert!(
                !logged.contains(leaked),
                "the log leaks {leaked}:\n{logged}"
            );
        }
        for kept in ["DOCKER_AUTH=", "manager-0", "worker-0", "msg=bring-up"] {
            assert!(
                logged.contains(kept),
                "the log lost {kept}, which is not secret:\n{logged}"
            );
        }

        let formatted = format!(
            "{} {:?} {}",
            Secret::new("x"),
            Secret::new("x"),
            Secret::new("x")
        );
        assert!(
            !formatted.contains('x'),
            "a Secret formats as {formatted:?}"
        );
        assert_eq!(formatted.matches(REDACTED).count(), 3, "{formatted:?}");
        assert_eq!(Secret::new("x").expose(), "x");

        assert_eq!(
            redact_env(&["A=1".to_owned(), "B=".to_owned(), "C".to_owned()]),
            [
                format!("A={REDACTED}"),
                format!("B={REDACTED}"),
                REDACTED.to_owned()
            ]
        );
    }
}
