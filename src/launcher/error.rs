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

// Adapted from swarm-rootless-docker/src/launcher/mod.rs at 16913a4d87d8,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The launcher's errors, SRD's `LaunchError` rewritten by hand with the Go launcher's texts:
//! each `Display` is the Go error string, its `%w` chain and `errors.Join` included.

use std::fmt;
use std::time::Duration;

use super::self_id::SelfIdError;
use super::short_id;
use super::spec::DAEMON_NAME;
use crate::docker::{ContextError, Error};
use crate::go_compat::{duration, errors, quote::quote};

/// The launcher failed; it then exits with status 1.
#[derive(Debug)]
pub enum LaunchError {
    /// `identify launcher container: <cause>`.
    Identify(SelfIdError),
    /// `inspect launcher container <short>: <cause>`.
    InspectSelf { launcher: String, source: Error },
    /// Go's `errNoImageID`: the launcher runs an image named by a tag, not by its ID.
    NoImageId(String),
    /// `list swarm-device-access containers: <cause>`.
    List(Error),
    /// A daemon container that is not provably an orphan.
    Stale(StaleError),
    /// `remove stale daemon <short>: <cause>`.
    RemoveStale { id: String, source: RemoveError },
    /// `create daemon container: <cause>`.
    Create(Error),
    /// `attach to daemon container: <cause>`.
    Attach(EstablishError),
    /// `wait for daemon container: <cause>`.
    Wait(EstablishError),
    /// `start daemon container: <cause>`.
    Start(Error),
    /// Go's `abandon`: `errors.Join(cause, removeErr)` of a setup failure whose daemon could not
    /// be removed.
    Abandoned {
        cause: Box<LaunchError>,
        remove: RemoveError,
    },
    /// Go's `errLogStream`, joined with the shutdown's error.
    LogStream {
        copy: String,
        shutdown: Option<Box<LaunchError>>,
    },
    /// Go's `errWaitStream`, joined with the removal's error.
    WaitStream {
        wait: String,
        removal: Option<BudgetExceeded>,
    },
    /// The shutdown ran out of its budget.
    Budget(BudgetExceeded),
}

impl fmt::Display for LaunchError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Identify(cause) => write!(formatter, "identify launcher container: {cause}"),
            Self::InspectSelf { launcher, source } => write!(
                formatter,
                "inspect launcher container {}: {source}",
                short_id(launcher)
            ),
            Self::NoImageId(image) => write!(
                formatter,
                "launcher container has no content-addressed image ID: {}",
                quote(image)
            ),
            Self::List(source) => write!(formatter, "list {DAEMON_NAME} containers: {source}"),
            Self::Stale(cause) => cause.fmt(formatter),
            Self::RemoveStale { id, source } => {
                write!(formatter, "remove stale daemon {}: {source}", short_id(id))
            }
            Self::Create(source) => write!(formatter, "create daemon container: {source}"),
            Self::Attach(cause) => write!(formatter, "attach to daemon container: {cause}"),
            Self::Wait(cause) => write!(formatter, "wait for daemon container: {cause}"),
            Self::Start(source) => write!(formatter, "start daemon container: {source}"),
            Self::Abandoned { cause, remove } => write_joined(
                formatter,
                [Some(cause.to_string()), Some(remove.to_string())],
            ),
            Self::LogStream { copy, shutdown } => write_joined(
                formatter,
                [
                    Some(format!(
                        "daemon log stream lost while the daemon was running: {copy}"
                    )),
                    shutdown.as_ref().map(ToString::to_string),
                ],
            ),
            Self::WaitStream { wait, removal } => write_joined(
                formatter,
                [
                    Some(format!("daemon wait stream failed: {wait}")),
                    removal.as_ref().map(ToString::to_string),
                ],
            ),
            Self::Budget(exceeded) => exceeded.fmt(formatter),
        }
    }
}

impl std::error::Error for LaunchError {}

/// Why a daemon container found by name was left alone.
#[derive(Debug)]
pub enum StaleError {
    /// Go's `errForeignContainer`: neither a launcher's daemon nor the sh wrapper's.
    Foreign { id: String },
    /// Go's `errNoOwner`.
    NoOwner { id: String },
    /// The owner's inspect failed, which says nothing about whether it still runs.
    InspectOwner {
        owner: String,
        id: String,
        source: Error,
    },
    /// Go's `errOwnerState`.
    OwnerState { owner: String, id: String },
    /// Go's `errOwnedByRunningLauncher`.
    OwnedByRunningLauncher { owner: String },
}

impl fmt::Display for StaleError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Foreign { id } => write!(
                formatter,
                "{DAEMON_NAME} ({}): container is not a swarm-device-access daemon",
                short_id(id)
            ),
            Self::NoOwner { id } => write!(
                formatter,
                "{DAEMON_NAME} ({}): daemon container has no launcher label",
                short_id(id)
            ),
            Self::InspectOwner { owner, id, source } => write!(
                formatter,
                "inspect launcher {} of daemon {}: {source}",
                short_id(owner),
                short_id(id)
            ),
            Self::OwnerState { owner, id } => write!(
                formatter,
                "launcher {} of daemon {}: inspect reported no state",
                short_id(owner),
                short_id(id)
            ),
            Self::OwnedByRunningLauncher { owner } => write!(
                formatter,
                "daemon owned by running launcher {}",
                short_id(owner)
            ),
        }
    }
}

impl std::error::Error for StaleError {}

/// `remove container <short>: <cause>`.
#[derive(Debug)]
pub struct RemoveError {
    pub id: String,
    pub source: Error,
}

impl fmt::Display for RemoveError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "remove container {}: {}",
            short_id(&self.id),
            self.source
        )
    }
}

impl std::error::Error for RemoveError {}

/// Why a stream was not established.
#[derive(Debug)]
pub enum EstablishError {
    /// Go's `errEstablishTimeout`.
    Timeout,
    /// The launcher was stopped first: `canceled: <context error>`.
    Canceled(ContextError),
    /// The call failed.
    Failed(Error),
}

impl fmt::Display for EstablishError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Timeout => formatter.write_str("not established before timeout"),
            Self::Canceled(why) => write!(formatter, "canceled: {}", why.text()),
            Self::Failed(source) => source.fmt(formatter),
        }
    }
}

impl std::error::Error for EstablishError {}

/// Go's `errShutdownBudget`, `daemon not removed within the shutdown budget (<budget>)`, joined
/// with the last inspect's error when the last inspect of the removal failed: a permission or
/// connection error is the diagnosis, not the budget.
#[derive(Debug)]
pub struct BudgetExceeded {
    pub budget: Duration,
    pub last_inspect: Option<Error>,
}

impl fmt::Display for BudgetExceeded {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let budget = duration::format(i64::try_from(self.budget.as_nanos()).unwrap_or(i64::MAX));
        write_joined(
            formatter,
            [
                Some(format!(
                    "daemon not removed within the shutdown budget ({budget})"
                )),
                self.last_inspect
                    .as_ref()
                    .map(|source| format!("last inspect of daemon: {source}")),
            ],
        )
    }
}

/// Go's `errors.Join` of `parts`, as its `Error` reads.
fn write_joined<const N: usize>(
    formatter: &mut fmt::Formatter<'_>,
    parts: [Option<String>; N],
) -> fmt::Result {
    formatter.write_str(&errors::join(parts).unwrap_or_default())
}

impl std::error::Error for BudgetExceeded {}
