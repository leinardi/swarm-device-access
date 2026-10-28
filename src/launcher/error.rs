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
    /// be removed. [`std::error::Error::source`] exposes `cause`; `remove` retains the cleanup
    /// failure separately.
    Abandoned {
        cause: Box<LaunchError>,
        remove: RemoveError,
    },
    /// Go's `errLogStream`, joined with the shutdown's error. The stream failure is text only;
    /// [`std::error::Error::source`] exposes the typed shutdown error, when present.
    LogStream {
        copy: String,
        shutdown: Option<Box<LaunchError>>,
    },
    /// Go's `errWaitStream`, joined with the removal's error. The stream failure is text only;
    /// [`std::error::Error::source`] exposes the typed removal error, when present.
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

impl std::error::Error for LaunchError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Identify(source) => Some(source),
            Self::InspectSelf { source, .. }
            | Self::List(source)
            | Self::Create(source)
            | Self::Start(source) => Some(source),
            Self::Stale(source) => Some(source),
            Self::RemoveStale { source, .. } => Some(source),
            Self::Attach(source) | Self::Wait(source) => Some(source),
            Self::Abandoned { cause, .. } => Some(cause.as_ref()),
            Self::LogStream { shutdown, .. } => shutdown
                .as_deref()
                .map(|error| error as &dyn std::error::Error),
            Self::WaitStream { removal, .. } => removal
                .as_ref()
                .map(|error| error as &dyn std::error::Error),
            Self::Budget(source) => Some(source),
            Self::NoImageId(_) => None,
        }
    }
}

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

impl std::error::Error for StaleError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::InspectOwner { source, .. } => Some(source),
            Self::Foreign { .. }
            | Self::NoOwner { .. }
            | Self::OwnerState { .. }
            | Self::OwnedByRunningLauncher { .. } => None,
        }
    }
}

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

impl std::error::Error for RemoveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        Some(&self.source)
    }
}

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

impl std::error::Error for EstablishError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Canceled(source) => Some(source),
            Self::Failed(source) => Some(source),
            Self::Timeout => None,
        }
    }
}

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

impl std::error::Error for BudgetExceeded {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        self.last_inspect
            .as_ref()
            .map(|error| error as &dyn std::error::Error)
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn launch_errors_expose_their_typed_sources_without_changing_text() {
        let error = LaunchError::RemoveStale {
            id: "daemon".to_owned(),
            source: RemoveError {
                id: "daemon".to_owned(),
                source: Error::deadline_exceeded(),
            },
        };
        assert_eq!(
            error.to_string(),
            "remove stale daemon daemon: remove container daemon: context deadline exceeded"
        );
        let remove = error
            .source()
            .unwrap()
            .downcast_ref::<RemoveError>()
            .unwrap();
        let source = remove.source().unwrap().downcast_ref::<Error>().unwrap();
        assert_eq!(source.kind(), crate::docker::ErrorKind::DeadlineExceeded);
        assert!(EstablishError::Timeout.source().is_none());
        assert!(LaunchError::NoImageId("tag".to_owned()).source().is_none());
        for cause in [ContextError::Canceled, ContextError::DeadlineExceeded] {
            let error = EstablishError::Canceled(cause);
            let source = error
                .source()
                .unwrap()
                .downcast_ref::<ContextError>()
                .unwrap();
            assert_eq!(*source, cause);
            assert_eq!(source.to_string(), cause.text());
        }
    }

    #[test]
    fn abandoned_launch_keeps_primary_and_cleanup_causes() {
        let error = LaunchError::Abandoned {
            cause: Box::new(LaunchError::Start(Error::deadline_exceeded())),
            remove: RemoveError {
                id: "daemon".to_owned(),
                source: Error::context(ContextError::Canceled),
            },
        };
        assert!(matches!(
            error.source().unwrap().downcast_ref::<LaunchError>(),
            Some(LaunchError::Start(_))
        ));
        let LaunchError::Abandoned { remove, .. } = &error else {
            panic!("expected abandoned launch");
        };
        assert_eq!(remove.source().unwrap().to_string(), "context canceled");
        assert_eq!(
            error.to_string(),
            "start daemon container: context deadline exceeded\nremove container daemon: context canceled"
        );
    }

    #[test]
    fn owner_stream_and_budget_failures_retain_typed_causes() {
        let owner = StaleError::InspectOwner {
            owner: "launcher".to_owned(),
            id: "daemon".to_owned(),
            source: Error::deadline_exceeded(),
        };
        assert!(owner.source().unwrap().is::<Error>());
        let stream = EstablishError::Failed(Error::deadline_exceeded());
        assert!(stream.source().unwrap().is::<Error>());

        let budget = BudgetExceeded {
            budget: Duration::from_secs(20),
            last_inspect: Some(Error::deadline_exceeded()),
        };
        assert!(budget.source().unwrap().is::<Error>());
        let wait = LaunchError::WaitStream {
            wait: "EOF".to_owned(),
            removal: Some(budget),
        };
        assert!(wait.source().unwrap().is::<BudgetExceeded>());
        let log = LaunchError::LogStream {
            copy: "EOF".to_owned(),
            shutdown: Some(Box::new(LaunchError::Start(Error::deadline_exceeded()))),
        };
        assert!(log.source().unwrap().is::<LaunchError>());
        let without_cleanup = LaunchError::LogStream {
            copy: "EOF".to_owned(),
            shutdown: None,
        };
        assert!(without_cleanup.source().is_none());
        assert!(
            BudgetExceeded {
                budget: Duration::from_secs(20),
                last_inspect: None,
            }
            .source()
            .is_none()
        );
    }
}
