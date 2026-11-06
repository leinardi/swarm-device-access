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

//! What the tests read a recorder through: its exposition parsed back into families and samples.

use std::collections::BTreeMap;

use super::Recorder;

/// One sample line: its name (with `_total`, `_bucket`, `_sum` or `_count`), its labels in the
/// order written, and its value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Sample {
    pub(crate) name: String,
    pub(crate) labels: Vec<(String, String)>,
    pub(crate) value: f64,
}

/// A parsed OpenMetrics exposition.
#[derive(Debug, Clone, Default)]
pub(crate) struct Scrape {
    /// Family name (no `_total`) to its type.
    pub(crate) types: BTreeMap<String, String>,
    /// Family name to its help text, as written.
    pub(crate) helps: BTreeMap<String, String>,
    pub(crate) samples: Vec<Sample>,
    /// Whether the text ended with `# EOF`.
    pub(crate) ended: bool,
}

impl Scrape {
    /// The exposition of `recorder`, parsed.
    pub(crate) fn of(recorder: &Recorder) -> Self {
        let exposition = recorder.exposition();
        Self::parse(&String::from_utf8(exposition.body).expect("the exposition is text"))
    }

    pub(crate) fn parse(text: &str) -> Self {
        let mut scrape = Self::default();
        for line in text.lines() {
            if line == "# EOF" {
                scrape.ended = true;
            } else if let Some(rest) = line.strip_prefix("# TYPE ") {
                let (name, kind) = rest.split_once(' ').expect("a TYPE line");
                scrape.types.insert(name.to_owned(), kind.to_owned());
            } else if let Some(rest) = line.strip_prefix("# HELP ") {
                let (name, help) = rest.split_once(' ').expect("a HELP line");
                scrape.helps.insert(name.to_owned(), help.to_owned());
            } else if !line.starts_with('#') && !line.is_empty() {
                scrape.samples.push(parse_sample(line));
            }
        }
        scrape
    }

    /// The value of the sample `name` with exactly `labels`.
    pub(crate) fn value(&self, name: &str, labels: &[(&str, &str)]) -> Option<f64> {
        self.samples
            .iter()
            .find(|sample| {
                sample.name == name
                    && sample
                        .labels
                        .iter()
                        .map(|(key, value)| (key.as_str(), value.as_str()))
                        .eq(labels.iter().copied())
            })
            .map(|sample| sample.value)
    }

    /// The label names of the samples of the family `name` (but `le`), sorted and joined by `,`.
    pub(crate) fn label_names(&self, family: &str) -> String {
        let mut names: Vec<&str> = self
            .samples
            .iter()
            .filter(|sample| owns(family, &sample.name))
            .flat_map(|sample| sample.labels.iter().map(|(key, _)| key.as_str()))
            .filter(|key| *key != "le")
            .collect();
        names.sort_unstable();
        names.dedup();
        names.join(",")
    }
}

/// Whether the sample `sample` belongs to the family `family`.
fn owns(family: &str, sample: &str) -> bool {
    sample == family
        || sample
            .strip_prefix(family)
            .is_some_and(|rest| matches!(rest, "_total" | "_bucket" | "_sum" | "_count"))
}

fn parse_sample(line: &str) -> Sample {
    let (head, value) = line.rsplit_once(' ').expect("a sample line");
    let value = match value {
        "+Inf" => f64::INFINITY,
        "-Inf" => f64::NEG_INFINITY,
        text => text
            .parse()
            .unwrap_or_else(|error| panic!("{line}: {error}")),
    };
    let Some((name, rest)) = head.split_once('{') else {
        return Sample {
            name: head.to_owned(),
            labels: Vec::new(),
            value,
        };
    };
    let body = rest.strip_suffix('}').expect("closing brace");
    let mut labels = Vec::new();
    let mut remaining = body;
    while !remaining.is_empty() {
        let (key, after) = remaining.split_once("=\"").expect("a label");
        let mut text = String::new();
        let mut chars = after.char_indices();
        let end = loop {
            match chars.next().expect("a closing quote") {
                (_, '\\') => match chars.next().expect("an escape").1 {
                    'n' => text.push('\n'),
                    other => text.push(other),
                },
                (index, '"') => break index,
                (_, other) => text.push(other),
            }
        };
        labels.push((key.to_owned(), text));
        remaining = after[end + 1..].trim_start_matches(',');
    }
    Sample {
        name: name.to_owned(),
        labels,
        value,
    }
}
