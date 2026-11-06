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

//! What this suite passes testenv-core: the envid's length, label and resource-name prefix, the
//! labels that record a run's owner, what a run leaves on the host daemon, and the variable that
//! sets the sweep's stale-after window. Free of the Docker SDK and of the `integration` feature,
//! so the test that pins them runs in a plain `cargo test`.
//!
//! The Go suite labels its containers with the envid alone, so the sweep never removes what a Go
//! run left (its started label is unreadable): `make sweep-test-leaks` does, for both suites.

use testenv_core::envid::EnvIds;
use testenv_core::sweep_rules::{Labels, Resources};

/// Six random bytes (twelve hex characters), as the Go suite's `TestMain` draws them, under the
/// Go suite's `labelEnvID`, so `make sweep-test-leaks` finds what either suite left; resources are
/// named `sda-it-<envid>-<base>`. The label sits outside the `swarm-device-access.` prefix, under
/// which the daemon warns about unknown keys. Use [`EnvIds::label_filter`] to scope Docker API
/// queries to one run, or [`EnvIds::label`] on its own to find every run.
pub const ENV_IDS: EnvIds = EnvIds {
    bytes: 6,
    label: "swarm-device-access-it.envid",
    prefix: "sda-it",
};

/// The envid label, then `<hostname>:<pid>` of the test process that created the resource, when
/// the environment came up (RFC 3339), and the owner's PID namespace (`pid:[4026531836]`).
pub const LABELS: Labels = Labels {
    env_id: ENV_IDS.label,
    owner: "swarm-device-access-it.owner",
    started: "swarm-device-access-it.started",
    pid_namespace: "swarm-device-access-it.pidns",
};

/// A run leaves containers and images labelled with its envid; no volumes or networks.
pub const RESOURCES: Resources = Resources {
    remove_volumes: false,
    networks: false,
    images: true,
};

/// Overrides how old an abandoned environment must be before the sweep removes it (an hour,
/// testenv-core's `DEFAULT_STALE_AFTER`).
pub const ENV_STALE_AFTER: &str = "SDA_IT_STALE_AFTER";

/// A shell command that removes every container and image of the run `env_id` from the host.
#[must_use]
pub fn cleanup_command(env_id: &str) -> String {
    RESOURCES.cleanup_command(&ENV_IDS.cli_filter(env_id))
}

#[cfg(test)]
mod tests {
    use testenv_core::sweep_rules::Owner;

    use super::{ENV_IDS, ENV_STALE_AFTER, LABELS, cleanup_command};

    /// The names, labels and cleanup command a run had before testenv-core, which a leaked
    /// environment of an older run is still found and removed by.
    #[test]
    fn the_suite_s_names_and_labels_are_unchanged() {
        let id = ENV_IDS.generate().unwrap();
        assert_eq!(id.len(), 12, "{id}");
        assert_eq!(
            ENV_IDS.resource_name("0123456789ab", "target"),
            "sda-it-0123456789ab-target"
        );
        assert_eq!(
            ENV_IDS.label_filter("0123456789ab"),
            "swarm-device-access-it.envid=0123456789ab"
        );
        assert_eq!(
            [
                LABELS.env_id,
                LABELS.owner,
                LABELS.started,
                LABELS.pid_namespace
            ],
            [
                "swarm-device-access-it.envid",
                "swarm-device-access-it.owner",
                "swarm-device-access-it.started",
                "swarm-device-access-it.pidns",
            ]
        );
        assert_eq!(ENV_STALE_AFTER, "SDA_IT_STALE_AFTER");
        assert_eq!(
            cleanup_command("0123456789ab"),
            "docker ps -aq --filter label=swarm-device-access-it.envid=0123456789ab | xargs -r \
             docker rm -f; docker image ls -q --filter \
             label=swarm-device-access-it.envid=0123456789ab | xargs -r docker rmi -f"
        );
        let owner = Owner {
            host: "ci".to_owned(),
            pid: 42,
            pid_namespace: "pid:[4026531836]".to_owned(),
        };
        let started = "2026-10-11T12:00:00.5Z".parse().unwrap();
        let mut labels: Vec<(String, String)> = LABELS
            .run_labels("0123456789ab", &owner, started)
            .into_iter()
            .collect();
        labels.sort();
        assert_eq!(
            labels,
            [
                ("swarm-device-access-it.envid", "0123456789ab"),
                ("swarm-device-access-it.owner", "ci:42"),
                ("swarm-device-access-it.pidns", "pid:[4026531836]"),
                ("swarm-device-access-it.started", "2026-10-11T12:00:00Z"),
            ]
            .map(|(key, value)| (key.to_owned(), value.to_owned()))
        );
    }
}
