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

// Adapted from swarm-scheduler-exporter/src/docker/version.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! API versions as moby client v0.6.0 parses and compares them.

use std::cmp::Ordering;

/// moby's `MaxAPIVersion`: the version a client starts from and negotiates down.
pub const MAX_API_VERSION: &str = "1.56";

/// moby's `MinAPIVersion`: a daemon below it is not negotiated with.
pub const MIN_API_VERSION: &str = "1.40";

/// moby's `parseAPIVersion`: `<major>.<minor>`, each a Go `strconv.Atoi`, normalised to
/// `%d.%d`. The error is moby's message.
///
/// # Errors
///
/// moby's message for an empty value, a `v` prefix, a missing dot, or a part that is not an
/// integer.
pub fn parse_api_version(version: &str) -> Result<String, String> {
    let trimmed = version.trim();
    if trimmed.strip_prefix('v').unwrap_or(trimmed).is_empty() {
        return Err("value is empty".to_owned());
    }
    if version.starts_with('v') {
        return Err("must be formatted <major>.<minor>".to_owned());
    }
    if version.trim().is_empty() {
        return Err("value is empty".to_owned());
    }
    let Some((major, minor)) = version.split_once('.') else {
        return Err("must be formatted <major>.<minor>".to_owned());
    };
    let major = atoi(major)
        .map_err(|()| "invalid major version: must be formatted <major>.<minor>".to_owned())?;
    let minor = atoi(minor)
        .map_err(|()| "invalid minor version: must be formatted <major>.<minor>".to_owned())?;
    Ok(format!("{major}.{minor}"))
}

/// Go's `strconv.Atoi`: an optional sign and decimal digits, in an `int`.
fn atoi(text: &str) -> Result<i64, ()> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(());
    }
    text.parse().map_err(|_| ())
}

/// `strconv.Atoi` as moby's version comparison reads it, ignoring the error: a value out of range
/// is clamped (as `ParseInt` returns it), anything else that is not a number is 0.
fn atoi_or_zero(text: &str) -> i64 {
    if let Ok(value) = atoi(text) {
        return value;
    }
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        0
    } else if text.starts_with('-') {
        i64::MIN
    } else {
        i64::MAX
    }
}

/// moby's `versions.compare`: dot-separated parts compared as integers, a missing part as 0.
#[must_use]
pub fn compare(left: &str, right: &str) -> Ordering {
    if left == right {
        return Ordering::Equal;
    }
    let left: Vec<&str> = left.split('.').collect();
    let right: Vec<&str> = right.split('.').collect();
    for index in 0..left.len().max(right.len()) {
        let left_part = left.get(index).map_or(0, |part| atoi_or_zero(part));
        let right_part = right.get(index).map_or(0, |part| atoi_or_zero(part));
        let ordering = left_part.cmp(&right_part);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

/// What the daemon's `Api-Version` negotiates, as moby's `Ping` and `negotiateAPIVersion` decide:
/// none or empty is [`MAX_API_VERSION`], a malformed one or one below [`MIN_API_VERSION`] is an
/// error (which moby's caller discards, leaving the client un-negotiated), and anything else is
/// the lower of it and [`MAX_API_VERSION`].
///
/// # Errors
///
/// moby's message, for a malformed or too old version.
pub fn negotiate(ping_version: &str) -> Result<String, String> {
    if ping_version.is_empty() {
        return Ok(MAX_API_VERSION.to_owned());
    }
    let ping_version = parse_api_version(ping_version)?;
    if compare(&ping_version, MIN_API_VERSION) == Ordering::Less {
        return Err(format!(
            "API version {ping_version} is not supported by this client: the minimum supported \
             API version is {MIN_API_VERSION}"
        ));
    }
    if compare(&ping_version, MAX_API_VERSION) == Ordering::Less {
        Ok(ping_version)
    } else {
        Ok(MAX_API_VERSION.to_owned())
    }
}
