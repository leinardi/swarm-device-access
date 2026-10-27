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

// Adapted from swarm-scheduler-exporter/src/logger/tests.rs at d57dcc220d29,
// originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi

use jiff::Timestamp;

use super::plain_handler::{level_to_upper, qualify};
use super::*;

/// A logger writing to a memory sink, with a fixed record time of 2025-01-15T10:00:00Z.
fn logger(format: Format, level: Level, include_time: bool) -> (Logger, MemorySink) {
    let sink = MemorySink::default();
    let logger = Logger::new(
        Config {
            format,
            level,
            include_time,
        },
        Box::new(sink.clone()),
        TimeZone::UTC,
        Arc::new(|| Timestamp::new(1_736_935_200, 0).unwrap()),
    );
    (logger, sink)
}

/// Go's `newTestHandler`: plain, no time.
fn plain(level: Level) -> (Logger, MemorySink) {
    logger(Format::Plain, level, false)
}

/// Go: `TestParseLevel`, plus the U+0130 case of Go's `strings.ToLower`.
#[test]
fn parse_level_reads_the_names_in_any_case_and_defaults_to_info() {
    let cases = [
        ("debug", Level::DEBUG),
        ("DEBUG", Level::DEBUG),
        ("info", Level::INFO),
        ("INFO", Level::INFO),
        ("warn", Level::WARN),
        ("WARN", Level::WARN),
        ("warning", Level::WARN),
        ("error", Level::ERROR),
        ("fatal", Level::ERROR),
        ("panic", Level::ERROR),
        ("", Level::INFO),
        ("unknown", Level::INFO),
        ("VERBOSE", Level::INFO),
        // Go's strings.ToLower maps U+0130 to a plain i.
        ("\u{130}NFO", Level::INFO),
        ("WARN\u{130}NG", Level::WARN),
    ];
    for (input, want) in cases {
        assert_eq!(parse_level(input), want, "parse_level({input:?})");
    }
}

/// The format switch of Go's `newHandler`: json and plain in any case, text for anything else.
#[test]
fn parse_format_reads_the_names_in_any_case_and_defaults_to_text() {
    for (input, want) in [
        ("json", Format::Json),
        ("JSON", Format::Json),
        ("text", Format::Text),
        ("Text", Format::Text),
        ("plain", Format::Plain),
        ("PLAIN", Format::Plain),
        ("", Format::Text),
        ("logfmt", Format::Text),
    ] {
        assert_eq!(parse_format(input), want, "parse_format({input:?})");
    }
}

/// Go: `TestTimeStripper_IncludeTime_ReturnsNil`: with the time on, nothing is stripped, a
/// user attribute keyed `time` included.
#[test]
fn include_time_keeps_every_time() {
    let (text, sink) = logger(Format::Text, Level::INFO, true);
    text.info("m", &[Attr::string("time", "x")]);
    assert_eq!(
        sink.contents(),
        "time=2025-01-15T10:00:00.000Z level=INFO msg=m time=x\n"
    );
}

/// Go: `TestTimeStripper_ExcludeTime_DropsTimeKey`: the record time and every attribute keyed
/// `time` are dropped, groups included, with slog's quirks when that empties a group (the text
/// prefix stays, and JSON loses a comma). Expected lines are Go's output.
#[test]
fn exclude_time_drops_every_time_key() {
    let cases: [(&str, Vec<Attr>, &str, &str); 4] = [
        (
            "top",
            vec![Attr::string("time", "x"), Attr::int("k", 1)],
            "level=INFO msg=top k=1",
            r#"{"level":"INFO","msg":"top","k":1}"#,
        ),
        (
            "group",
            vec![
                Attr::group("g", vec![Attr::string("time", "x")]),
                Attr::int("k", 1),
            ],
            "level=INFO msg=group g.k=1",
            r#"{"level":"INFO","msg":"group""k":1}"#,
        ),
        (
            "only",
            vec![Attr::string("time", "x")],
            "level=INFO msg=only",
            r#"{"level":"INFO","msg":"only"}"#,
        ),
        (
            "nested",
            vec![Attr::group(
                "g",
                vec![Attr::string("time", "x"), Attr::int("a", 2)],
            )],
            "level=INFO msg=nested g.a=2",
            r#"{"level":"INFO","msg":"nested","g":{"a":2}}"#,
        ),
    ];
    for (message, attrs, want_text, want_json) in cases {
        let (text, text_sink) = logger(Format::Text, Level::INFO, false);
        text.info(message, &attrs);
        assert_eq!(
            text_sink.contents(),
            format!("{want_text}\n"),
            "text {message}"
        );
        let (json, json_sink) = logger(Format::Json, Level::INFO, false);
        json.info(message, &attrs);
        assert_eq!(
            json_sink.contents(),
            format!("{want_json}\n"),
            "json {message}"
        );
    }

    let (text, sink) = logger(Format::Text, Level::INFO, false);
    text.with_attrs(&[Attr::string("time", "w"), Attr::string("c", "d")])
        .info("with", &[]);
    text.with_group("wg")
        .info("withgroup", &[Attr::string("time", "x")]);
    text.with_group("wg").info(
        "withgroup2",
        &[Attr::string("time", "x"), Attr::int("k", 1)],
    );
    assert_eq!(
        sink.contents(),
        "level=INFO msg=with c=d\nlevel=INFO msg=withgroup\nlevel=INFO msg=withgroup2 wg.k=1\n"
    );
}

/// Go: `TestTimeStripper_ExcludeTime_PassesOtherAttrs`.
#[test]
fn exclude_time_passes_other_attrs() {
    let (text, sink) = logger(Format::Text, Level::INFO, false);
    text.info("m", &[Attr::string("service", "myapp")]);
    assert_eq!(sink.contents(), "level=INFO msg=m service=myapp\n");
}

/// Go: `TestPlainHandler_BasicLine`
#[test]
fn plain_basic_line() {
    let (logger, sink) = plain(Level::INFO);
    logger.info("hello world", &[]);
    assert_eq!(sink.contents(), "level=INFO hello world\n");
}

/// Go: `TestPlainHandler_WithAttr`
#[test]
fn plain_with_attr() {
    let (logger, sink) = plain(Level::INFO);
    logger.info("msg", &[Attr::string("k", "v")]);
    assert_eq!(sink.contents(), "level=INFO msg k=v\n");
}

/// Go: `TestPlainHandler_QuotedValue`
#[test]
fn plain_quoted_value() {
    let (logger, sink) = plain(Level::INFO);
    logger.info("msg", &[Attr::string("k", "hello world")]);
    assert_eq!(sink.contents(), "level=INFO msg k=\"hello world\"\n");
}

/// Go: `TestPlainHandler_LevelVariants`
#[test]
fn plain_level_variants() {
    for (level, want) in [
        (Level::DEBUG, "DEBUG"),
        (Level::INFO, "INFO"),
        (Level::WARN, "WARN"),
        (Level::ERROR, "ERROR"),
    ] {
        let (logger, sink) = plain(level);
        logger.log(level, "", &[]);
        assert_eq!(sink.contents(), format!("level={want}\n"), "{level}");
    }
}

/// Go: `TestPlainHandler_Enabled`
#[test]
fn plain_enabled() {
    let (logger, _) = plain(Level::WARN);
    assert!(!logger.enabled(Level::INFO));
    assert!(logger.enabled(Level::WARN));
    assert!(logger.enabled(Level::ERROR));
}

/// Go: `TestPlainHandler_WithGroup`
#[test]
fn plain_with_group() {
    let (logger, sink) = plain(Level::INFO);
    logger
        .with_group("g1")
        .info("msg", &[Attr::string("k", "v")]);
    assert_eq!(sink.contents(), "level=INFO msg g1.k=v\n");
}

/// Go: `TestPlainHandler_WithAttrs`
#[test]
fn plain_with_attrs() {
    let (logger, sink) = plain(Level::INFO);
    logger
        .with_attrs(&[Attr::string("prekey", "preval")])
        .info("msg", &[Attr::string("k", "v")]);
    assert_eq!(sink.contents(), "level=INFO msg prekey=preval k=v\n");
}

/// Go: `TestPlainHandler_WithAttrs_Empty_ReturnsSelf`
#[test]
fn plain_with_no_attrs_returns_the_same_logger() {
    let (logger, _) = plain(Level::INFO);
    let derived = logger.with_attrs(&[]);
    assert!(Arc::ptr_eq(&logger.handler, &derived.handler));
}

/// Go: `TestPlainHandler_WithGroup_EmptyName_ReturnsSelf`: a blank name is trimmed to nothing
/// and changes nothing.
#[test]
fn plain_with_blank_group_changes_nothing() {
    let (logger, sink) = plain(Level::INFO);
    let derived = logger.with_group("   ");
    assert_eq!(derived.handler, logger.handler);
    derived.info("msg", &[Attr::string("k", "v")]);
    assert_eq!(sink.contents(), "level=INFO msg k=v\n");
    assert!(Arc::ptr_eq(&logger.with_group("").handler, &logger.handler));
}

/// Go: `TestPlainHandler_OriginalUnmutatedByWithAttrs`
#[test]
fn plain_original_unchanged_by_with_attrs() {
    let (original, sink) = plain(Level::INFO);
    let derived = original.with_attrs(&[Attr::string("extra", "x")]);
    original.info("msg", &[]);
    derived.info("msg", &[]);
    assert_eq!(sink.contents(), "level=INFO msg\nlevel=INFO msg extra=x\n");
}

/// Go: `TestPlainHandler_GroupAttr`
#[test]
fn plain_group_attr() {
    let (logger, sink) = plain(Level::INFO);
    logger.info("msg", &[Attr::group("g", vec![Attr::int("n", 42)])]);
    assert_eq!(sink.contents(), "level=INFO msg g={g.n=42}\n");
}

/// Go: `TestPlainHandler_IncludeTime`
#[test]
fn plain_include_time() {
    let (logger, sink) = logger(Format::Plain, Level::INFO, true);
    logger.info("msg", &[]);
    assert_eq!(
        sink.contents(),
        "time=2025-01-15T10:00:00Z level=INFO msg\n"
    );
}

/// Go: `TestLevelToUpper`
#[test]
fn plain_level_to_upper() {
    for (level, want) in [
        (Level::DEBUG, "DEBUG"),
        (Level(-5), "DEBUG"),
        (Level::INFO, "INFO"),
        (Level::WARN, "WARN"),
        (Level::ERROR, "ERROR"),
        (Level(9), "ERROR"),
        // Not exactly info or warn: the plain format reads ERROR.
        (Level(1), "ERROR"),
    ] {
        assert_eq!(level_to_upper(level), want, "{level}");
    }
}

/// Go: `TestQualify_NoGroups`
#[test]
fn plain_qualify_without_groups() {
    assert_eq!(qualify(&[], &Attr::string("key", "val")).key, "key");
}

/// Go: `TestQualify_WithGroups`
#[test]
fn plain_qualify_with_groups() {
    let groups = ["g1".to_owned(), "g2".to_owned()];
    assert_eq!(qualify(&groups, &Attr::string("k", "v")).key, "g1.g2.k");
}

/// The plain format's 1.x rules where swarm-scheduler-exporter's escapes: the message and keys
/// raw, a newline included; a string quoted only for a space or a tab, with only `"` escaped; an
/// error, a stringer and a `[]string` raw; an attribute without a key dropped; and an empty group
/// from `with_attrs` written `key={}`. Expected lines are Go's output.
#[test]
fn plain_keeps_the_go_handler_rules() {
    let (logger, sink) = plain(Level::INFO);
    logger.info("two\nlines", &[Attr::string("a key", "x")]);
    logger.info(
        "msg",
        &[
            Attr::string("tab", "a\tb"),
            Attr::string("quote", r#"say "hi""#),
            Attr::string("bare", r#"a"b\c"#),
            Attr::string("empty", ""),
            Attr::error("err", &"dial tcp: connection refused"),
            Attr::stringer("device", &"c 226:0"),
            Attr::strings("list", ["a b", "c"]),
            Attr::nil_strings("none"),
            Attr::string("", "dropped"),
            Attr::group("", vec![Attr::string("inlined", "dropped")]),
        ],
    );
    logger
        .with_attrs(&[Attr::group("g", vec![])])
        .info("msg", &[]);
    assert_eq!(
        sink.contents(),
        "level=INFO two\nlines a key=x\n\
         level=INFO msg tab=\"a\tb\" quote=\"say \\\"hi\\\"\" bare=a\"b\\c empty= \
         err=dial tcp: connection refused device=c 226:0 list=[a b c] none=[]\n\
         level=INFO msg g={}\n"
    );
}

/// A logger writing to a memory sink with the fixture's clock and zone, behind a handle.
fn handle(format: Format) -> (LogHandle, MemorySink) {
    let (logger, sink) = logger(format, Level::INFO, false);
    (LogHandle::new(logger), sink)
}

/// A reconfigure reaches every clone of the handle, and the handles made by `with_attrs`, from
/// their next record; the clock and zone carry over.
#[test]
fn configure_swaps_the_logger_for_every_clone() {
    let (handle, first) = handle(Format::Text);
    let clone = handle.clone();
    let component = handle.with_attrs(&[Attr::string("component", "launcher")]);
    clone.info("before", &[]);
    component.info("before", &[]);

    let second = MemorySink::default();
    handle.configure_to(
        Config::parse("plain", "warn", true),
        Box::new(second.clone()),
    );
    clone.info("dropped below warn", &[]);
    clone.warn("after", &[]);
    component.warn("after", &[Attr::int("n", 1)]);

    assert_eq!(
        first.contents(),
        "level=INFO msg=before\nlevel=INFO msg=before component=launcher\n"
    );
    assert_eq!(
        second.contents(),
        "time=2025-01-15T10:00:00Z level=WARN after\n\
         time=2025-01-15T10:00:00Z level=WARN after component=launcher n=1\n"
    );
    assert!(!component.enabled(Level::INFO));
}

/// Go's `Set`: the logger a handle installs is the one every clone logs through.
#[test]
fn set_replaces_the_logger() {
    let (handle, first) = handle(Format::Text);
    let (replacement, second) = logger(Format::Json, Level::DEBUG, false);
    handle.clone().set(replacement);
    handle.debug("m", &[]);
    assert_eq!(first.contents(), "");
    assert_eq!(second.contents(), "{\"level\":\"DEBUG\",\"msg\":\"m\"}\n");
}

/// The two value kinds swarm-scheduler-exporter's logger did not have, in text and JSON: a nil
/// `[]string` (`[]` in text, `null` in JSON) and a string-typed stringer, which JSON writes through
/// encoding/json without HTML escaping. Expected lines are Go's output.
#[test]
fn nil_strings_and_stringer_match_slog() {
    let attrs = [Attr::nil_strings("none"), Attr::stringer("s", &"<a>&")];
    let (text, text_sink) = logger(Format::Text, Level::INFO, false);
    text.info("m", &attrs);
    assert_eq!(text_sink.contents(), "level=INFO msg=m none=[] s=<a>&\n");
    let (json, json_sink) = logger(Format::Json, Level::INFO, false);
    json.info(
        "m",
        &[
            Attr::nil_strings("none"),
            Attr::stringer("s", &"<a>&\u{2028}"),
        ],
    );
    assert_eq!(
        json_sink.contents(),
        "{\"level\":\"INFO\",\"msg\":\"m\",\"none\":null,\"s\":\"<a>&\\u2028\"}\n"
    );
}
