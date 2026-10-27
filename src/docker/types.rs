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

// Adapted from swarm-scheduler-exporter/src/docker/types.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The Engine API objects the daemon reads, with only the fields the Go daemon uses.
//!
//! Decoding follows Go's `encoding/json` where the daemon's output can tell them apart: a missing
//! field or a JSON `null` is the zero value (`null_default`), unknown fields are ignored, and
//! every state and other enum-like field is a plain `String`, so a value a newer Engine
//! introduces reaches the daemon's own rules instead of failing the whole answer. A field Go holds
//! behind a pointer is an `Option`, so its presence stays visible. `StartedAt` stays the string the
//! Engine sent: the Go daemon compares it as one to tell two runs of a container apart.

use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer};

/// Labels, as the daemon sends them.
pub type Labels = BTreeMap<String, String>;

/// Go decodes a JSON `null` into the zero value; derived serde would refuse it.
pub(crate) fn null_default<'de, D, T>(deserializer: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: Default + Deserialize<'de>,
{
    Ok(Option::<T>::deserialize(deserializer)?.unwrap_or_default())
}

/// container `Summary`, one entry of the container list.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ContainerSummary {
    /// `Id`.
    #[serde(rename = "Id", deserialize_with = "null_default")]
    pub id: String,
}

/// container `InspectResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ContainerInspect {
    /// `Id`.
    #[serde(rename = "Id", deserialize_with = "null_default")]
    pub id: String,
    /// `State`.
    #[serde(rename = "State")]
    pub state: Option<ContainerState>,
    /// `Config`.
    #[serde(rename = "Config")]
    pub config: Option<ContainerConfig>,
    /// `HostConfig`.
    #[serde(rename = "HostConfig")]
    pub host_config: Option<HostConfig>,
    /// `Mounts`.
    #[serde(rename = "Mounts", deserialize_with = "null_default")]
    pub mounts: Vec<MountPoint>,
}

/// container `State`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ContainerState {
    /// `Running`.
    #[serde(rename = "Running", deserialize_with = "null_default")]
    pub running: bool,
    /// `Pid`.
    #[serde(rename = "Pid", deserialize_with = "null_default")]
    pub pid: i64,
    /// `StartedAt`, as sent.
    #[serde(rename = "StartedAt", deserialize_with = "null_default")]
    pub started_at: String,
}

/// container `Config`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct ContainerConfig {
    /// `Labels`.
    #[serde(rename = "Labels", deserialize_with = "null_default")]
    pub labels: Labels,
}

/// container `HostConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct HostConfig {
    /// `Privileged`.
    #[serde(rename = "Privileged", deserialize_with = "null_default")]
    pub privileged: bool,
}

/// container `MountPoint`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct MountPoint {
    /// `Type`: `bind`, `volume`, `tmpfs`, …
    #[serde(rename = "Type", deserialize_with = "null_default")]
    pub kind: String,
    /// `Source`.
    #[serde(rename = "Source", deserialize_with = "null_default")]
    pub source: String,
    /// `Destination`.
    #[serde(rename = "Destination", deserialize_with = "null_default")]
    pub destination: String,
}

/// events `Message`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Event {
    /// `Type`: `container`, … (the stream is filtered by action only, as in Go).
    #[serde(rename = "Type", deserialize_with = "null_default")]
    pub kind: String,
    /// `Action`.
    #[serde(rename = "Action", deserialize_with = "null_default")]
    pub action: String,
    /// `Actor`.
    #[serde(rename = "Actor", deserialize_with = "null_default")]
    pub actor: Actor,
    /// `time`: seconds since the epoch.
    #[serde(rename = "time", deserialize_with = "null_default")]
    pub time: i64,
    /// `timeNano`: nanoseconds since the epoch.
    #[serde(rename = "timeNano", deserialize_with = "null_default")]
    pub time_nano: i64,
}

/// events `Actor`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default)]
pub struct Actor {
    /// `ID`.
    #[serde(rename = "ID", deserialize_with = "null_default")]
    pub id: String,
    /// `Attributes`.
    #[serde(rename = "Attributes", deserialize_with = "null_default")]
    pub attributes: Labels,
}

/// Decodes the first JSON value of `body` the way Go's `json.NewDecoder(body).Decode(&out)`
/// does: leading whitespace skipped, whatever follows the value ignored, a `null` the zero value,
/// and an empty body `EOF`.
pub(crate) fn decode_first<T>(body: &[u8]) -> Result<T, String>
where
    T: Default + for<'de> Deserialize<'de>,
{
    match serde_json::Deserializer::from_slice(body)
        .into_iter::<Option<T>>()
        .next()
    {
        None => Err("EOF".to_owned()),
        Some(Ok(value)) => Ok(value.unwrap_or_default()),
        Some(Err(error)) if error.is_eof() => Err("unexpected EOF".to_owned()),
        Some(Err(error)) => Err(error.to_string()),
    }
}

/// Decodes a list the way Go decodes it into a slice: `null` is an empty list and a `null`
/// element the zero value.
pub(crate) fn decode_list<T>(body: &[u8]) -> Result<Vec<T>, String>
where
    T: Default + for<'de> Deserialize<'de>,
{
    decode_first::<Vec<Nullable<T>>>(body)
        .map(|items| items.into_iter().map(|item| item.0).collect())
}

/// A list element Go would decode from `null` into the zero value.
#[derive(Debug, Default)]
struct Nullable<T>(T);

impl<'de, T: Default + Deserialize<'de>> Deserialize<'de> for Nullable<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        null_default(deserializer).map(Self)
    }
}

#[cfg(test)]
mod tests;
