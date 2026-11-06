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

// Adapted from swarm-scheduler-exporter/src/docker/mod.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The daemon's read-only Docker API on rust-common's `docker-engine` client: the [`DockerApi`]
//! the daemon calls, the [`types`] it decodes and its event stream. The crate is reached through
//! this module only (`meta/tests/docker_boundary.rs`).
//!
//! - [`Settings::unix`] takes the socket the daemon is configured with (`--docker-socket`), as the
//!   Go daemon's `client.New(client.WithHost("unix://" + socket))` does, with the crate's
//!   defaults: moby's own `User-Agent` and no response-header timeout. Nothing else is dialed:
//!   the crate's `tcp` and `tls` are off, and no `DOCKER_*` variable is read.
//! - [`Client`] sends HTTP/1.1 with moby's default transport: the `api.moby.localhost` host, a
//!   10s dial, up to two idle connections kept for 30s, no compression and no response-header
//!   timeout, every call being bounded by its caller ([`with_context`], or
//!   [`with_context_timeout`], which keeps net/http's `Get "<url>": context deadline exceeded`).
//! - The first versioned request pings `/_ping` (`HEAD`, then `GET`) and negotiates the API
//!   version down from 1.56; a failed negotiation is not an error, exactly as in moby.
//!
//! Read-only by construction: [`DockerApi`] has the ping, list, inspect and events calls only, and
//! builds them on the client's `GET`s. The launcher's write calls are its own engine on the same
//! client (`src/launcher/engine.rs`, the only caller of `Client::exchange`).

mod api;
pub mod types;

pub use api::{Client, ClientEventStream, DockerApi, EventStream};
pub use docker_engine::context;
pub(crate) use docker_engine::stdcopy::std_copy;
/// The fake engine core and the helpers the unit tests share (`src/test_support.rs`).
#[cfg(test)]
pub(crate) use docker_engine::test_util;
pub(crate) use docker_engine::wait::wait_answer;
pub use docker_engine::{
    Body, CallContext, ConfigError, ContextError, Error, ErrorKind, Filters, MAX_API_VERSION,
    MIN_API_VERSION, Settings, StreamError, StreamErrorKind, since_parameter, with_context,
    with_context_timeout,
};
pub(crate) use docker_engine::{encode_query, ensure_reader_closed, push_json_string, trim_id};

#[cfg(test)]
mod tests;
