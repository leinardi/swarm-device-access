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

//! The daemon's read-only Docker Engine client, in-tree on hyper: the transport and moby client
//! v0.6.0's lazy API version negotiation, the read-only [`DockerApi`] on top of it, the
//! [`types`] it decodes and the [`events`] stream.
//!
//! - [`Settings::unix`] takes the socket the daemon is configured with (`--docker-socket`), as the
//!   Go daemon's `client.New(client.WithHost("unix://" + socket))` does. Nothing else is dialed:
//!   no TCP, TLS or proxy, and no `DOCKER_*` variable is read.
//! - [`Client`] sends HTTP/1.1 with moby's default transport: the `api.moby.localhost` host, a
//!   10s dial, up to two idle connections kept for 30s, no compression and no response-header
//!   timeout, every call being bounded by its caller.
//! - The first versioned request pings `/_ping` (`HEAD`, then `GET`) and negotiates the API
//!   version down from 1.56; a failed negotiation is not an error, exactly as in moby.
//!
//! Read-only by construction: the request builder only makes `GET` requests, `HEAD` is set in
//! one place, the ping, on `/_ping`, and [`DockerApi`] has the ping, list, inspect and events
//! calls only. The launcher's write calls are a separate engine on the same transport.

mod api;
mod client;
mod connect;
mod error;
pub mod events;
mod host;
mod pool;
pub mod types;
pub mod version;

pub use api::{ClientEventStream, DockerApi, EventStream, Filters, since_parameter, with_timeout};
pub use client::{Body, Client, Settings};
pub use error::{ConfigError, Error, ErrorKind};
pub use events::{StreamError, StreamErrorKind};
pub use version::{MAX_API_VERSION, MIN_API_VERSION};

use std::time::Duration;

/// moby's `DummyHost`: the `Host` header of requests over a unix socket.
pub const DUMMY_HOST: &str = "api.moby.localhost";

/// go-connections' dial timeout of a unix socket.
pub const DIAL_TIMEOUT: Duration = Duration::from_secs(10);

#[cfg(test)]
mod tests;
