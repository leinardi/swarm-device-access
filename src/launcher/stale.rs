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

//! Go's `removeStale`: the daemon containers a previous launcher left behind are removed, so
//! the name is free for this one, but only one that provably has no live owner. Any doubt is an
//! error and leaves the container alone: a transient inspect failure never makes a daemon an
//! orphan, and a container that is not a daemon is never removed.

use super::engine::{EngineApi, Summary};
use super::error::{LaunchError, RemoveError, StaleError};
use super::spec::{LABEL_LAUNCHER, LABEL_ROLE, ROLE_DAEMON};
use super::{Runner, short_id};
use crate::docker::{CallContext, with_context};
use crate::logger::Attr;

/// Identifies a daemon started by the sh wrapper of earlier releases, which recorded no owner.
const LABEL_IMAGE_TITLE: &str = "org.opencontainers.image.title";
const IMAGE_TITLE: &str = "swarm-device-access";

impl<E: EngineApi> Runner<E> {
    /// Removes every `swarm-device-access` container that may be removed, or refuses at the
    /// first that may not.
    pub(super) async fn remove_stale(
        &self,
        context: &CallContext,
        launcher_id: &str,
    ) -> Result<(), LaunchError> {
        let listed = with_context(
            &context.with_timeout(self.timing.call),
            self.engine.list_daemons(),
        )
        .await
        .map_err(LaunchError::List)?;
        for stale in &listed {
            self.check_stale(context, stale, launcher_id)
                .await
                .map_err(LaunchError::Stale)?;
            self.remove_container(&stale.id)
                .await
                .map_err(|source| LaunchError::RemoveStale {
                    id: stale.id.clone(),
                    source,
                })?;
        }
        Ok(())
    }

    /// Ok when `stale` may be removed: a daemon whose launcher is confirmed gone or stopped, one
    /// owned by this very launcher container (a restart of it), or one started by the old sh
    /// wrapper.
    async fn check_stale(
        &self,
        context: &CallContext,
        stale: &Summary,
        launcher_id: &str,
    ) -> Result<(), StaleError> {
        let label = |key: &str| stale.labels.get(key).map_or("", String::as_str);
        if label(LABEL_ROLE) != ROLE_DAEMON {
            if label(LABEL_IMAGE_TITLE) == IMAGE_TITLE {
                self.log.info(
                    "removing daemon left by the sh wrapper",
                    &[Attr::string("id", short_id(&stale.id))],
                );
                return Ok(());
            }
            return Err(StaleError::Foreign {
                id: stale.id.clone(),
            });
        }

        let owner = label(LABEL_LAUNCHER);
        if owner.is_empty() {
            return Err(StaleError::NoOwner {
                id: stale.id.clone(),
            });
        }
        if owner == launcher_id {
            self.log.info(
                "removing daemon left by an earlier run of this launcher",
                &[Attr::string("id", short_id(&stale.id))],
            );
            return Ok(());
        }

        let inspected = with_context(
            &context.with_timeout(self.timing.call),
            self.engine.inspect(owner),
        )
        .await;
        match inspected {
            Err(error) if error.is_not_found() => {}
            // A timeout or an engine error says nothing about whether the owner still runs;
            // only a confirmed absence makes the daemon an orphan.
            Err(source) => {
                return Err(StaleError::InspectOwner {
                    owner: owner.to_owned(),
                    id: stale.id.clone(),
                    source,
                });
            }
            Ok(inspected) => match inspected.state {
                None => {
                    return Err(StaleError::OwnerState {
                        owner: owner.to_owned(),
                        id: stale.id.clone(),
                    });
                }
                Some(owner_state) if owner_state.running => {
                    return Err(StaleError::OwnedByRunningLauncher {
                        owner: owner.to_owned(),
                    });
                }
                Some(_) => {}
            },
        }
        self.log.info(
            "removing orphaned daemon",
            &[
                Attr::string("id", short_id(&stale.id)),
                Attr::string("launcher", short_id(owner)),
            ],
        );
        Ok(())
    }

    /// Force-removes `container_id` by ID; one already gone is fine. It runs under a context of
    /// its own, so a failed setup still cleans up after a signal.
    pub(super) async fn remove_container(&self, container_id: &str) -> Result<(), RemoveError> {
        let removed = with_context(
            &CallContext::new().with_timeout(self.timing.call),
            self.engine.remove(container_id),
        )
        .await;
        match removed {
            Err(source) if !source.is_not_found() => Err(RemoveError {
                id: container_id.to_owned(),
                source,
            }),
            Ok(()) | Err(_) => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests;
