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

//! The metric registry and its exposition, rust-common's go-prom (the Prometheus Go client's
//! registry, families, text format, promhttp negotiation as prometheus/common v0.70.1 does it, and
//! the process collector), re-exported here, and the daemon's own [`Recorder`] of its `sda_*`
//! families. The `go_*` runtime families have no Rust counterpart and are not served (Q3 in
//! `docs/rust-port.md`).

pub use go_prom::*;

mod recorder;

pub use recorder::Recorder;
