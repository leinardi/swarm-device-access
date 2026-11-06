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

//! The daemon's metrics on prometheus-client: the [`Recorder`] of its `sda_*` families, with
//! service-kit's `process_*` collector, exposed as OpenMetrics text. The `go_*` runtime families
//! have no Rust counterpart and are not served (Q3 in `docs/rust-port.md`), and neither are
//! promhttp's own `promhttp_metric_handler_*` families.

mod recorder;
#[cfg(test)]
pub(crate) mod testing;

pub use recorder::Recorder;

/// The content type of the exposition: OpenMetrics text 1.0.0.
pub const CONTENT_TYPE: &str = "application/openmetrics-text; version=1.0.0; charset=utf-8";

/// The upper bounds of `sda_apply_duration_seconds`, the Prometheus Go client's `DefBuckets`
/// (the `+Inf` bucket is implicit).
pub const DEF_BUCKETS: [f64; 11] = [
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// What a scrape of `/metrics` is answered with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Exposition {
    /// The `Content-Type`.
    pub content_type: &'static str,
    /// The body.
    pub body: Vec<u8>,
}
