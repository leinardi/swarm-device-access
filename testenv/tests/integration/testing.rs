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
// Adapted from swarm-scheduler-exporter/testenv/tests/integration/testing.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! The suite's shared state: the host Docker client, the binary and image under test, the run's
//! labels and the suite context. What the Go suite took from the testing package (the test
//! handle, `Fatal`, `Outcome` and the runner) is testenv-core's `testing`.

use std::collections::HashMap;
use std::path::PathBuf;

use bollard::Docker;
use testenv_core::context::Context;
use testenv_core::testing;
pub use testenv_core::testing::{Fatal, OrFatal, TestHandle, TestResult};
use tokio::sync::OnceCell;

/// What every test shares: set up once, before the first test.
pub struct Suite {
    /// The host daemon's client.
    pub docker: Docker,
    /// The Rust daemon binary under test (`SDA_RUST_TEST_BINARY`), run as a host process.
    pub binary: PathBuf,
    /// The Rust daemon image under test (`SDA_RUST_TEST_IMAGE`), which passed the identity check.
    pub image: String,
    /// The run's envid.
    pub env_id: String,
    /// The labels every container and image of the run carries, the envid among them.
    pub labels: HashMap<String, String>,
    /// Ends at the suite deadline or on SIGINT or SIGTERM; every test's context derives from it.
    pub ctx: Context,
    /// The one pull of the test image per run, Go's `pullTestImageOnce`, and its outcome.
    pub test_image_pull: OnceCell<Result<(), String>>,
}

static SUITE: testing::Suite<Suite> = testing::Suite::new("the suite is used before it is set up");

/// Installs the suite; the runner calls it once, after the identity check.
pub fn install(suite: Suite) {
    SUITE.install(suite);
}

/// The suite.
pub fn suite() -> &'static Suite {
    SUITE.get()
}

/// The host daemon's client.
pub fn docker() -> &'static Docker {
    &suite().docker
}
