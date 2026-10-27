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

//! The identity check's negative control, new to the Rust suite: the check the runner applies to
//! the image under test, applied to the last published 1.x Go image (`SDA_GO_TEST_IMAGE`), must
//! refuse it. Without it, a check that accepted everything would pass every run. The same image is
//! the Go daemon of the cross-language upgrade test, so the requirement on it lives here: set,
//! present (`make test-integration` pulls it) and answering the identity flag as Go.

use testenv::enforce::{ENV_GO_TEST_IMAGE, go_test_image};
use testenv::host::{OP_TIMEOUT, check_go_image_identity, check_image_identity, with_timeout};

use crate::testing::{Fatal, TestHandle, TestResult, docker, suite};

/// Checks that the identity check refuses the Go image, as the Go daemon answers `-help` with its
/// usage and exit 0.
pub async fn refuses_go_image(test: TestHandle) -> TestResult {
    let image = require_go_image(&test).await?;
    let refused = check_image_identity(test.ctx(), docker(), &image, &suite().labels).await;
    match refused {
        Err(error)
            if error
                .to_string()
                .contains("is not the Rust daemon: -help exited 0") =>
        {
            test.log(format!("refused as expected: {error}"));
            Ok(())
        }
        Err(error) => Err(test.fatal(format!(
            "the Go image {image} was refused for another reason: {error}"
        ))),
        Ok(()) => Err(test.fatal(format!("the identity check accepted the Go image {image}"))),
    }
}

/// The Go image: [`ENV_GO_TEST_IMAGE`] must name it, it must be present on the host, and it must
/// answer the identity flag as the Go daemon. Anything else fails the test; none of it skips.
pub async fn require_go_image(test: &TestHandle) -> Result<String, Fatal> {
    let Some(image) = go_test_image(std::env::var(ENV_GO_TEST_IMAGE).ok().as_deref()) else {
        return Err(test.fatal(format!(
            "{ENV_GO_TEST_IMAGE} must name the 1.x Go daemon image (make test-integration sets it)"
        )));
    };
    if let Err(error) = with_timeout(test.ctx(), OP_TIMEOUT, docker().inspect_image(&image)).await {
        return Err(test.fatal(format!(
            "Go daemon image {image:?} is not on this host (make go-test-image pulls it): {error}"
        )));
    }
    check_go_image_identity(test.ctx(), docker(), &image, &suite().labels)
        .await
        .map_err(|error| test.fatal(format!("{ENV_GO_TEST_IMAGE}: {error}")))?;
    test.log(format!("identity: {image} is the Go daemon"));
    Ok(image)
}
