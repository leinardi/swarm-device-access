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

// Adapted from swarm-scheduler-exporter/src/test_support/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The shared helpers' own tests.

use super::*;
use crate::logger::Attr;

#[test]
fn capture_logs_records_what_is_logged_at_the_level() {
    let (log, sink) = capture_logs(Level::INFO);
    log.debug("hidden", &[]);
    log.warn("shown", &[Attr::string("key", "value")]);
    assert_eq!(sink.contents(), "level=WARN shown key=value\n");
}

#[tokio::test(start_paused = true)]
async fn eventually_returns_once_the_condition_holds() {
    let start = tokio::time::Instant::now();
    eventually("a second", Duration::from_secs(5), || {
        start.elapsed() >= Duration::from_secs(1)
    })
    .await;
    assert!(start.elapsed() < Duration::from_secs(2));
}

#[tokio::test(start_paused = true)]
#[should_panic(expected = "timed out after 1s waiting for nothing")]
async fn eventually_fails_once_the_wait_is_over() {
    eventually("nothing", Duration::from_secs(1), || false).await;
}
