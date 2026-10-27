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
// Adapted from swarm-scheduler-exporter/testenv/src/envid.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The run's envid, the label that namespaces everything it creates, and the resource names
//! derived from it.

use std::fmt::Write as _;
use std::fs::File;
use std::io::Read as _;

use crate::Error;

/// Six random bytes, twelve hex characters, as the Go suite's `TestMain` draws them.
const ENV_ID_BYTES: usize = 6;

/// The Docker label key applied to every resource the harness creates: the Go suite's
/// `labelEnvID`, so `make sweep-test-leaks` finds what either suite left. Use it with
/// [`label_filter`] to scope Docker API queries to one run, or on its own to find every run. It
/// sits outside the `swarm-device-access.` prefix, under which the daemon warns about unknown
/// keys.
pub const LABEL_ENV_ID: &str = "swarm-device-access-it.envid";

const RESOURCE_PREFIX: &str = "sda-it";

/// A fresh envid: twelve lowercase hex characters from the kernel's random source, as Go's
/// `crypto/rand` reads it.
///
/// # Errors
///
/// The random source cannot be read.
pub fn generate_env_id() -> Result<String, Error> {
    let mut bytes = [0_u8; ENV_ID_BYTES];
    File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut bytes))
        .map_err(|error| Error::new(format!("generate env ID: {error}")))?;
    Ok(bytes.iter().fold(String::new(), |mut hex, byte| {
        let _ = write!(hex, "{byte:02x}");
        hex
    }))
}

/// The name of the run's resource `base`: `sda-it-<envid>-<base>`.
#[must_use]
pub fn resource_name(env_id: &str, base: &str) -> String {
    format!("{RESOURCE_PREFIX}-{env_id}-{base}")
}

/// The `label` filter value that selects the run's resources.
#[must_use]
pub fn label_filter(env_id: &str) -> String {
    format!("{LABEL_ENV_ID}={env_id}")
}
