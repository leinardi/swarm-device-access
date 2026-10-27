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
// Adapted from swarm-scheduler-exporter/testenv/src/lib.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The integration harness for swarm-device-access. It drives the host Docker daemon, as the Go
//! suite under `test/integration` does, because device enforcement needs the host's own cgroups;
//! it is not a Docker-in-Docker cluster. Everything it creates carries the envid label, so a run
//! can be cleaned up (or swept after a crash) without touching anything else on the host.
//!
//! The harness mutates Docker on purpose: it creates and removes containers and images. That is
//! why the modules that reach a daemon sit behind the `integration` feature, as Go's sat behind
//! its integration build tag, and why the daemon package depends on this crate in no table. The
//! run filter, the sweep rules, the redacting logger, the image identity rule and the enforcement
//! tests' guards and program checks need no daemon and always compile, so their tests run in a
//! plain `cargo test`.

#[cfg(feature = "integration")]
pub mod context;
pub mod enforce;
#[cfg(feature = "integration")]
pub mod envid;
mod error;
#[cfg(feature = "integration")]
pub mod host;
pub mod identity;
pub mod redact;
pub mod run_filter;
pub mod sweep_rules;

pub use error::Error;
