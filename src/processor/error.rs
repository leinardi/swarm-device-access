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

//! Why a reconcile, a termination or a revocation failed, worded as the Go processor's errors,
//! with Go's `errors.Is` checks as methods that walk every wrap and join.

use std::fmt;

use super::pin::{PinError, ProcCgroupError};
use super::rules::DeviceError;
use crate::cgroup::{self, HandleError};
use crate::docker::{self, ErrorKind};
use crate::go_compat::quote::quote;

/// A failed reconcile, termination or revocation.
#[derive(Debug)]
pub enum Error {
    /// `inspect container "<id>": <cause>`.
    Inspect { id: String, source: docker::Error },
    /// Go's `ErrContainerGone`: Docker does not know the container and nothing is left to revoke;
    /// `container no longer exists: <inspect error>`.
    ContainerGone(Box<Error>),
    /// Go's `errors.Join`: the texts on separate lines. [`std::error::Error::source`] exposes the
    /// first error; this variant retains every branch for callers that need to inspect them all.
    Joined(Vec<Error>),
    /// `container "<id>": pidfd_open(<pid>): <errno>`.
    Pin { id: String, source: PinError },
    /// `container "<id>": <cause>`: the process's cgroup could not be read.
    ProcCgroup { id: String, source: ProcCgroupError },
    /// `set device rules: <cause>`: the resolved cgroup could not be opened.
    OpenCgroup(HandleError),
    /// `set device rules: <cause>`.
    SetRules(Box<cgroup::Error>),
    /// `verify container "<id>": <cause>`: the second inspect or the membership read failed.
    Verify { id: String, cause: VerifyCause },
    /// Go's `errLifecycleChanged`: `container "<id>" pid <pid>: container changed between inspect
    /// and mutation`.
    LifecycleChanged { id: String, pid: i64 },
    /// Go's `errPinnedProcessGone`: `container "<id>" pid <pid>: pinned container process exited:
    /// <errno>`.
    PinnedGone {
        id: String,
        pid: i64,
        source: PinError,
    },
    /// Go's `errNotInCgroup`: `container "<id>" pid <pid>: container process is not in the
    /// resolved cgroup`.
    NotInCgroup { id: String, pid: i64 },
    /// `revoke container "<id>": <cause>`: the recorded cgroup could not be opened.
    RevokeOpen { id: String, source: HandleError },
    /// `revoke container "<id>" in "<path>": <cause>`.
    Revoke {
        id: String,
        path: String,
        source: Box<cgroup::Error>,
    },
    /// `revoke container "<id>" in its previous cgroup "<path>": <cause>`.
    RevokeMoved {
        id: String,
        path: String,
        source: Box<cgroup::Error>,
    },
    /// `container "<id>" (reason <reason>, retryable): <cause>`: a cgroup failure the container
    /// is retried for.
    Retryable {
        id: String,
        reason: &'static str,
        source: Box<Error>,
    },
    /// `container "<id>" (reason incomplete_device_set, retryable): <the device errors joined>`.
    /// [`std::error::Error::source`] exposes the first device error; `errors` retains them all.
    Incomplete {
        id: String,
        errors: Vec<DeviceError>,
    },
    /// The reconcile never ran: the runtime shut down before its blocking transaction started.
    /// Worded as the context error a Go caller would see then.
    Canceled,
}

/// What failed in the check made immediately before a mutation.
#[derive(Debug)]
pub enum VerifyCause {
    /// The second inspect.
    Inspect(Box<Error>),
    /// The read of `cgroup.procs`.
    Membership(HandleError),
}

impl Error {
    /// Whether it is or wraps Go's `ErrContainerGone`.
    #[must_use]
    pub fn is_container_gone(&self) -> bool {
        self.any(&|error| matches!(error, Self::ContainerGone(_)))
    }

    /// Whether it is or wraps Go's `errLifecycleChanged`.
    #[must_use]
    pub fn is_lifecycle_changed(&self) -> bool {
        self.any(&|error| matches!(error, Self::LifecycleChanged { .. }))
    }

    /// Whether it is or wraps Go's `errPinnedProcessGone`.
    #[must_use]
    pub fn is_pinned_process_gone(&self) -> bool {
        self.any(&|error| matches!(error, Self::PinnedGone { .. }))
    }

    /// Whether it is or wraps Go's `errNotInCgroup`.
    #[must_use]
    pub fn is_not_in_cgroup(&self) -> bool {
        self.any(&|error| matches!(error, Self::NotInCgroup { .. }))
    }

    /// Whether it wraps a Docker error of `kind` (Go's `errors.Is` on `context.Canceled` or
    /// `context.DeadlineExceeded`).
    #[must_use]
    pub fn is_docker(&self, kind: ErrorKind) -> bool {
        self.any(&|error| matches!(error, Self::Inspect { source, .. } if source.kind() == kind))
    }

    /// Whether it wraps a cgroup error `matches` accepts (`cgroup::Error::is_filter_missing` and
    /// the other classifiers).
    #[must_use]
    pub fn is_cgroup(&self, matches: fn(&cgroup::Error) -> bool) -> bool {
        self.any(&|error| match error {
            Self::SetRules(source)
            | Self::Revoke { source, .. }
            | Self::RevokeMoved { source, .. } => matches(source),
            Self::Inspect { .. }
            | Self::Retryable { .. }
            | Self::ContainerGone(_)
            | Self::Joined(_)
            | Self::Pin { .. }
            | Self::ProcCgroup { .. }
            | Self::OpenCgroup(_)
            | Self::Verify { .. }
            | Self::LifecycleChanged { .. }
            | Self::PinnedGone { .. }
            | Self::NotInCgroup { .. }
            | Self::RevokeOpen { .. }
            | Self::Incomplete { .. }
            | Self::Canceled => false,
        })
    }

    /// Whether the inspect behind it got a 404 from Docker: containerd's `errdefs.IsNotFound`.
    #[must_use]
    pub fn is_not_found(&self) -> bool {
        self.any(&|error| matches!(error, Self::Inspect { source, .. } if source.is_not_found()))
    }

    /// Whether `matches` accepts this error or any error it wraps or joins.
    fn any(&self, matches: &dyn Fn(&Self) -> bool) -> bool {
        if matches(self) {
            return true;
        }
        match self {
            Self::ContainerGone(inner)
            | Self::Retryable { source: inner, .. }
            | Self::Verify {
                cause: VerifyCause::Inspect(inner),
                ..
            } => inner.any(matches),
            Self::Joined(errors) => errors.iter().any(|error| error.any(matches)),
            Self::Inspect { .. }
            | Self::Pin { .. }
            | Self::ProcCgroup { .. }
            | Self::OpenCgroup(_)
            | Self::SetRules(_)
            | Self::Verify {
                cause: VerifyCause::Membership(_),
                ..
            }
            | Self::LifecycleChanged { .. }
            | Self::PinnedGone { .. }
            | Self::NotInCgroup { .. }
            | Self::RevokeOpen { .. }
            | Self::Revoke { .. }
            | Self::RevokeMoved { .. }
            | Self::Incomplete { .. }
            | Self::Canceled => false,
        }
    }
}

/// Go's `errors.Join` of the errors that are present: `None` when there are none.
pub(super) fn join(errors: impl IntoIterator<Item = Option<Error>>) -> Option<Error> {
    let errors: Vec<Error> = errors.into_iter().flatten().collect();
    (!errors.is_empty()).then_some(Error::Joined(errors))
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Inspect { id, source } => {
                write!(formatter, "inspect container {}: {source}", quote(id))
            }
            Self::ContainerGone(inner) => write!(formatter, "container no longer exists: {inner}"),
            Self::Joined(errors) => {
                for (index, error) in errors.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str("\n")?;
                    }
                    error.fmt(formatter)?;
                }
                Ok(())
            }
            Self::Pin { id, source } => write!(formatter, "container {}: {source}", quote(id)),
            Self::ProcCgroup { id, source } => {
                write!(formatter, "container {}: {source}", quote(id))
            }
            Self::OpenCgroup(source) => write!(formatter, "set device rules: {source}"),
            Self::SetRules(source) => write!(formatter, "set device rules: {source}"),
            Self::Verify { id, cause } => {
                write!(formatter, "verify container {}: ", quote(id))?;
                match cause {
                    VerifyCause::Inspect(inner) => inner.fmt(formatter),
                    VerifyCause::Membership(source) => source.fmt(formatter),
                }
            }
            Self::LifecycleChanged { id, pid } => write!(
                formatter,
                "container {} pid {pid}: container changed between inspect and mutation",
                quote(id)
            ),
            Self::PinnedGone { id, pid, source } => {
                write!(formatter, "container {} pid {pid}: {source}", quote(id))
            }
            Self::NotInCgroup { id, pid } => write!(
                formatter,
                "container {} pid {pid}: container process is not in the resolved cgroup",
                quote(id)
            ),
            Self::RevokeOpen { id, source } => {
                write!(formatter, "revoke container {}: {source}", quote(id))
            }
            Self::Revoke { id, path, source } => write!(
                formatter,
                "revoke container {} in {}: {source}",
                quote(id),
                quote(path)
            ),
            Self::RevokeMoved { id, path, source } => write!(
                formatter,
                "revoke container {} in its previous cgroup {}: {source}",
                quote(id),
                quote(path)
            ),
            Self::Retryable { id, reason, source } => write!(
                formatter,
                "container {} (reason {reason}, retryable): {source}",
                quote(id)
            ),
            Self::Incomplete { id, errors } => {
                write!(
                    formatter,
                    "container {} (reason incomplete_device_set, retryable): ",
                    quote(id)
                )?;
                for (index, error) in errors.iter().enumerate() {
                    if index > 0 {
                        formatter.write_str("\n")?;
                    }
                    error.fmt(formatter)?;
                }
                Ok(())
            }
            Self::Canceled => formatter.write_str(docker::ContextError::Canceled.text()),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Inspect { source, .. } => Some(source),
            Self::ContainerGone(source) | Self::Retryable { source, .. } => Some(source.as_ref()),
            Self::Joined(errors) => errors.first().map(|error| error as &dyn std::error::Error),
            Self::Pin { source, .. } | Self::PinnedGone { source, .. } => Some(source),
            Self::ProcCgroup { source, .. } => Some(source),
            Self::OpenCgroup(source) | Self::RevokeOpen { source, .. } => Some(source),
            Self::SetRules(source)
            | Self::Revoke { source, .. }
            | Self::RevokeMoved { source, .. } => Some(source.as_ref()),
            Self::Verify { cause, .. } => match cause {
                VerifyCause::Inspect(source) => Some(source.as_ref()),
                VerifyCause::Membership(source) => Some(source),
            },
            Self::Incomplete { errors, .. } => {
                errors.first().map(|error| error as &dyn std::error::Error)
            }
            Self::LifecycleChanged { .. } | Self::NotInCgroup { .. } | Self::Canceled => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::error::Error as _;

    use super::*;

    #[test]
    fn wrapped_errors_expose_their_typed_sources_without_changing_text() {
        let error = Error::ContainerGone(Box::new(Error::Inspect {
            id: "container".to_owned(),
            source: docker::Error::deadline_exceeded(),
        }));
        assert_eq!(
            error.to_string(),
            "container no longer exists: inspect container \"container\": context deadline exceeded"
        );
        let inspect = error.source().unwrap().downcast_ref::<Error>().unwrap();
        let source = inspect
            .source()
            .unwrap()
            .downcast_ref::<docker::Error>()
            .unwrap();
        assert_eq!(source.kind(), ErrorKind::DeadlineExceeded);
        assert!(Error::Canceled.source().is_none());
    }

    #[test]
    fn joined_errors_expose_the_first_cause_and_retain_all_branches() {
        let error = Error::Joined(vec![
            Error::Canceled,
            Error::NotInCgroup {
                id: "container".to_owned(),
                pid: 42,
            },
        ]);
        assert!(matches!(
            error.source().unwrap().downcast_ref::<Error>(),
            Some(Error::Canceled)
        ));
        assert!(error.is_not_in_cgroup());
        assert!(Error::Joined(Vec::new()).source().is_none());
    }

    #[test]
    fn verification_and_incomplete_errors_expose_their_first_cause() {
        let verify = Error::Verify {
            id: "container".to_owned(),
            cause: VerifyCause::Inspect(Box::new(Error::Canceled)),
        };
        assert!(matches!(
            verify.source().unwrap().downcast_ref::<Error>(),
            Some(Error::Canceled)
        ));
        let incomplete = Error::Incomplete {
            id: "container".to_owned(),
            errors: vec![DeviceError::MountSourceGone("/dev/missing".to_owned())],
        };
        assert!(matches!(
            incomplete.source().unwrap().downcast_ref::<DeviceError>(),
            Some(DeviceError::MountSourceGone(path)) if path == "/dev/missing"
        ));
        assert!(
            Error::Incomplete {
                id: "container".to_owned(),
                errors: Vec::new(),
            }
            .source()
            .is_none()
        );
    }
}
