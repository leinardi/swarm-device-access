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

//! The daemon loop: the [`Coordinator`] that owns every reconciliation pass, retry and container
//! event. Go's `internal/daemon`.

mod coordinator;

pub use coordinator::{ContainerLister, Coordinator, Reconciler, Settings};

use std::time::Duration;

/// The first delay of a retry: a failed container, a failed list, a lost event stream.
pub const MIN_BACKOFF: Duration = Duration::from_secs(1);

/// The longest delay of a retry; each failure doubles the delay up to it.
pub const MAX_BACKOFF: Duration = Duration::from_secs(30);

/// Bounds every request/response Docker API call and the establishment of the event stream.
/// Without it a wedged dockerd blocks the caller forever, and nothing behind it (retries,
/// reconnects, shutdown) ever runs.
pub const DOCKER_CALL_TIMEOUT: Duration = Duration::from_secs(10);
