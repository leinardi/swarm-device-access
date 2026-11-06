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

//! The daemon's read-only Docker API on rust-common's `docker-client` (bollard): the [`DockerApi`]
//! the daemon calls, the [`types`] it reads and its event stream. The crate is reached through this
//! module only (`meta/tests/docker_boundary.rs`).
//!
//! - [`Client::unix`] takes the socket the daemon is configured with (`--docker-socket`), with
//!   docker-client's defaults: the unix socket only (its `tcp` and `tls` are off, and no `DOCKER_*`
//!   variable is read), a `swarm-device-access/<version>` `User-Agent` and a response-header
//!   timeout the caller sets.
//! - Every call is bounded as a whole, the body included: by the caller ([`with_context`], Go's
//!   `context`) and by the client's own `bounded` call timeout, because bollard's timeout stops
//!   at the response headers.
//! - The API version is negotiated down from bollard's on the first call that reaches an engine; a
//!   failed negotiation is not an error, as in moby.
//!
//! Read-only by construction: [`DockerApi`] has the ping, list, inspect and events calls only. The
//! launcher's write calls are its own engine on the same client (`src/launcher/engine.rs`, the
//! only caller of `Client::transport`).

mod api;
pub mod context;
mod error;
pub mod types;

pub use api::{Client, ClientEventStream, DockerApi, EventStream, Filters};
pub(crate) use api::{bounded, trim_id};
pub use context::{CallContext, ContextError, with_context};
pub use docker_client::ConnectError;
/// docker-client's error, which a stream of output carries item by item.
pub(crate) use docker_client::Error as ClientError;
/// The fake engine on a unix socket that the unit tests share.
#[cfg(test)]
pub(crate) use docker_client::test_util;
/// bollard's models and its attach and wait pumps, for the launcher's engine.
pub(crate) use docker_client::{CancellationToken, attach, bollard};
#[cfg(test)]
pub(crate) use error::stream_failure;
pub use error::{Error, ErrorKind, StreamError, StreamErrorKind};

#[cfg(test)]
mod tests;
