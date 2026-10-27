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

// Adapted from swarm-scheduler-exporter/src/logger/mod.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The daemon's logger: slog's text and JSON formats and this project's plain format, byte for
//! byte (`internal/logger`).
//!
//! A [`Logger`] carries a minimum [`Level`], a [`Format`], whether the time is logged, a sink and
//! the clock and zone the record time is read from. `with_attrs` and `with_group` return a copy;
//! the original is unchanged. Each record is formatted whole, then written under one lock, so
//! records never interleave.
//!
//! Components log through a cloned [`LogHandle`], the port of Go's global `logger.L()`: every
//! clone shares one cell, and [`LogHandle::configure`] swaps the logger in it, so a reconfigure
//! (startup, a `SIGHUP` that changes the format or the level) reaches every component without a
//! process global.
//!
//! The levels are slog's: debug -4, info 0, warn 4, error 8, and any value between (`INFO+1`).

mod plain_handler;
mod slog_handler;
mod value;

use std::fmt;
use std::io::{self, Write};
use std::sync::{Arc, Mutex, PoisonError, RwLock};

use jiff::tz::TimeZone;

use crate::clock::Clock;
use crate::go_compat::{local_zone, time_layout};

use plain_handler::PlainHandler;
use slog_handler::SlogHandler;
pub use value::{Attr, Time, Value};

/// A slog level.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Level(pub i32);

impl Level {
    /// `slog.LevelDebug`.
    pub const DEBUG: Self = Self(-4);
    /// `slog.LevelInfo`.
    pub const INFO: Self = Self(0);
    /// `slog.LevelWarn`.
    pub const WARN: Self = Self(4);
    /// `slog.LevelError`.
    pub const ERROR: Self = Self(8);
}

impl fmt::Display for Level {
    /// slog's `Level.String`: `INFO`, `INFO+1`, `DEBUG-1`, `ERROR+4`.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (base, delta) = if *self < Self::INFO {
            ("DEBUG", self.0 - Self::DEBUG.0)
        } else if *self < Self::WARN {
            ("INFO", self.0 - Self::INFO.0)
        } else if *self < Self::ERROR {
            ("WARN", self.0 - Self::WARN.0)
        } else {
            ("ERROR", self.0 - Self::ERROR.0)
        };
        if delta == 0 {
            formatter.write_str(base)
        } else {
            write!(formatter, "{base}{delta:+}")
        }
    }
}

/// The log format.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// slog's `TextHandler`.
    Text,
    /// slog's `JSONHandler`.
    Json,
    /// This project's plain handler.
    Plain,
}

/// Go's `parseLevel`: a level name in any case, `debug`, `info`, `warn` or `warning`, and
/// `error`, `fatal` or `panic` (all three read as error). Anything else reads as info; the
/// settings are validated against the accepted names before they reach the logger.
#[must_use]
pub fn parse_level(value: &str) -> Level {
    match go_to_lower(value).as_str() {
        "debug" => Level::DEBUG,
        "warn" | "warning" => Level::WARN,
        "error" | "fatal" | "panic" => Level::ERROR,
        // "info" and any other value: Go's default.
        _ => Level::INFO,
    }
}

/// The format switch of Go's `newHandler`: `json` or `plain` in any case; anything else is text.
#[must_use]
pub fn parse_format(value: &str) -> Format {
    match go_to_lower(value).as_str() {
        "json" => Format::Json,
        "plain" => Format::Plain,
        // "text" and any other value: Go's default.
        _ => Format::Text,
    }
}

/// Go's `strings.ToLower`, which maps each character to one character: it differs from Rust's
/// `to_lowercase` on U+0130 (`İ`), which Go lowers to a plain `i` and Rust to `i` plus a
/// combining dot, so `İNFO` is a level in Go.
fn go_to_lower(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            let lower: Vec<char> = if character == '\u{130}' {
                vec!['i']
            } else {
                character.to_lowercase().collect()
            };
            lower
        })
        .collect()
}

/// Where the record time comes from.
pub type Now = Arc<dyn Clock>;

/// A sink every record is written to, whole, under one lock.
pub type Sink = Box<dyn Write + Send>;

/// The logger options.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Config {
    /// The format.
    pub format: Format,
    /// The minimum level written.
    pub level: Level,
    /// Whether records carry their time (`--log-time`).
    pub include_time: bool,
}

impl Config {
    /// The options Go's `Configure(format, level, includeTime)` reads from the settings.
    #[must_use]
    pub fn parse(format: &str, level: &str, include_time: bool) -> Self {
        Self {
            format: parse_format(format),
            level: parse_level(level),
            include_time,
        }
    }
}

/// What every copy of a logger shares.
struct Shared {
    level: Level,
    include_time: bool,
    sink: Mutex<Sink>,
    zone: TimeZone,
    now: Now,
}

/// The handler a logger formats with.
#[derive(Debug, Clone, PartialEq)]
enum Handler {
    Slog(SlogHandler),
    Plain(PlainHandler),
}

/// A logger. Clones are cheap and share the sink.
#[derive(Clone)]
pub struct Logger {
    shared: Arc<Shared>,
    handler: Arc<Handler>,
}

impl fmt::Debug for Logger {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("Logger")
            .field("level", &self.shared.level)
            .field("include_time", &self.shared.include_time)
            .field("handler", &self.handler)
            .finish_non_exhaustive()
    }
}

impl Logger {
    /// A logger writing to `sink`, with record times from `now` written in `zone`.
    #[must_use]
    pub fn new(config: Config, sink: Sink, zone: TimeZone, now: Now) -> Self {
        let handler = match config.format {
            Format::Text => Handler::Slog(SlogHandler::new(false, config.include_time)),
            Format::Json => Handler::Slog(SlogHandler::new(true, config.include_time)),
            Format::Plain => Handler::Plain(PlainHandler::default()),
        };
        Self {
            shared: Arc::new(Shared {
                level: config.level,
                include_time: config.include_time,
                sink: Mutex::new(sink),
                zone,
                now,
            }),
            handler: Arc::new(handler),
        }
    }

    /// The zone record times are written in, Go's `time.Local`.
    #[must_use]
    pub fn zone(&self) -> &TimeZone {
        &self.shared.zone
    }

    /// The clock record times are read from.
    #[must_use]
    pub fn clock(&self) -> Now {
        Arc::clone(&self.shared.now)
    }

    /// Whether a record at `level` is written.
    #[must_use]
    pub fn enabled(&self, level: Level) -> bool {
        level >= self.shared.level
    }

    /// `Logger.With`: a copy that adds `attrs` to every record. No attributes return the same
    /// logger.
    #[must_use]
    pub fn with_attrs(&self, attrs: &[Attr]) -> Self {
        if attrs.is_empty() {
            return self.clone();
        }
        let handler = match &*self.handler {
            Handler::Slog(handler) => Handler::Slog(handler.with_attrs(attrs)),
            Handler::Plain(handler) => Handler::Plain(handler.with_attrs(attrs)),
        };
        self.with_handler(handler)
    }

    /// `Logger.WithGroup`: a copy that qualifies the attributes that follow with `name`. An empty
    /// name returns the same logger (and the plain format also ignores a blank one).
    #[must_use]
    pub fn with_group(&self, name: &str) -> Self {
        if name.is_empty() {
            return self.clone();
        }
        let handler = match &*self.handler {
            Handler::Slog(handler) => Handler::Slog(handler.with_group(name)),
            Handler::Plain(handler) => Handler::Plain(handler.with_group(name)),
        };
        self.with_handler(handler)
    }

    fn with_handler(&self, handler: Handler) -> Self {
        Self {
            shared: Arc::clone(&self.shared),
            handler: Arc::new(handler),
        }
    }

    /// Logs at debug.
    pub fn debug(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::DEBUG, message, attrs);
    }

    /// Logs at info.
    pub fn info(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::INFO, message, attrs);
    }

    /// Logs at warn.
    pub fn warn(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::WARN, message, attrs);
    }

    /// Logs at error.
    pub fn error(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::ERROR, message, attrs);
    }

    /// Logs one record at `level`, if enabled. Empty groups among `attrs` are dropped, as
    /// `Record.Add` does. A failed write has nowhere to be reported and is ignored, as slog's
    /// `Logger` ignores its handler's error.
    pub fn log(&self, level: Level, message: &str, attrs: &[Attr]) {
        if !self.enabled(level) {
            return;
        }
        let timestamp = self.shared.now.now();
        let time = (!time_layout::is_go_zero_time(timestamp)).then(|| Time {
            timestamp,
            offset: self.shared.zone.to_offset(timestamp),
        });
        let attrs: Vec<Attr> = attrs
            .iter()
            .filter(|attr| !attr.is_empty_group())
            .cloned()
            .collect();
        let line = match &*self.handler {
            Handler::Slog(handler) => handler.format(time, level, message, &attrs),
            // The plain handler checks the time option itself; slog's handlers drop the time
            // through the time stripper.
            Handler::Plain(handler) => {
                let time = time.filter(|_| self.shared.include_time);
                handler.format(time, level, message, &attrs)
            }
        };
        // Under panic = "abort" a poisoned lock is only reachable in tests, and the sink holds
        // no state a panic could leave half-updated.
        let mut sink = self
            .shared
            .sink
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let _ = sink.write_all(line.as_bytes());
        let _ = sink.flush();
    }
}

/// The logger every component logs through, Go's `logger.L()`. Clones share one cell:
/// [`LogHandle::configure`] and [`LogHandle::set`] swap the logger in it, and every clone logs
/// through the new one from its next record. A handle from [`LogHandle::with_attrs`] adds its
/// attributes to the current logger at each record, as Go's `logger.L().With(...)` at the call
/// site does.
#[derive(Clone)]
pub struct LogHandle {
    current: Arc<RwLock<Logger>>,
    attrs: Arc<[Attr]>,
}

impl fmt::Debug for LogHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LogHandle")
            .field("current", &self.logger())
            .field("attrs", &self.attrs)
            .finish()
    }
}

impl LogHandle {
    /// A handle on `logger`.
    #[must_use]
    pub fn new(logger: Logger) -> Self {
        Self {
            current: Arc::new(RwLock::new(logger)),
            attrs: Arc::from([]),
        }
    }

    /// Go's default before `Configure`: text at info, with the time from `now` in the local zone
    /// (resolved from `getenv("TZ")` as Go does), to stdout.
    #[must_use]
    pub fn stdout(now: Now, getenv: impl Fn(&str) -> Option<String>) -> Self {
        Self::new(Logger::new(
            Config {
                format: Format::Text,
                level: Level::INFO,
                include_time: true,
            },
            Box::new(io::stdout()),
            local_zone::local_zone(getenv),
            now,
        ))
    }

    /// The current logger, with this handle's attributes.
    #[must_use]
    pub fn logger(&self) -> Logger {
        let current = self
            .current
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        current.with_attrs(&self.attrs)
    }

    /// Go's `Set`: replaces the logger every clone logs through.
    pub fn set(&self, logger: Logger) {
        // A poisoned lock is only reachable in tests (panic = "abort"); the cell holds a whole
        // logger either way.
        *self.current.write().unwrap_or_else(PoisonError::into_inner) = logger;
    }

    /// Go's `Configure`: installs the logger `format`, `level` and `include_time` ask for,
    /// writing to stdout, with the clock and zone of the current one.
    pub fn configure(&self, format: &str, level: &str, include_time: bool) {
        self.configure_to(
            Config::parse(format, level, include_time),
            Box::new(io::stdout()),
        );
    }

    /// [`LogHandle::configure`] with a sink of the caller's: Go's `newHandler(output, ...)`.
    pub fn configure_to(&self, config: Config, sink: Sink) {
        let (zone, now) = {
            let current = self.current.read().unwrap_or_else(PoisonError::into_inner);
            (current.zone().clone(), current.clock())
        };
        self.set(Logger::new(config, sink, zone, now));
    }

    /// A handle on the same cell that adds `attrs` to every record. No attributes return a plain
    /// clone.
    #[must_use]
    pub fn with_attrs(&self, attrs: &[Attr]) -> Self {
        Self {
            current: Arc::clone(&self.current),
            attrs: self.attrs.iter().chain(attrs).cloned().collect(),
        }
    }

    /// Whether a record at `level` is written by the current logger.
    #[must_use]
    pub fn enabled(&self, level: Level) -> bool {
        self.logger().enabled(level)
    }

    /// Logs at debug.
    pub fn debug(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::DEBUG, message, attrs);
    }

    /// Logs at info.
    pub fn info(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::INFO, message, attrs);
    }

    /// Logs at warn.
    pub fn warn(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::WARN, message, attrs);
    }

    /// Logs at error.
    pub fn error(&self, message: &str, attrs: &[Attr]) {
        self.log(Level::ERROR, message, attrs);
    }

    /// Logs one record at `level` through the current logger.
    pub fn log(&self, level: Level, message: &str, attrs: &[Attr]) {
        self.logger().log(level, message, attrs);
    }
}

/// A sink that keeps what is written, for tests: clones share the buffer.
#[derive(Debug, Clone, Default)]
pub struct MemorySink {
    buffer: Arc<Mutex<Vec<u8>>>,
}

impl MemorySink {
    /// Everything written so far, as text.
    #[must_use]
    pub fn contents(&self) -> String {
        let buffer = self.buffer.lock().unwrap_or_else(PoisonError::into_inner);
        String::from_utf8_lossy(&buffer).into_owned()
    }
}

impl Write for MemorySink {
    fn write(&mut self, data: &[u8]) -> io::Result<usize> {
        self.buffer
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .extend_from_slice(data);
        Ok(data.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests;
