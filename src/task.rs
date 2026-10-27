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

//! Awaiting the work a caller handed to a task of its own, so that dropping the caller never
//! ends the work: the processor's blocking transactions and the coordinator's holds.

use tokio::task::JoinHandle;

/// Awaits a spawned task to completion. A panic inside it is resumed here, as Go's would crash
/// the process; `None` only when the runtime shut down before it finished.
pub(crate) async fn finish<T>(task: JoinHandle<T>) -> Option<T> {
    match task.await {
        Ok(value) => Some(value),
        Err(join_error) if join_error.is_panic() => {
            std::panic::resume_unwind(join_error.into_panic())
        }
        Err(_) => None,
    }
}
