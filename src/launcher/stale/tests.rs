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

//! The tests of `stale_test.go`.

use std::collections::BTreeMap;

use super::super::fake::{FakeEngine, TEST_LAUNCHER_ID, result, running, start, test_run};
use super::super::{Inspected, LaunchError, Runner, StaleError, Summary};
use crate::docker::{CallContext, Error};
use crate::launcher::spec::{LABEL_LAUNCHER, LABEL_ROLE, ROLE_DAEMON};

const STALE_ID: &str = "4444444444444444444444444444444444444444444444444444444444444444";
const OWNER_ID: &str = "5555555555555555555555555555555555555555555555555555555555555555";

fn labels(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
    pairs
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect()
}

fn daemon_labels(owner: &str) -> BTreeMap<String, String> {
    labels(&[(LABEL_ROLE, ROLE_DAEMON), (LABEL_LAUNCHER, owner)])
}

/// What a case expects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Want {
    Removed,
    OwnedByRunningLauncher,
    InspectOwner,
    OwnerState,
    NoOwner,
    Foreign,
}

fn outcome(removed: &Result<(), LaunchError>) -> Option<Want> {
    match removed {
        Ok(()) => Some(Want::Removed),
        Err(LaunchError::Stale(StaleError::OwnedByRunningLauncher { .. })) => {
            Some(Want::OwnedByRunningLauncher)
        }
        Err(LaunchError::Stale(StaleError::InspectOwner { .. })) => Some(Want::InspectOwner),
        Err(LaunchError::Stale(StaleError::OwnerState { .. })) => Some(Want::OwnerState),
        Err(LaunchError::Stale(StaleError::NoOwner { .. })) => Some(Want::NoOwner),
        Err(LaunchError::Stale(StaleError::Foreign { .. })) => Some(Want::Foreign),
        Err(_) => None,
    }
}

/// One case of `TestRemoveStale`.
struct Case {
    name: &'static str,
    labels: BTreeMap<String, String>,
    owner: Option<Inspected>,
    owner_error: Option<Error>,
    want: Want,
}

fn case(name: &'static str, labels: BTreeMap<String, String>, want: Want) -> Case {
    Case {
        name,
        labels,
        owner: None,
        owner_error: None,
        want,
    }
}

fn cases() -> Vec<Case> {
    vec![
        case("launcher gone", daemon_labels(OWNER_ID), Want::Removed),
        Case {
            owner: Some(running("", false)),
            ..case("launcher exited", daemon_labels(OWNER_ID), Want::Removed)
        },
        Case {
            owner: Some(running("", true)),
            ..case(
                "launcher running",
                daemon_labels(OWNER_ID),
                Want::OwnedByRunningLauncher,
            )
        },
        Case {
            owner_error: Some(Error::deadline_exceeded()),
            ..case(
                "launcher inspect times out",
                daemon_labels(OWNER_ID),
                Want::InspectOwner,
            )
        },
        Case {
            owner: Some(Inspected::default()),
            ..case(
                "launcher state unknown",
                daemon_labels(OWNER_ID),
                Want::OwnerState,
            )
        },
        case(
            "earlier run of this launcher",
            daemon_labels(TEST_LAUNCHER_ID),
            Want::Removed,
        ),
        case(
            "no owner label",
            labels(&[(LABEL_ROLE, ROLE_DAEMON)]),
            Want::NoOwner,
        ),
        case(
            "sh wrapper daemon",
            labels(&[("org.opencontainers.image.title", "swarm-device-access")]),
            Want::Removed,
        ),
        case(
            "foreign container",
            labels(&[("org.opencontainers.image.title", "something-else")]),
            Want::Foreign,
        ),
        case("unlabelled container", BTreeMap::new(), Want::Foreign),
    ]
}

/// Go: `TestRemoveStale`. A daemon is removed only when it provably has no live owner, or was
/// left by this very launcher or by the sh wrapper; any doubt leaves it alone.
#[tokio::test(start_paused = true)]
async fn remove_stale() {
    for case in cases() {
        let fake = FakeEngine::default();
        fake.set_list(vec![Summary {
            id: STALE_ID.to_owned(),
            labels: case.labels,
        }]);
        if let Some(owner) = case.owner {
            fake.set_inspect(OWNER_ID, owner);
        }
        if let Some(error) = case.owner_error {
            fake.fail_inspect(OWNER_ID, error);
        }
        let launch = test_run(&fake).launch;
        let runner = Runner {
            engine: launch.engine,
            self_id: launch.self_id,
            timing: launch.timing,
            log: launch.log,
        };

        let removed = runner
            .remove_stale(&CallContext::new(), TEST_LAUNCHER_ID)
            .await;
        assert_eq!(outcome(&removed), Some(case.want), "{}", case.name);
        let want_removed: &[&str] = if case.want == Want::Removed {
            &[STALE_ID]
        } else {
            &[]
        };
        assert_eq!(fake.removed(), want_removed, "{}", case.name);
    }
}

/// The texts and log lines of the stale checks.
#[test]
fn stale_errors_read_as_go_words_them() {
    let cases = [
        (
            StaleError::Foreign {
                id: STALE_ID.to_owned(),
            },
            "swarm-device-access (444444444444): container is not a swarm-device-access daemon",
        ),
        (
            StaleError::NoOwner {
                id: STALE_ID.to_owned(),
            },
            "swarm-device-access (444444444444): daemon container has no launcher label",
        ),
        (
            StaleError::InspectOwner {
                owner: OWNER_ID.to_owned(),
                id: STALE_ID.to_owned(),
                source: Error::deadline_exceeded(),
            },
            "inspect launcher 555555555555 of daemon 444444444444: context deadline exceeded",
        ),
        (
            StaleError::OwnerState {
                owner: OWNER_ID.to_owned(),
                id: STALE_ID.to_owned(),
            },
            "launcher 555555555555 of daemon 444444444444: inspect reported no state",
        ),
    ];
    for (error, want) in cases {
        assert_eq!(error.to_string(), want);
    }
}

/// The removal of an orphan names it and its missing launcher.
#[tokio::test(start_paused = true)]
async fn remove_stale_logs_the_orphan() {
    let fake = FakeEngine::default();
    fake.set_list(vec![Summary {
        id: STALE_ID.to_owned(),
        labels: daemon_labels(OWNER_ID),
    }]);
    let run = test_run(&fake);
    let done = start(run.launch, &CallContext::new());
    fake.started().await;
    fake.exit(0);
    assert_eq!(result(done).await.unwrap(), 0);
    assert!(run.sink.contents().contains(
        "level=INFO removing orphaned daemon component=launcher id=444444444444 \
         launcher=555555555555"
    ));
    assert_eq!(fake.removed(), [STALE_ID]);
}

/// Go: `TestRun_RefusesLiveDaemon`. A daemon owned by a live launcher stops the whole run before
/// anything is created.
#[tokio::test(start_paused = true)]
async fn run_refuses_live_daemon() {
    let fake = FakeEngine::default();
    fake.set_list(vec![Summary {
        id: STALE_ID.to_owned(),
        labels: daemon_labels(OWNER_ID),
    }]);
    fake.set_inspect(OWNER_ID, running("", true));
    let run = test_run(&fake);

    let error = result(start(run.launch, &CallContext::new()))
        .await
        .unwrap_err();
    assert!(matches!(
        error,
        LaunchError::Stale(StaleError::OwnedByRunningLauncher { .. })
    ));
    assert!(!fake.called("create"));
    assert!(fake.removed().is_empty());
}
