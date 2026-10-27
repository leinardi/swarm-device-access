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

// Adapted from swarm-scheduler-exporter/src/docker/api.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The read-only Docker API the daemon uses: the four calls of [`DockerApi`] and nothing else,
//! with moby client v0.6.0's requests for the options the Go daemon passes.
//!
//! The requests: `HEAD /_ping` (then `GET`) for the ping, `GET /containers/json` without
//! options (running containers only), `GET /containers/{id}/json`, and `GET /events` with
//! `filters` (moby's JSON, keys sorted) and `since` (`<seconds>.<9 digits>`), the query keys
//! sorted and escaped as Go's `url.Values.Encode` does.

use std::collections::{BTreeMap, BTreeSet};
use std::future::Future;
use std::time::Duration;

use http_body_util::BodyExt as _;
use hyper::header::CONTENT_TYPE;
use jiff::Timestamp;
use serde::Deserialize;

use super::client::{Body, Client};
use super::context::{CallContext, with_context};
use super::error::Error;
use super::events::{EVENTS_ACCEPT, JsonStream, StreamError};
use super::host::push_percent;
use super::types::{ContainerInspect, ContainerSummary, Event, decode_first, decode_list};
use crate::go_compat::quote::push_hex_escape;

/// The Docker calls the daemon makes. Every one but [`DockerApi::events`] is a single request
/// and response, which the caller bounds with [`with_timeout`]; the event stream is long-lived
/// and has no deadline.
pub trait DockerApi: Send + Sync {
    /// The event stream [`DockerApi::events`] opens.
    type Events: EventStream;

    /// moby's `Ping` without `NegotiateAPIVersion`: `HEAD /_ping`, then `GET /_ping` when that
    /// fails or is not a 200.
    fn ping(&self) -> impl Future<Output = Result<(), Error>> + Send;

    /// moby's `ContainerList` with no options: `GET /containers/json`, the running containers.
    fn container_list(&self) -> impl Future<Output = Result<Vec<ContainerSummary>, Error>> + Send;

    /// moby's `ContainerInspect` without size: `GET /containers/{id}/json`.
    fn container_inspect(
        &self,
        id: &str,
    ) -> impl Future<Output = Result<ContainerInspect, Error>> + Send;

    /// moby's `Events` from `since` with `filters`: resolves once the response headers passed
    /// moby's status check, which is when the stream counts as established, and fails without a
    /// stream otherwise.
    fn events(
        &self,
        since: Timestamp,
        filters: &Filters,
    ) -> impl Future<Output = Result<Self::Events, Error>> + Send;
}

/// An open event stream.
pub trait EventStream: Send {
    /// The next event, or how the stream ended; after an error the stream is over.
    fn next(&mut self) -> impl Future<Output = Result<Event, StreamError>> + Send;
}

/// The event stream of the Docker client.
#[derive(Debug)]
pub struct ClientEventStream {
    stream: JsonStream<Body>,
}

impl ClientEventStream {
    /// Whether the response was a JSON text sequence, read through the RS filter.
    #[must_use]
    pub fn filters_rs(&self) -> bool {
        self.stream.filters_rs()
    }
}

impl EventStream for ClientEventStream {
    async fn next(&mut self) -> Result<Event, StreamError> {
        self.stream.next().await
    }
}

/// moby's `Filters`: terms with sets of values, sent as `{"term":{"value":true}}`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Filters {
    terms: BTreeMap<String, BTreeSet<String>>,
}

impl Filters {
    /// No filters.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// moby's `Filters.Add`.
    #[must_use]
    pub fn add(mut self, term: &str, values: &[&str]) -> Self {
        let set = self.terms.entry(term.to_owned()).or_default();
        set.extend(values.iter().map(|value| (*value).to_owned()));
        self
    }

    /// Whether there are none, in which case no `filters` parameter is sent.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.terms.is_empty()
    }

    /// The `filters` parameter: Go's `json.Marshal` of the map, keys sorted.
    #[must_use]
    pub fn to_json(&self) -> String {
        let mut out = String::from("{");
        for (term_index, (term, values)) in self.terms.iter().enumerate() {
            if term_index > 0 {
                out.push(',');
            }
            push_json_string(&mut out, term);
            out.push_str(":{");
            for (value_index, value) in values.iter().enumerate() {
                if value_index > 0 {
                    out.push(',');
                }
                push_json_string(&mut out, value);
                out.push_str(":true");
            }
            out.push('}');
        }
        out.push('}');
        out
    }
}

/// encoding/json's string encoding with `json.Marshal`'s HTML escaping.
fn push_json_string(out: &mut String, text: &str) {
    out.push('"');
    for character in text.chars() {
        match character {
            '"' | '\\' => {
                out.push('\\');
                out.push(character);
            }
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            '<' | '>' | '&' | '\u{2028}' | '\u{2029}' | '\0'..='\u{1f}' => {
                push_hex_escape(out, 'u', u32::from(character), 4);
            }
            _ => out.push(character),
        }
    }
    out.push('"');
}

/// Go's `url.Values.Encode`: the pairs sorted by key, each side `url.QueryEscape`d.
fn encode_query(pairs: &mut [(&str, String)]) -> String {
    pairs.sort_by(|left, right| left.0.cmp(right.0));
    let mut out = String::new();
    for (key, value) in pairs.iter() {
        if !out.is_empty() {
            out.push('&');
        }
        push_query_escaped(&mut out, key);
        out.push('=');
        push_query_escaped(&mut out, value);
    }
    out
}

/// Go's `url.QueryEscape`: the unreserved bytes kept, a space as `+`, the rest `%XX`.
fn push_query_escaped(out: &mut String, text: &str) {
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(byte));
        } else if byte == b' ' {
            out.push('+');
        } else {
            push_percent(out, byte);
        }
    }
}

/// moby's `GetTimestamp` of the RFC 3339 time the Go daemon passes as `since`:
/// `<seconds>.<nanoseconds>`, nine digits, the seconds rounded down.
#[must_use]
pub fn since_parameter(since: Timestamp) -> String {
    let mut seconds = since.as_second();
    let mut nanos = since.subsec_nanosecond();
    if nanos < 0 {
        seconds -= 1;
        nanos += 1_000_000_000;
    }
    format!("{seconds}.{nanos:09}")
}

/// moby's `trimID`: surrounding whitespace dropped, an empty ID refused.
fn trim_id<'id>(kind: &str, id: &'id str) -> Result<&'id str, Error> {
    let id = id.trim();
    if id.is_empty() {
        return Err(Error::invalid_parameter(format!(
            "invalid {kind} name or ID: value is empty"
        )));
    }
    Ok(id)
}

/// Bounds `call` by `timeout`, as `context.WithTimeout` bounds a Go call: [`with_context`] under
/// a context with that deadline only.
///
/// # Errors
///
/// The call's [`Error`], or the deadline's (see [`with_context`]) when the time ran out first.
pub async fn with_timeout<T>(
    timeout: Duration,
    call: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    with_context(&CallContext::default().with_timeout(timeout), call).await
}

impl Client {
    /// The whole body of a `GET`, read to its end.
    async fn get_body(&self, path: &str, query: &str) -> Result<Vec<u8>, Error> {
        let response = self.get(path, query, &[]).await?;
        Ok(response.into_body().collect().await?.to_bytes().to_vec())
    }

    async fn get_list<T>(&self, path: &str, query: &str) -> Result<Vec<T>, Error>
    where
        T: Default + for<'de> Deserialize<'de>,
    {
        decode_list(&self.get_body(path, query).await?).map_err(Error::decode)
    }

    async fn get_object<T>(&self, path: &str) -> Result<T, Error>
    where
        T: Default + for<'de> Deserialize<'de>,
    {
        decode_first(&self.get_body(path, "").await?).map_err(Error::decode)
    }
}

impl DockerApi for Client {
    type Events = ClientEventStream;

    async fn ping(&self) -> Result<(), Error> {
        Client::ping(self).await.map(|_| ())
    }

    async fn container_list(&self) -> Result<Vec<ContainerSummary>, Error> {
        self.get_list("/containers/json", "").await
    }

    async fn container_inspect(&self, id: &str) -> Result<ContainerInspect, Error> {
        let id = trim_id("container", id)?;
        self.get_object(&format!("/containers/{id}/json")).await
    }

    async fn events(
        &self,
        since: Timestamp,
        filters: &Filters,
    ) -> Result<ClientEventStream, Error> {
        let mut pairs = vec![("since", since_parameter(since))];
        if !filters.is_empty() {
            pairs.push(("filters", filters.to_json()));
        }
        let response = self
            .get("/events", &encode_query(&mut pairs), &EVENTS_ACCEPT)
            .await?;
        let content_type = response
            .headers()
            .get(CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        Ok(ClientEventStream {
            stream: JsonStream::new(response.into_body(), content_type.as_deref()),
        })
    }
}

#[cfg(test)]
mod tests;
