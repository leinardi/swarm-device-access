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

use serde_json::Value as Json;

use super::{ContainerInspect, ContainerSummary, Event, MountPoint, decode_first, decode_list};

/// The response bodies of `fixture`'s exchanges whose request URI matches `wanted`, from the Go
/// captures under `tests/testdata/engine_wire`.
fn bodies(fixture: &str, wanted: impl Fn(&str) -> bool) -> Vec<String> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/engine_wire/api-1.56")
        .join(fixture);
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

/// Every answer the Go daemon got decodes, with the fields it reads: the list's IDs, the
/// inspects' state, labels, privilege and mounts, and the events' actions and actors.
#[test]
fn the_captured_answers_decode_the_fields_the_daemon_reads() {
    let lists = bodies("daemon.jsonl", |uri| uri.ends_with("/containers/json"));
    assert!(!lists.is_empty());
    let mut listed = Vec::new();
    for list in &lists {
        for summary in decode_list::<ContainerSummary>(list.as_bytes()).unwrap() {
            assert_eq!(summary.id.len(), 64, "{summary:?}");
            listed.push(summary.id);
        }
    }

    let inspects = bodies("daemon.jsonl", |uri| {
        uri.contains("/containers/") && uri.ends_with("/json") && !uri.ends_with("/containers/json")
    });
    assert!(!inspects.is_empty());
    for body in &inspects {
        let inspect: ContainerInspect = decode_first(body.as_bytes()).unwrap();
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

    let streams = bodies("daemon.jsonl", |uri| uri.contains("/events?"));
    let mut actions = Vec::new();
    for stream in &streams {
        for line in stream.lines() {
            let event: Event = decode_first(line.as_bytes()).unwrap();
            assert_eq!(event.kind, "container");
            assert!(listed.contains(&event.actor.id), "{}", event.actor.id);
            assert!(event.actor.attributes.contains_key("name"));
            assert_eq!(event.time, event.time_nano / 1_000_000_000);
            actions.push(event.action);
        }
    }
    assert_eq!(actions, ["start", "unpause", "die", "destroy"]);
}

/// Go decodes a missing field or a `null` into the zero value, and leaves a `null` pointer nil.
#[test]
fn null_and_missing_fields_are_the_zero_value() {
    let inspect: ContainerInspect = decode_first(
        br#"{"Id":null,"State":{"Running":null,"Pid":null,"StartedAt":null},"Config":{"Labels":null},"Mounts":null}"#,
    )
    .unwrap();
    assert_eq!(inspect.id, "");
    assert_eq!(inspect.state.unwrap(), super::ContainerState::default());
    assert!(inspect.config.unwrap().labels.is_empty());
    assert!(inspect.mounts.is_empty());
    // A null pointer stays absent.
    let inspect: ContainerInspect = decode_first(br#"{"State":null,"HostConfig":null}"#).unwrap();
    assert_eq!(inspect.state, None);
    assert_eq!(inspect.host_config, None);
    let event: Event = decode_first(br#"{"Actor":null,"timeNano":null}"#).unwrap();
    assert_eq!(event, Event::default());
}

#[test]
fn null_lists_and_elements_are_empty_and_zero() {
    assert_eq!(decode_list::<ContainerSummary>(b"null").unwrap(), []);
    assert_eq!(decode_list::<ContainerSummary>(b" []").unwrap(), []);
    let containers = decode_list::<ContainerSummary>(br#"[null,{"Id":"c"}]"#).unwrap();
    assert_eq!(containers[0], ContainerSummary::default());
    assert_eq!(containers[1].id, "c");
}

/// Unknown values of enum-like fields are data, not decode errors: the daemon's own rules see
/// them.
#[test]
fn unknown_enum_values_decode_as_strings() {
    let event: Event = decode_first(br#"{"Type":"plugin","Action":"brand-new"}"#).unwrap();
    assert_eq!(
        (event.kind.as_str(), event.action.as_str()),
        ("plugin", "brand-new")
    );
    let inspect: ContainerInspect =
        decode_first(br#"{"Mounts":[{"Type":"cluster","Source":"/dev/x"}]}"#).unwrap();
    assert_eq!(inspect.mounts[0].kind, "cluster");
}

/// Go's `json.NewDecoder(body).Decode` reads the first value and ignores the rest.
#[test]
fn decode_first_reads_like_a_go_decoder() {
    assert_eq!(
        decode_first::<Vec<ContainerSummary>>(b"  [] trailing garbage").unwrap(),
        []
    );
    assert_eq!(decode_first::<ContainerSummary>(b"").unwrap_err(), "EOF");
    assert_eq!(
        decode_first::<ContainerSummary>(b" \n\t").unwrap_err(),
        "EOF"
    );
    assert_eq!(
        decode_first::<ContainerSummary>(br#"{"Id":"n"#).unwrap_err(),
        "unexpected EOF"
    );
    assert!(decode_first::<ContainerSummary>(br#"{"Id":5}"#).is_err());
    assert!(decode_first::<ContainerInspect>(br#"{"State":{"Pid":"one"}}"#).is_err());
}
