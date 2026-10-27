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

// Adapted from swarm-scheduler-exporter/tests/support/mod.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Helpers the integration tests share.

pub mod fake_engine;

use std::fmt::{Debug, Display};

/// `unwrap` and `unwrap_err` for the helpers of a test, which clippy's test allowance does not
/// cover: the panic names the caller's line and the error.
pub trait OrPanic<T, E> {
    /// The value, or a panic with the error.
    fn or_panic(self) -> T;
    /// The error, or a panic with the value.
    fn or_panic_err(self) -> E;
}

impl<T: Debug, E: Display> OrPanic<T, E> for Result<T, E> {
    #[track_caller]
    fn or_panic(self) -> T {
        match self {
            Ok(value) => value,
            Err(error) => panic!("unexpected error: {error}"),
        }
    }

    #[track_caller]
    fn or_panic_err(self) -> E {
        match self {
            Ok(value) => panic!("unexpected success: {value:?}"),
            Err(error) => error,
        }
    }
}
