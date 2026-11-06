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

//! The Engine API objects the daemon reads, with only the fields the Go daemon uses, converted
//! from bollard's models.
//!
//! A field bollard holds as `None` is the zero value, as Go's `encoding/json` leaves a missing
//! field or a JSON `null`, and every state and other enum-like field is a plain `String`, so a
//! value a newer Engine introduces reaches the daemon's own rules. `StartedAt` stays the string
//! the Engine sent: the Go daemon compares it as one to tell two runs of a container apart.

use std::collections::BTreeMap;

use docker_client::bollard::models;

/// A label or attribute map.
pub type Labels = BTreeMap<String, String>;

/// container `Summary`, one entry of the container list.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerSummary {
    /// `Id`.
    pub id: String,
}

/// container `InspectResponse`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerInspect {
    /// `Id`.
    pub id: String,
    /// `State`.
    pub state: Option<ContainerState>,
    /// `Config`.
    pub config: Option<ContainerConfig>,
    /// `HostConfig`.
    pub host_config: Option<HostConfig>,
    /// `Mounts`.
    pub mounts: Vec<MountPoint>,
}

/// container `State`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerState {
    /// `Running`.
    pub running: bool,
    /// `Pid`.
    pub pid: i64,
    /// `StartedAt`, as sent.
    pub started_at: String,
}

/// container `Config`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ContainerConfig {
    /// `Labels`.
    pub labels: Labels,
}

/// container `HostConfig`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HostConfig {
    /// `Privileged`.
    pub privileged: bool,
}

/// container `MountPoint`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MountPoint {
    /// `Type`: `bind`, `volume`, `tmpfs`, ...
    pub kind: String,
    /// `Source`.
    pub source: String,
    /// `Destination`.
    pub destination: String,
}

/// events `Message`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Event {
    /// `Type`: `container`, ... (the stream is filtered by action only, as in Go).
    pub kind: String,
    /// `Action`.
    pub action: String,
    /// `Actor`.
    pub actor: Actor,
    /// `time`: seconds since the epoch.
    pub time: i64,
    /// `timeNano`: nanoseconds since the epoch.
    pub time_nano: i64,
}

/// events `Actor`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Actor {
    /// `ID`.
    pub id: String,
    /// `Attributes`.
    pub attributes: Labels,
}

fn labels(map: Option<std::collections::HashMap<String, String>>) -> Labels {
    map.unwrap_or_default().into_iter().collect()
}

impl From<models::ContainerSummary> for ContainerSummary {
    fn from(summary: models::ContainerSummary) -> Self {
        Self {
            id: summary.id.unwrap_or_default(),
        }
    }
}

impl From<models::ContainerInspectResponse> for ContainerInspect {
    fn from(inspect: models::ContainerInspectResponse) -> Self {
        Self {
            id: inspect.id.unwrap_or_default(),
            state: inspect.state.map(|state| ContainerState {
                running: state.running.unwrap_or_default(),
                pid: state.pid.unwrap_or_default(),
                started_at: state.started_at.unwrap_or_default(),
            }),
            config: inspect.config.map(|config| ContainerConfig {
                labels: labels(config.labels),
            }),
            host_config: inspect.host_config.map(|host| HostConfig {
                privileged: host.privileged.unwrap_or_default(),
            }),
            mounts: inspect
                .mounts
                .unwrap_or_default()
                .into_iter()
                .map(|mount| MountPoint {
                    kind: mount.typ.unwrap_or_default(),
                    source: mount.source.unwrap_or_default(),
                    destination: mount.destination.unwrap_or_default(),
                })
                .collect(),
        }
    }
}

impl From<models::EventMessage> for Event {
    fn from(event: models::EventMessage) -> Self {
        let actor = event.actor.unwrap_or_default();
        Self {
            kind: event.typ.map(|kind| kind.to_string()).unwrap_or_default(),
            action: event.action.unwrap_or_default(),
            actor: Actor {
                id: actor.id.unwrap_or_default(),
                attributes: labels(actor.attributes),
            },
            time: event.time.unwrap_or_default(),
            time_nano: event.time_nano.unwrap_or_default(),
        }
    }
}

#[cfg(test)]
mod tests;
