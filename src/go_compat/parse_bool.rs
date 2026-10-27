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

//! Go's `strconv.ParseBool`, which the label `swarm-device-access.enable` and the boolean flags
//! and settings are read with: exactly its twelve spellings, and its error text, which reaches the
//! logs and the startup errors.

use std::fmt;

use super::quote;

/// The error of `strconv.ParseBool`: `strconv.ParseBool: parsing "<value>": invalid syntax`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseBoolError {
    value: String,
}

impl fmt::Display for ParseBoolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "strconv.ParseBool: parsing {}: invalid syntax",
            quote::quote(&self.value)
        )
    }
}

impl std::error::Error for ParseBoolError {}

/// `strconv.ParseBool`: `1`, `t`, `T`, `TRUE`, `true` and `True` are true; `0`, `f`, `F`,
/// `FALSE`, `false` and `False` are false.
///
/// # Errors
///
/// Any other value, surrounding space and the empty string included.
pub fn parse_bool(value: &str) -> Result<bool, ParseBoolError> {
    match value {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Ok(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Ok(false),
        _ => Err(ParseBoolError {
            value: value.to_owned(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_exactly_the_go_spellings() {
        for value in ["1", "t", "T", "TRUE", "true", "True"] {
            assert_eq!(parse_bool(value), Ok(true), "{value}");
        }
        for value in ["0", "f", "F", "FALSE", "false", "False"] {
            assert_eq!(parse_bool(value), Ok(false), "{value}");
        }
        for value in ["", "yes", "on", " true", "tRUE", "2"] {
            assert!(parse_bool(value).is_err(), "{value}");
        }
        assert_eq!(
            parse_bool("y\"es").unwrap_err().to_string(),
            r#"strconv.ParseBool: parsing "y\"es": invalid syntax"#
        );
    }
}
