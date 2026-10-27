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

// Adapted from swarm-scheduler-exporter/src/metrics/testing.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Test helpers over the registry, ports of the Go tests' `gatherSeries` and `seriesID`.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::go_compat::quote::quote;

use super::{Collector, Registry, Value};

/// A series as `name{a="x",b="y"}`, label names sorted and values quoted as Go's `%q`.
#[must_use]
pub(crate) fn series_id(name: &str, labels: &[(&str, &str)]) -> String {
    let mut labels = labels.to_vec();
    labels.sort_unstable();
    let pairs: Vec<String> = labels
        .iter()
        .map(|(label, value)| format!("{label}={}", quote(value)))
        .collect();
    format!("{name}{{{}}}", pairs.join(","))
}

/// Registers `collector` in a fresh registry, gathers it, and returns every scalar series by
/// [`series_id`]. A histogram series fails the test: the Go helper reads gauge values only.
pub(crate) fn gather_series(collector: Arc<dyn Collector>) -> BTreeMap<String, f64> {
    let registry = Registry::new();
    registry
        .register(collector)
        .unwrap_or_else(|error| panic!("register collector: {error}"));
    let mut series = BTreeMap::new();
    for family in registry.gather() {
        for gathered in family.series {
            let labels: Vec<(&str, &str)> = gathered
                .labels
                .iter()
                .map(|(name, value)| (name.as_str(), value.as_str()))
                .collect();
            let Value::Scalar(value) = gathered.value else {
                panic!("{}: not a scalar series", family.name);
            };
            series.insert(series_id(&family.name, &labels), value);
        }
    }
    series
}
