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

// Adapted from swarm-scheduler-exporter/src/go_compat/mod.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Ports of the Go standard library's formatting and parsing that the daemon's behaviour depends
//! on byte for byte: the log lines, the metrics exposition and, later in the port, the flags. Each
//! one is a port rather than a Rust equivalent, because the Rust equivalents differ in the
//! details (float ties, `+00:00` for UTC, Unicode versions, POSIX `TZ` strings).
//!
//! - [`float`] ports `strconv.FormatFloat(f, 'g', -1, 64)` and encoding/json's float layout;
//! - [`duration`] ports `time.Duration.String` and `time.ParseDuration`;
//! - [`quote`] ports `strconv.Quote`, with Go's Unicode 15 `IsPrint` and `IsSpace`;
//! - [`time_layout`] ports the `time.RFC3339Nano` and `2006-01-02T15:04:05.000Z07:00` layouts;
//! - [`local_zone`] ports how Go's `time.Local` is resolved from `TZ` and the zoneinfo files;
//! - [`strings`] ports `strings.EqualFold`, `strings.ToLower` and `strings.TrimSpace`, with Go's
//!   Unicode 15 case tables;
//! - [`parse_bool`] ports `strconv.ParseBool` (new here);
//! - [`errno`] gives an `io::Error` Go's `syscall.Errno` text;
//! - [`filepath`] ports `filepath.Match` and `filepath.Clean` (new here);
//! - [`parse_float`] ports `strconv.ParseFloat`, for the q values of an `Accept` header;
//! - [`sort`] ports `sort.Sort`, whose order goautoneg's comparison depends on.
//!
//! Only the modules the ported code uses are here; swarm-scheduler-exporter's others join when a
//! phase needs them (`docs/rust-port.md`, the reuse ledger).

pub mod duration;
pub mod errno;
pub mod filepath;
pub mod float;
pub mod local_zone;
pub mod parse_bool;
pub mod parse_float;
pub mod quote;
pub mod sort;
pub mod strings;
pub mod time_layout;
mod unicode_case;
mod unicode_tables;
