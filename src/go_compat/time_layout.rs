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

// Adapted from swarm-scheduler-exporter/src/go_compat/time_layout.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

//! Go's two RFC 3339 layouts: `time.RFC3339Nano` (`2006-01-02T15:04:05.999999999Z07:00`) and
//! slog's text time (`2006-01-02T15:04:05.000Z07:00`), and `time.Parse` of the first.
//!
//! jiff's own formats are not Go's (swarm-scheduler-exporter's S7 spike): they write UTC as
//! `+00:00` rather than `Z`, and round a sub-minute offset (a local mean time such as Amsterdam's
//! `+00:19:32` in 1900) where Go truncates it to `+00:19`. The zone is written exactly as Go's `appendFormatRFC3339` does.

use std::fmt::Write as _;

use jiff::Timestamp;
use jiff::tz::Offset;

const NANOS_PER_MILLI: i32 = 1_000_000;

/// The instant Go's zero `time.Time` stands for, `0001-01-01T00:00:00Z`; `IsZero` compares the
/// instant only, whatever the zone.
#[must_use]
pub fn go_zero_time() -> Timestamp {
    Timestamp::new(-62_135_596_800, 0).unwrap_or(Timestamp::MIN)
}

/// Go's `Time.IsZero`.
#[must_use]
pub fn is_go_zero_time(timestamp: Timestamp) -> bool {
    timestamp == go_zero_time()
}

/// Appends `timestamp` at `offset` in `time.RFC3339Nano`: the fraction with its trailing zeros
/// trimmed, and none when it is zero.
pub fn push_rfc3339_nano(out: &mut String, timestamp: Timestamp, offset: Offset) {
    let nanos = push_date_time(out, timestamp, offset);
    if nanos != 0 {
        let fraction = format!("{nanos:09}");
        out.push('.');
        out.push_str(fraction.trim_end_matches('0'));
    }
    push_zone(out, offset);
}

/// Appends `timestamp` at `offset` in `2006-01-02T15:04:05.000Z07:00`, the fraction truncated to
/// the millisecond.
pub fn push_rfc3339_millis(out: &mut String, timestamp: Timestamp, offset: Offset) {
    let nanos = push_date_time(out, timestamp, offset);
    let _ = write!(out, ".{:03}", nanos / NANOS_PER_MILLI);
    push_zone(out, offset);
}

/// `timestamp` at `offset` in `time.RFC3339Nano`.
#[must_use]
pub fn format_rfc3339_nano(timestamp: Timestamp, offset: Offset) -> String {
    let mut out = String::with_capacity(35);
    push_rfc3339_nano(&mut out, timestamp, offset);
    out
}

/// Writes the civil date and time, to the second, and returns the nanoseconds of the second.
fn push_date_time(out: &mut String, timestamp: Timestamp, offset: Offset) -> i32 {
    let civil = offset.to_datetime(timestamp);
    let year = i32::from(civil.year());
    // Go's appendInt(year, 4): a sign, then at least four digits.
    if year < 0 {
        out.push('-');
    }
    let _ = write!(
        out,
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
        year.unsigned_abs(),
        civil.month(),
        civil.day(),
        civil.hour(),
        civil.minute(),
        civil.second()
    );
    civil.subsec_nanosecond()
}

/// Go's `Z07:00`: `Z` for a zero offset, otherwise the sign and the hours and minutes, with the
/// seconds of the offset dropped (the minutes are the offset divided by 60, truncated toward
/// zero, so an offset of -30 seconds writes `+00:00`).
fn push_zone(out: &mut String, offset: Offset) {
    let seconds = offset.seconds();
    if seconds == 0 {
        out.push('Z');
        return;
    }
    let mut minutes = seconds / 60;
    if minutes < 0 {
        out.push('-');
        minutes = -minutes;
    } else {
        out.push('+');
    }
    let _ = write!(out, "{:02}:{:02}", minutes / 60, minutes % 60);
}

/// Go's `time.Parse(time.RFC3339Nano, value)`: `None` where Go returns an error. The date and the
/// time must be in range (the day for its month), the fraction may follow a `.` or a `,` and
/// have any number of digits (only the first nine count), and the zone is `Z` or `±hh:mm` with
/// an hour up to 24 and minutes up to 60, as Go's general parser allows.
#[must_use]
pub fn parse_rfc3339_nano(value: &str) -> Option<Timestamp> {
    let bytes = value.as_bytes();
    if bytes.len() < 19
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = digits(&bytes[0..4])?;
    let month = digits(&bytes[5..7])?;
    let day = digits(&bytes[8..10])?;
    let hour = digits(&bytes[11..13]).filter(|hour| *hour <= 23)?;
    let minute = digits(&bytes[14..16]).filter(|minute| *minute <= 59)?;
    let second = digits(&bytes[17..19]).filter(|second| *second <= 59)?;
    let mut rest = &bytes[19..];
    let mut nanos = 0;
    if let [b'.' | b',', fraction @ ..] = rest {
        let count = fraction
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        if count == 0 {
            return None;
        }
        for (index, byte) in fraction[..count.min(9)].iter().enumerate() {
            let place = 10_i32.pow(u32::try_from(8 - index).ok()?);
            nanos += i32::from(byte - b'0') * place;
        }
        rest = &fraction[count..];
    }
    let offset_seconds = match rest {
        [b'Z'] => 0,
        [sign @ (b'+' | b'-'), hours @ .., b':', _, _] if hours.len() == 2 => {
            let hours = digits(hours).filter(|hours| *hours <= 24)?;
            let minutes = digits(&rest[4..6]).filter(|minutes| *minutes <= 60)?;
            let seconds = (hours * 60 + minutes) * 60;
            if *sign == b'-' { -seconds } else { seconds }
        }
        _ => return None,
    };
    let date = jiff::civil::Date::new(
        i16::try_from(year).ok()?,
        i8::try_from(month).ok()?,
        i8::try_from(day).ok()?,
    )
    .ok()?;
    let time = jiff::civil::Time::new(
        i8::try_from(hour).ok()?,
        i8::try_from(minute).ok()?,
        i8::try_from(second).ok()?,
        nanos,
    )
    .ok()?;
    let offset = Offset::from_seconds(offset_seconds).ok()?;
    offset.to_timestamp(date.to_datetime(time)).ok()
}

/// The value of ASCII decimal digits; `None` for any other byte.
fn digits(bytes: &[u8]) -> Option<i32> {
    bytes.iter().try_fold(0_i32, |value, byte| {
        byte.is_ascii_digit()
            .then(|| value * 10 + i32::from(byte - b'0'))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn offset(seconds: i32) -> Offset {
        Offset::from_seconds(seconds).unwrap()
    }

    fn at(seconds: i64, nanos: i32) -> Timestamp {
        Timestamp::new(seconds, nanos).unwrap()
    }

    fn millis(timestamp: Timestamp, offset: Offset) -> String {
        let mut out = String::new();
        push_rfc3339_millis(&mut out, timestamp, offset);
        out
    }

    /// Go's output for the golden record time, 2026-10-25T14:03:04.567891234+01:00.
    #[test]
    fn layouts_match_go_on_the_record_time() {
        let record = at(1_792_933_384, 567_891_234);
        assert_eq!(
            format_rfc3339_nano(record, offset(3600)),
            "2026-10-25T14:03:04.567891234+01:00"
        );
        assert_eq!(
            millis(record, offset(3600)),
            "2026-10-25T14:03:04.567+01:00"
        );
    }

    /// The zone and fraction rules of swarm-scheduler-exporter's S7 corpus.
    #[test]
    fn layouts_follow_go_zone_and_fraction_rules() {
        let utc = at(1_767_323_045, 120_000_000); // 2026-01-02T03:04:05.12Z
        assert_eq!(
            format_rfc3339_nano(utc, Offset::UTC),
            "2026-01-02T03:04:05.12Z"
        );
        assert_eq!(millis(utc, Offset::UTC), "2026-01-02T03:04:05.120Z");
        let whole = at(1_767_323_045, 0);
        assert_eq!(
            format_rfc3339_nano(whole, Offset::UTC),
            "2026-01-02T03:04:05Z"
        );
        // 999.5ms truncates to .999, never rounds up.
        assert_eq!(
            millis(at(1_767_323_045, 999_500_000), Offset::UTC),
            "2026-01-02T03:04:05.999Z"
        );
        // -05:30, and a local mean time offset whose seconds Go drops.
        assert_eq!(
            format_rfc3339_nano(at(946_704_599, 999_999_999), offset(-(5 * 3600 + 30 * 60))),
            "1999-12-31T23:59:59.999999999-05:30"
        );
        assert_eq!(
            format_rfc3339_nano(at(-2_208_988_800, 0), offset(1172)),
            "1900-01-01T00:19:32+00:19"
        );
        assert_eq!(
            format_rfc3339_nano(at(0, 0), offset(-30)),
            "1969-12-31T23:59:30+00:00"
        );
    }

    #[test]
    fn go_zero_time_formats_as_year_one() {
        assert!(is_go_zero_time(go_zero_time()));
        assert_eq!(
            format_rfc3339_nano(go_zero_time(), Offset::UTC),
            "0001-01-01T00:00:00Z"
        );
        assert_eq!(
            millis(go_zero_time(), Offset::UTC),
            "0001-01-01T00:00:00.000Z"
        );
    }

    /// Every value and answer is a Go program's `time.Parse(time.RFC3339Nano, value)`.
    #[test]
    fn parse_rfc3339_nano_matches_go() {
        for (value, want) in [
            ("2026-09-27T10:00:00.000000000Z", Some((1_790_503_200, 0))),
            ("2026-09-27T10:00:00Z", Some((1_790_503_200, 0))),
            ("0001-01-01T00:00:00Z", Some((-62_135_596_800, 0))),
            ("0000-01-01T00:00:00Z", Some((-62_167_219_200, 0))),
            (
                "2026-09-27T10:00:00.5+02:00",
                Some((1_790_496_000, 500_000_000)),
            ),
            (
                "2026-09-27T10:00:00.123456789123-05:30",
                Some((1_790_523_000, 123_456_789)),
            ),
            (
                "2026-09-27T10:00:00.12345Z",
                Some((1_790_503_200, 123_450_000)),
            ),
            ("2024-02-29T23:59:59Z", Some((1_709_251_199, 0))),
            ("2026-09-27T10:00:00,5Z", Some((1_790_503_200, 500_000_000))),
            ("2026-09-27T10:00:00+24:00", Some((1_790_416_800, 0))),
            ("2026-09-27T10:00:00-24:00", Some((1_790_589_600, 0))),
            ("2026-09-27T10:00:00+00:60", Some((1_790_499_600, 0))),
            ("2026-09-27T10:00:00+00:59", Some((1_790_499_660, 0))),
            ("2023-02-29T23:59:59Z", None),
            ("2026-09-27T24:00:00Z", None),
            ("2026-09-27T10:00:60Z", None),
            ("2026-09-27t10:00:00Z", None),
            ("2026-09-27 10:00:00Z", None),
            ("2026-09-27T10:00:00z", None),
            ("2026-09-27T10:00:00.Z", None),
            ("2026-09-27T10:00:00,Z", None),
            ("2026-09-27T10:00:00.1,2Z", None),
            ("2026-09-27T10:00:00+0200", None),
            ("2026-09-27T10:00:00+25:00", None),
            ("2026-09-27T10:00:00+02:00x", None),
            ("2026-09-27T10:00:00", None),
            ("", None),
            ("+2026-09-27T10:00:00Z", None),
            ("26-09-27T10:00:00Z", None),
            ("2026-9-27T10:00:00Z", None),
        ] {
            let got = parse_rfc3339_nano(value)
                .map(|parsed| (parsed.as_second(), parsed.subsec_nanosecond()));
            assert_eq!(got, want, "{value}");
        }
    }
}
