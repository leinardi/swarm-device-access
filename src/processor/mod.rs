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

//! The reconcile of a container's device grants ([`Processor`]): what each `/dev` bind mount of
//! a container names ([`devfs`], [`rules`]), the process whose cgroup gets the rules ([`pin`]),
//! and the history of runs revocations are resolved against ([`lifecycle`]). Go's
//! `internal/processor`.

pub mod devfs;
mod error;
#[cfg(test)]
mod fake_devfs;
pub mod lifecycle;
pub mod pin;
mod reconcile;
pub mod rules;

pub use error::{Error, VerifyCause};
pub use reconcile::{Inspector, PROVISIONAL_TTL, PassRequester, Processor, Settings};
