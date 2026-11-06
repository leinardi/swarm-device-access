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

use std::path::PathBuf;

use docker_client::bollard::models;
use serde_json::Value as Json;

use super::{ContainerInspect, ContainerState, ContainerSummary, Event, MountPoint};

/// The response bodies of the exchanges of `tests/testdata/docker/daemon.jsonl` (what a real
/// Engine answered the Go daemon, API 1.56) whose request URI matches `wanted`.
fn bodies(wanted: impl Fn(&str) -> bool) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/testdata/docker/daemon.jsonl");
    std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Json>(line).unwrap())
        .filter(|exchange| wanted(exchange["uri"].as_str().unwrap()))
        .map(|exchange| {
            exchange["responseBody"]
                .as_str()
                .unwrap_or_default()
                .to_owned()
        })
        .collect()
}

fn inspect(json: &str) -> ContainerInspect {
    serde_json::from_str::<models::ContainerInspectResponse>(json)
        .unwrap()
        .into()
}

fn event(json: &str) -> Event {
    serde_json::from_str::<models::EventMessage>(json)
        .unwrap()
        .into()
}

/// Every answer a real Engine gave the Go daemon decodes through bollard's models, with the
/// fields the daemon reads: the list's IDs, the inspects' state, labels, privilege and mounts, and
/// the events' actions and actors.
#[test]
fn the_captured_answers_convert_to_the_fields_the_daemon_reads() {
    let lists = bodies(|uri| uri.ends_with("/containers/json"));
    assert!(!lists.is_empty());
    let mut listed = Vec::new();
    for list in &lists {
        for summary in serde_json::from_str::<Vec<models::ContainerSummary>>(list).unwrap() {
            let summary = ContainerSummary::from(summary);
            assert_eq!(summary.id.len(), 64, "{summary:?}");
            listed.push(summary.id);
        }
    }

    let inspects = bodies(|uri| {
        uri.contains("/containers/") && uri.ends_with("/json") && !uri.ends_with("/containers/json")
    });
    assert!(!inspects.is_empty());
    for body in &inspects {
        let inspect = inspect(body);
        assert!(listed.contains(&inspect.id), "{}", inspect.id);
        let state = inspect.state.unwrap();
        assert!(state.running);
        assert!(state.pid > 0);
        assert!(state.started_at.ends_with('Z'), "{}", state.started_at);
        assert_eq!(
            inspect.config.unwrap().labels["swarm-device-access.enable"],
            "true"
        );
        assert!(!inspect.host_config.unwrap().privileged);
        assert!(inspect.mounts.contains(&MountPoint {
            kind: "bind".to_owned(),
            source: "/dev/null".to_owned(),
            destination: "/dev/null".to_owned(),
        }));
    }

    let streams = bodies(|uri| uri.contains("/events?"));
    let mut actions = Vec::new();
    for stream in &streams {
        for line in stream.lines() {
            let event = event(line);
            assert_eq!(event.kind, "container");
            assert!(listed.contains(&event.actor.id), "{}", event.actor.id);
            assert!(event.actor.attributes.contains_key("name"));
            assert_eq!(event.time, event.time_nano / 1_000_000_000);
            actions.push(event.action);
        }
    }
    assert_eq!(actions, ["start", "unpause", "die", "destroy"]);
}

/// A missing field or a `null` is the zero value, and a `null` object stays absent.
#[test]
fn null_and_missing_fields_are_the_zero_value() {
    let full = inspect(
        r#"{"Id":null,"State":{"Running":null,"Pid":null,"StartedAt":null},"Config":{"Labels":null},"Mounts":null}"#,
    );
    assert_eq!(full.id, "");
    assert_eq!(full.state.unwrap(), ContainerState::default());
    assert!(full.config.unwrap().labels.is_empty());
    assert!(full.mounts.is_empty());
    let absent = inspect(r#"{"State":null,"HostConfig":null}"#);
    assert_eq!(absent.state, None);
    assert_eq!(absent.host_config, None);
    assert_eq!(event(r#"{"Actor":null,"timeNano":null}"#), Event::default());
}

/// Unknown values of enum-like fields are data, not decode errors: the daemon's own rules see
/// them.
#[test]
fn unknown_values_of_enum_like_fields_stay_strings() {
    let unknown = event(r#"{"Type":"plugin","Action":"brand-new"}"#);
    assert_eq!(
        (unknown.kind.as_str(), unknown.action.as_str()),
        ("plugin", "brand-new")
    );
    let cluster = inspect(r#"{"Mounts":[{"Type":"cluster","Source":"/dev/x"}]}"#);
    assert_eq!(cluster.mounts[0].kind, "cluster");
}
