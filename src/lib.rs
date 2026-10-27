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

//! swarm-device-access, the Rust port: a daemon that injects cgroup device-allow rules into the
//! containers that bind-mount `/dev` paths, for Docker Swarm, where `devices:` and
//! `device_cgroup_rules:` are ignored.
//!
//! The port lands module by module beside the Go daemon, which keeps shipping until the cutover;
//! `docs/rust-port.md` records the design, the intended differences from 1.x and where each module
//! comes from.

pub mod cgroup;
pub mod clock;
pub mod config;
pub mod docker;
pub mod go_compat;
pub mod logger;
pub mod metrics;
pub mod policy;
pub mod systemd;
#[cfg(test)]
mod test_support;
