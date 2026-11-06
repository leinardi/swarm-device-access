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

//! The process's logging: `tracing` events written to stdout through rust-common's
//! `tracing-setup`, as tracing-subscriber's text lines or one flat JSON object per line, without
//! the target. The daemon's subscriber is reloadable, so a `SIGHUP` reload switches the format,
//! the level and the timestamp; the launcher's is fixed and carries no timestamp, as its
//! supervisor stamps the lines.

use std::io;

use tracing::level_filters::LevelFilter;
use tracing_setup::{Format, Options, ReloadHandle};

/// The format a validated `log-format` names: `json`, otherwise text.
#[must_use]
pub fn format(name: &str) -> Format {
    if name == "json" {
        Format::Json
    } else {
        Format::Text
    }
}

/// The level a validated `log-level` names; an unknown name reads as `info`.
#[must_use]
pub fn level(name: &str) -> LevelFilter {
    name.parse().unwrap_or(LevelFilter::INFO)
}

/// The subscriber options: no target, no span fields, and the timestamp when `time` is set.
const fn options(time: bool) -> Options {
    Options::new().target(false).span_fields(false).time(time)
}

/// Makes the daemon's reloadable subscriber the process's default, writing to stdout, and returns
/// the handle a reload reconfigures it with. A default already installed (a second run in one
/// process, as the tests make) stays; the handle then drives a subscriber nothing logs through.
#[must_use]
pub fn install_daemon(format: Format, level: LevelFilter, time: bool) -> ReloadHandle {
    let (dispatch, handle) =
        tracing_setup::build_reloadable(io::stdout, format, level, options(time));
    let _ = tracing_setup::install_global(dispatch);
    handle
}

/// Makes the launcher's own subscriber the process's default: stdout, no timestamp.
pub fn install_launcher(format: Format, level: LevelFilter) {
    let dispatch = tracing_setup::build(io::stdout, format, level, options(false));
    let _ = tracing_setup::install_global(dispatch);
}

/// `work` that runs under the dispatcher current where this is called, wherever it is run. A
/// blocking task runs on a pool thread, which has the process's global default but not a scoped
/// one, so the work of a blocking transaction logs where its caller does (as the unit tests,
/// which scope a capture to the test thread, need).
pub fn inherit<T>(work: impl FnOnce() -> T) -> impl FnOnce() -> T {
    let dispatch = tracing::dispatcher::get_default(Clone::clone);
    move || tracing::dispatcher::with_default(&dispatch, work)
}
