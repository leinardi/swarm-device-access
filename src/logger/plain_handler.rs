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

// Adapted from swarm-scheduler-exporter/src/logger/plain_handler.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! This project's plain format (`internal/logger/plain_handler.go`):
//!
//! ```text
//! level=INFO hello world k=v foo=bar
//! ```
//!
//! with `time=<RFC3339Nano>` first when the time is logged.
//!
//! swarm-scheduler-exporter's plain handler escapes and quotes so that a record is always one
//! line; this project's Go handler does not, and its output is the 1.x contract, so its rules are
//! kept, checked against the Go goldens in `tests/testdata/logger/plain_*.log`:
//!
//! - the message and every key are written raw, a newline included;
//! - a string value is quoted only when it holds a space or a tab, and then only `"` is escaped;
//! - an error, a stringer, a `[]string` and nil are written raw, as `fmt.Fprint` does;
//! - a level that is not exactly info, warn or error reads `ERROR` above debug;
//! - attributes added by `with_attrs` are qualified with every group, those opened after them
//!   included; an attribute whose key is empty after qualification is dropped, a group included;
//!   and a group with no attributes is written `key={}`.

use crate::go_compat::{duration, float, time_layout};

use super::Level;
use super::value::{Attr, Time, Value};

/// The plain handler: the attributes from `with_attrs` and the groups from `with_group`.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct PlainHandler {
    prefix_attrs: Vec<Attr>,
    groups: Vec<String>,
}

impl PlainHandler {
    /// `WithAttrs`, copy on write; no attributes change nothing.
    pub(super) fn with_attrs(&self, attrs: &[Attr]) -> Self {
        let mut next = self.clone();
        next.prefix_attrs.extend_from_slice(attrs);
        next
    }

    /// `WithGroup`: the name is trimmed, and a blank one changes nothing.
    pub(super) fn with_group(&self, name: &str) -> Self {
        let name = name.trim();
        let mut next = self.clone();
        if !name.is_empty() {
            next.groups.push(name.to_owned());
        }
        next
    }

    /// `Handle`: one record, newline included. `time` is `None` when the time is not logged, or
    /// is Go's zero time.
    pub(super) fn format(
        &self,
        time: Option<Time>,
        level: Level,
        message: &str,
        attrs: &[Attr],
    ) -> String {
        let mut buffer = String::with_capacity(128);
        if let Some(time) = time {
            buffer.push_str("time=");
            time_layout::push_rfc3339_nano(&mut buffer, time.timestamp, time.offset);
            buffer.push(' ');
        }
        buffer.push_str("level=");
        buffer.push_str(level_to_upper(level));
        if !message.is_empty() {
            buffer.push(' ');
            buffer.push_str(message);
        }
        for attr in self.prefix_attrs.iter().chain(attrs) {
            write_attr_kv(&mut buffer, &qualify(&self.groups, attr));
        }
        buffer.push('\n');
        buffer
    }
}

/// `levelToUpper`.
pub(super) fn level_to_upper(level: Level) -> &'static str {
    if level <= Level::DEBUG {
        "DEBUG"
    } else if level == Level::INFO {
        "INFO"
    } else if level == Level::WARN {
        "WARN"
    } else {
        "ERROR"
    }
}

/// `qualify`: the key under the nested group prefixes (`g1.g2.key`).
pub(super) fn qualify(groups: &[String], attr: &Attr) -> Attr {
    if groups.is_empty() {
        return attr.clone();
    }
    let mut key = groups.join(".");
    if !attr.key.is_empty() {
        key.push('.');
        key.push_str(&attr.key);
    }
    Attr {
        key,
        value: attr.value.clone(),
    }
}

/// `writeAttrKV`: ` key=value`, or nothing for an attribute without a key (the empty `Attr{}`
/// included).
fn write_attr_kv(buffer: &mut String, attr: &Attr) {
    if attr.key.is_empty() {
        return;
    }
    buffer.push(' ');
    write_kv(buffer, &attr.key, &attr.value);
}

/// `writeKV` without the leading space: the key raw, `=`, then the value.
fn write_kv(buffer: &mut String, key: &str, value: &Value) {
    buffer.push_str(key);
    buffer.push('=');
    match value {
        Value::Group(children) => write_group_braced(buffer, key, children),
        Value::Str(text) => write_string_value(buffer, text),
        // fmt.Fprint of an error, a stringer, a []string (nil or not) and nil.
        Value::Error(text) | Value::Stringer(text) => buffer.push_str(text),
        Value::Strings(items) => {
            buffer.push('[');
            buffer.push_str(&items.join(" "));
            buffer.push(']');
        }
        Value::NilStrings => buffer.push_str("[]"),
        Value::Nil => buffer.push_str("<nil>"),
        Value::Int(number) => buffer.push_str(&number.to_string()),
        Value::Uint(number) => buffer.push_str(&number.to_string()),
        Value::Float(number) => float::push_g(buffer, *number),
        Value::Bool(flag) => buffer.push_str(if *flag { "true" } else { "false" }),
        Value::Time(time) => {
            if time_layout::is_go_zero_time(time.timestamp) {
                buffer.push('0');
            } else {
                time_layout::push_rfc3339_nano(buffer, time.timestamp, time.offset);
            }
        }
        Value::Duration(nanos) => duration::push(buffer, *nanos),
    }
}

/// `writeGroupBraced`: `key={key.a=1 key.b="two"}`, the children's keys under the parent's, and
/// `key={}` for a group without attributes. A child's qualified key is never empty, so every
/// child is written.
fn write_group_braced(buffer: &mut String, parent_key: &str, children: &[Attr]) {
    buffer.push('{');
    for (index, child) in children.iter().enumerate() {
        if index > 0 {
            buffer.push(' ');
        }
        let mut key = parent_key.to_owned();
        if !child.key.is_empty() {
            key.push('.');
            key.push_str(&child.key);
        }
        write_kv(buffer, &key, &child.value);
    }
    buffer.push('}');
}

/// `writeScalarValue` for a string: quoted when it holds a space or a tab, with only `"`
/// escaped; raw otherwise, the empty string included.
fn write_string_value(buffer: &mut String, text: &str) {
    if text.contains([' ', '\t']) {
        buffer.push('"');
        buffer.push_str(&text.replace('"', "\\\""));
        buffer.push('"');
    } else {
        buffer.push_str(text);
    }
}
