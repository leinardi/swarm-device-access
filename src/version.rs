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
//! unset.

/// The version, `dev` unless stamped.
pub const VERSION: &str = stamp(option_env!("SWARM_DEVICE_ACCESS_VERSION"), "dev");

/// The commit, `none` unless stamped.
pub const COMMIT: &str = stamp(option_env!("SWARM_DEVICE_ACCESS_COMMIT"), "none");

/// The build date, `unknown` unless stamped.
pub const DATE: &str = stamp(option_env!("SWARM_DEVICE_ACCESS_DATE"), "unknown");

/// The stamp, or `default` when the variable was unset at build time. A variable set to the empty
/// string stamps the empty string, as `-X main.version=` did.
const fn stamp(value: Option<&'static str>, default: &'static str) -> &'static str {
    match value {
        Some(value) => value,
        None => default,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_stamps_fall_back_to_go_defaults() {
        assert_eq!(stamp(None, "dev"), "dev");
        assert_eq!(stamp(Some(""), "dev"), "");
        assert_eq!(stamp(Some("v1.2.3"), "dev"), "v1.2.3");
    }
}
