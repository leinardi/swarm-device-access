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

// Adapted from swarm-scheduler-exporter/src/version.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The build's version stamps, logged when the daemon and the launcher start. They are read at
//! compile time from `SWARM_DEVICE_ACCESS_VERSION`, `_COMMIT` and `_DATE` (`make rust-build` sets
//! them from git and the clock, as Go's `-ldflags -X` did), and fall back to Go's defaults when
//! unset; a variable set to the empty string stamps the empty string, as `-X main.version=` did.

use service_kit::{BuildInfo, Defaults, EmptyStamp, buildinfo};

/// The version (`dev` unless stamped), the commit (`none`) and the build date (`unknown`).
pub const BUILD: BuildInfo = buildinfo!(
    "SWARM_DEVICE_ACCESS",
    Defaults {
        version: "dev",
        commit: "none",
        date: "unknown",
        empty: EmptyStamp::Kept,
    },
);
