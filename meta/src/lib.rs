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
// Adapted from swarm-scheduler-exporter/meta/src/lib.rs at d57dcc220d29, originally MIT-licensed,
// Copyright (c) 2026 Roberto Leinardi

//! Repository meta-tests: checks on the repository itself rather than on the daemon. The tests
//! live under `tests/`; this library holds what they share: the repository's files. Every
//! meta-test fails rather than skips when something it needs is missing, and carries a negative
//! control: a case proving the rule fails on the input it exists to stop.

pub mod repo;
