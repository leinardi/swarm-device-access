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

// Adapted from swarm-scheduler-exporter/src/docker/api/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

use std::time::Duration;

use super::*;

#[test]
fn filters_marshal_as_go_sorted_and_escaped() {
    let filters = Filters::new().add("type", &["service", "node"]);
    assert_eq!(
        filters.to_json(),
        r#"{"type":{"node":true,"service":true}}"#
    );
    let filters = Filters::new()
        .add("type", &["service"])
        .add("label", &["a<b&c>\"d\\\n\u{1}\u{2028}é"])
        .add("type", &["node"]);
    assert_eq!(
        filters.to_json(),
        r#"{"label":{"a\u003cb\u0026c\u003e\"d\\\n\u0001\u2028é":true},"type":{"node":true,"service":true}}"#
    );
    assert!(Filters::new().is_empty());
    assert_eq!(Filters::new().to_json(), "{}");
}

#[test]
fn queries_encode_as_url_values() {
    let filters = Filters::new().add("type", &["service", "node"]);
    let mut pairs = [
        (
            "since",
            since_parameter("2026-09-20T10:00:00.5Z".parse().unwrap()),
        ),
        ("filters", filters.to_json()),
    ];
    assert_eq!(
        encode_query(&mut pairs),
        "filters=%7B%22type%22%3A%7B%22node%22%3Atrue%2C%22service%22%3Atrue%7D%7D\
         &since=1789898400.500000000"
    );
    assert_eq!(encode_query(&mut [("all", "1".to_owned())]), "all=1");
    assert_eq!(
        encode_query(&mut [("k y", "a b+c/é~".to_owned())]),
        "k+y=a+b%2Bc%2F%C3%A9~"
    );
}

#[test]
fn since_is_seconds_and_nine_digits_of_nanoseconds() {
    let since = |text: &str| since_parameter(text.parse().unwrap());
    assert_eq!(since("2026-09-20T10:00:00Z"), "1789898400.000000000");
    assert_eq!(
        since("2026-09-20T10:00:00.000000001Z"),
        "1789898400.000000001"
    );
    // The reconnect case of the engine-wire test: 500ms before an event at …300.2.
    assert_eq!(
        since_parameter(jiff::Timestamp::from_nanosecond(1_789_902_299_700_000_000).unwrap()),
        "1789902299.700000000"
    );
    // Go's Unix() rounds down, and Nanosecond() is never negative.
    assert_eq!(since("1969-12-31T23:59:59.25Z"), "-1.250000000");
}

#[test]
fn an_empty_id_is_refused_before_any_request() {
    let error = trim_id("container", " \t").unwrap_err();
    assert_eq!(
        error.to_string(),
        "invalid container name or ID: value is empty"
    );
    assert_eq!(error.kind(), crate::docker::ErrorKind::InvalidParameter);
    assert_eq!(trim_id("container", " c-1 ").unwrap(), "c-1");
}

/// The Go daemon's subscription filter, as the captured request carries it.
#[test]
fn the_daemon_filter_encodes_as_the_go_daemon_sent_it() {
    let filters = Filters::new().add("event", &["start", "unpause", "die", "destroy"]);
    assert_eq!(
        encode_query(&mut [("filters", filters.to_json())]),
        "filters=%7B%22event%22%3A%7B%22destroy%22%3Atrue%2C%22die%22%3Atrue%2C%22start%22%3Atrue\
         %2C%22unpause%22%3Atrue%7D%7D"
    );
}

#[tokio::test(start_paused = true)]
async fn a_timeout_ends_a_call_with_deadline_exceeded() {
    let timeout = Duration::from_secs(10);
    let start = tokio::time::Instant::now();
    let error = with_timeout(timeout, std::future::pending::<Result<(), Error>>())
        .await
        .unwrap_err();
    assert_eq!(start.elapsed(), timeout);
    assert_eq!(error.kind(), crate::docker::ErrorKind::DeadlineExceeded);
    assert_eq!(error.to_string(), "context deadline exceeded");

    assert_eq!(
        with_timeout(timeout, async { Ok::<_, Error>(7) })
            .await
            .unwrap(),
        7
    );
}
