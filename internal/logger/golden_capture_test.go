//go:build goldencapture

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

package logger

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"log/slog"
	"strconv"
	"testing"
	"time"

	"github.com/leinardi/swarm-device-access/internal/goldencapture"
)

// goldenAttr is one attribute of the logger corpus, in a form the Rust golden harness can read
// back: a kind, a key and a value whose JSON type follows the kind.
//
//	string, stringer, error  Value is the text (a stringer's String, an error's Error)
//	int, uint                Value is the number, as a decimal string (no float rounding)
//	float                    Value is strconv.FormatFloat(f, 'g', -1, 64)
//	bool                     Value is true or false
//	duration                 Value is the nanoseconds, as a decimal string
//	time                     Value is the instant in RFC 3339 with nanoseconds and its offset
//	strings                  Value is the list, or null for a nil slice
//	nil                      no Value: slog.Any(key, nil)
//	group                    Attrs holds the members: slog.Group(key, ...)
type goldenAttr struct {
	Kind  string       `json:"kind"`
	Key   string       `json:"key"`
	Value any          `json:"value,omitempty"`
	Attrs []goldenAttr `json:"attrs,omitempty"`
}

// goldenStep derives a handler from the one before it: WithAttrs when Attrs is set, WithGroup
// when Group is.
type goldenStep struct {
	Attrs []goldenAttr `json:"attrs,omitempty"`
	Group string       `json:"group,omitempty"`
}

// goldenRecord is one record of the corpus, handled at goldenTime.
type goldenRecord struct {
	Level   int          `json:"level"`
	Message string       `json:"message"`
	With    []goldenStep `json:"with,omitempty"`
	Attrs   []goldenAttr `json:"attrs,omitempty"`
}

// goldenLevel is how parseLevel reads one --log-level spelling.
type goldenLevel struct {
	Input string `json:"input"`
	Level int    `json:"level"`
}

// goldenCorpus is everything the logger golden captures besides the log files themselves.
type goldenCorpus struct {
	Time    string         `json:"time"`
	Levels  []goldenLevel  `json:"levels"`
	Records []goldenRecord `json:"records"`
}

// goldenError is an error with a fixed text, as the daemon's wrapped errors read when logged.
type goldenError string

func (e goldenError) Error() string { return string(e) }

// goldenStringer is a value logged through its String method.
type goldenStringer string

func (s goldenStringer) String() string { return string(s) }

func str(key, value string) goldenAttr { return goldenAttr{Kind: "string", Key: key, Value: value} }

func integer(key string, value int64) goldenAttr {
	return goldenAttr{Kind: "int", Key: key, Value: strconv.FormatInt(value, 10)}
}

func unsigned(key string, value uint64) goldenAttr {
	return goldenAttr{Kind: "uint", Key: key, Value: strconv.FormatUint(value, 10)}
}

func float(key string, value float64) goldenAttr {
	return goldenAttr{Kind: "float", Key: key, Value: strconv.FormatFloat(value, 'g', -1, 64)}
}

func boolean(key string, value bool) goldenAttr {
	return goldenAttr{Kind: "bool", Key: key, Value: value}
}

func duration(key string, value time.Duration) goldenAttr {
	return goldenAttr{Kind: "duration", Key: key, Value: strconv.FormatInt(int64(value), 10)}
}

func instant(key string, value time.Time) goldenAttr {
	return goldenAttr{Kind: "time", Key: key, Value: value.Format(time.RFC3339Nano)}
}

// failure is an error under the key the daemon always logs errors with.
func failure(err error) goldenAttr {
	return goldenAttr{Kind: "error", Key: "err", Value: err.Error()}
}

func stringer(key, value string) goldenAttr {
	return goldenAttr{Kind: "stringer", Key: key, Value: value}
}

func strs(key string, value []string) goldenAttr {
	return goldenAttr{Kind: "strings", Key: key, Value: value}
}

func nothing(key string) goldenAttr { return goldenAttr{Kind: "nil", Key: key} }

func group(key string, attrs ...goldenAttr) goldenAttr {
	return goldenAttr{Kind: "group", Key: key, Attrs: attrs}
}

// goldenTime is the instant every record is handled at, in a zone with an offset, so the time
// layouts show both the fraction and the offset.
func goldenTime() time.Time {
	return time.Date(2026, 10, 25, 14, 3, 4, 567891234, time.FixedZone("CET", 3600))
}

// goldenRecords is the corpus: every attribute kind the daemon logs, the values that need quoting
// or escaping in each format, the levels, and attributes and groups added through With.
func goldenRecords() []goldenRecord {
	quoting := []string{
		"plain",
		"with space",
		`say "hi"`,
		"k=v",
		`C:\path\x`,
		"line1\nline2",
		"a\tb",
		"a\rb",
		"a\x01b",
		"",
		"ok 🚀",
		string([]rune{0x65e5, 0x672c, 0x8a9e}),
		"html <a>&amp;",
		"trailing ",
		" leading",
		"-dash",
		"{brace}",
		"[bracket]",
		"/dev/dri/card0",
		"/dev/dri/by-path/pci-0000:00:02.0-card",
		"c 226:0 rwm",
		"b 8:*",
		"a=b c",
		"nbsp\u00a0x",
		"ls\u2028x",
	}

	records := make([]goldenRecord, 0, len(quoting)+32)

	for _, value := range quoting {
		records = append(records, goldenRecord{
			Level: int(
				slog.LevelInfo,
			),
			Message: "string value",
			Attrs:   []goldenAttr{str("value", value)},
		})
	}

	joined := errors.Join(
		goldenError(
			"device identity could not be established: /dev/dri: openat2 dri: permission denied",
		),
		fmt.Errorf(
			"device mount enumeration incomplete: %w",
			goldenError("/dev/dri/by-path: permission denied"),
		),
	)

	records = append(records,
		goldenRecord{Level: int(slog.LevelDebug), Message: "debug message"},
		goldenRecord{Level: int(slog.LevelInfo), Message: "info message"},
		goldenRecord{Level: int(slog.LevelWarn), Message: "warn message"},
		goldenRecord{Level: int(slog.LevelError), Message: "error message"},
		goldenRecord{Level: int(slog.LevelDebug + 1), Message: "between debug and info"},
		goldenRecord{Level: int(slog.LevelInfo + 2), Message: "between info and warn"},
		goldenRecord{Level: int(slog.LevelWarn + 1), Message: "between warn and error"},
		goldenRecord{Level: int(slog.LevelError + 4), Message: "above error"},
		goldenRecord{Level: int(slog.LevelInfo), Message: ""},
		goldenRecord{Level: int(slog.LevelInfo), Message: `message with "quotes" and = sign`},
		goldenRecord{Level: int(slog.LevelInfo), Message: "message\nwith a newline"},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "container processed",
			Attrs: []goldenAttr{
				str("id", "abc"), integer("pid", 4242), integer("devices_granted", 1),
				integer("skipped", 0), integer("errors", 0), boolean("dry_run", false),
			},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "integers",
			Attrs: []goldenAttr{
				integer("zero", 0), integer("negative", -1), integer("min", -9223372036854775808),
				integer("max", 9223372036854775807), unsigned("major", 226),
				unsigned("umax", 18446744073709551615),
			},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "floats",
			Attrs: []goldenAttr{
				float("zero", 0), float("half", 0.5), float("big", 1e21), float("small", 1e-7),
				float("third", 1.0/3),
			},
		},
		goldenRecord{
			Level: int(slog.LevelWarn), Message: "booleans",
			Attrs: []goldenAttr{boolean("yes", true), boolean("no", false)},
		},
		goldenRecord{
			Level: int(slog.LevelWarn), Message: "durations",
			Attrs: []goldenAttr{
				duration("zero", 0), duration("nanos", 999), duration("micros", 1500),
				duration("millis", 1234*time.Microsecond), duration("second", time.Second),
				duration("backoff", 30*time.Second), duration("retry_in", 5*time.Minute),
				duration("hour", time.Hour+30*time.Second), duration("negative", -2*time.Second),
			},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "times",
			Attrs: []goldenAttr{
				instant("at", goldenTime()),
				instant("utc", time.Date(2026, 1, 2, 3, 4, 5, 0, time.UTC)),
				instant(
					"millis",
					time.Date(2026, 1, 2, 3, 4, 5, 120000000, time.FixedZone("", -5*3600)),
				),
			},
		},
		goldenRecord{
			Level: int(slog.LevelWarn), Message: "device rule failed",
			Attrs: []goldenAttr{
				str("id", "abc"),
				failure(goldenError(
					"device identity could not be established: function not implemented",
				),
				),
			},
		},
		goldenRecord{
			Level:   int(slog.LevelWarn),
			Message: "wrapped error",
			Attrs: []goldenAttr{
				failure(fmt.Errorf(
					"open cgroup %q: %w",
					"/sys/fs/cgroup/x",
					goldenError("no such file or directory"),
				),
				),
			},
		},
		goldenRecord{
			Level: int(slog.LevelWarn), Message: "joined error",
			Attrs: []goldenAttr{str("id", "abc"), failure(joined)},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "nil value",
			Attrs: []goldenAttr{nothing("err")},
		},
		goldenRecord{
			Level: int(
				slog.LevelWarn,
			),
			Message: "mount excluded: no children matched allow/deny policy",
			Attrs: []goldenAttr{
				str("path", "/dev/dri"), strs("children_seen", []string{"card0", "renderD128"}),
				strs("allow_globs", []string{"/dev/dri/*"}), strs("deny_globs", []string{}),
			},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "string lists",
			Attrs: []goldenAttr{
				strs("nil", nil), strs("empty", []string{}), strs("one", []string{"a"}),
				strs("spaced", []string{"a b", "c"}), strs("quoted", []string{`"q"`}),
				strs("empty_item", []string{""}),
			},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "device denied by policy under one of its names",
			Attrs: []goldenAttr{
				str("id", "abc"), stringer("device", "b 8:0"), str("denied_by", "/dev/evil/link"),
			},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "groups",
			Attrs: []goldenAttr{
				group(
					"rule",
					str("type", "c"),
					unsigned("major", 1),
					unsigned("minor", 3),
					str("access", "rwm"),
				),
				group("empty"),
				group("outer", group("inner", str("deep", "value with space"))),
				group("", str("inlined", "x")),
			},
		},
		goldenRecord{
			Level:   int(slog.LevelInfo),
			Message: "with attributes",
			With: []goldenStep{
				{Attrs: []goldenAttr{str("component", "systemd"), integer("attempt", 2)}},
			},
			Attrs: []goldenAttr{duration("retry_in", 30*time.Second)},
		},
		goldenRecord{
			Level: int(slog.LevelWarn), Message: "with a group",
			With:  []goldenStep{{Group: "watcher"}, {Attrs: []goldenAttr{str("owner", ":1.42")}}},
			Attrs: []goldenAttr{failure(goldenError("connection reset"))},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "nested groups",
			With:  []goldenStep{{Group: "a"}, {Group: "b"}},
			Attrs: []goldenAttr{str("key", "value"), group("g", integer("n", 1))},
		},
		goldenRecord{
			Level: int(slog.LevelInfo), Message: "group without attributes",
			With: []goldenStep{{Group: "unused"}},
		},
	)

	return records
}

// toSlog turns a corpus attribute into the slog.Attr the daemon would log.
func toSlog(t *testing.T, attr goldenAttr) slog.Attr {
	t.Helper()

	switch attr.Kind {
	case "string":
		return slog.String(attr.Key, valueText(t, attr))
	case "stringer":
		return slog.Any(attr.Key, goldenStringer(valueText(t, attr)))
	case "error":
		return slog.Any(attr.Key, goldenError(valueText(t, attr)))
	case "int":
		value, err := strconv.ParseInt(valueText(t, attr), 10, 64)
		mustParse(t, attr, err)

		return slog.Int64(attr.Key, value)
	case "uint":
		value, err := strconv.ParseUint(valueText(t, attr), 10, 64)
		mustParse(t, attr, err)

		return slog.Uint64(attr.Key, value)
	case "float":
		value, err := strconv.ParseFloat(valueText(t, attr), 64)
		mustParse(t, attr, err)

		return slog.Float64(attr.Key, value)
	case "duration":
		value, err := strconv.ParseInt(valueText(t, attr), 10, 64)
		mustParse(t, attr, err)

		return slog.Duration(attr.Key, time.Duration(value))
	case "time":
		value, err := time.Parse(time.RFC3339Nano, valueText(t, attr))
		mustParse(t, attr, err)

		return slog.Time(attr.Key, value)
	case "bool":
		value, ok := attr.Value.(bool)
		if !ok {
			t.Fatalf("%s: a bool attribute holds %T", attr.Key, attr.Value)
		}

		return slog.Bool(attr.Key, value)
	case "strings":
		value, ok := attr.Value.([]string)
		if !ok && attr.Value != nil {
			t.Fatalf("%s: a strings attribute holds %T", attr.Key, attr.Value)
		}

		return slog.Any(attr.Key, value)
	case "nil":
		return slog.Any(attr.Key, nil)
	case "group":
		members := make([]any, 0, len(attr.Attrs))
		for _, member := range attr.Attrs {
			members = append(members, toSlog(t, member))
		}

		return slog.Group(attr.Key, members...)
	default:
		t.Fatalf("unknown attribute kind %q", attr.Kind)

		return slog.Attr{}
	}
}

// valueText is the text a string-valued corpus attribute holds.
func valueText(t *testing.T, attr goldenAttr) string {
	t.Helper()

	text, ok := attr.Value.(string)
	if !ok {
		t.Fatalf("%s: a %s attribute holds %T", attr.Key, attr.Kind, attr.Value)
	}

	return text
}

// mustParse fails the test when a corpus attribute's text did not parse.
func mustParse(t *testing.T, attr goldenAttr, err error) {
	t.Helper()

	if err != nil {
		t.Fatalf("%s: %v", attr.Key, err)
	}
}

func toSlogAll(t *testing.T, attrs []goldenAttr) []slog.Attr {
	t.Helper()

	converted := make([]slog.Attr, 0, len(attrs))
	for _, attr := range attrs {
		converted = append(converted, toSlog(t, attr))
	}

	return converted
}

// TestGoldenLogger captures every corpus record in the text, JSON and plain formats, with and
// without the time, through the handlers Configure installs; and the corpus itself.
func TestGoldenLogger(t *testing.T) {
	records := goldenRecords()

	levels := make([]goldenLevel, 0, 9)
	for _, input := range []string{"debug", "info", "warn", "warning", "error", "fatal", "panic", "DEBUG", "Warn", "", "bogus"} {
		levels = append(levels, goldenLevel{Input: input, Level: int(parseLevel(input))})
	}

	corpus := goldenCorpus{
		Time:    goldenTime().Format(time.RFC3339Nano),
		Levels:  levels,
		Records: records,
	}
	goldencapture.Check(t, "tests/testdata/logger/corpus.json", goldencapture.JSON(t, corpus))

	for _, format := range []string{"text", "json", "plain"} {
		for _, includeTime := range []bool{true, false} {
			var output bytes.Buffer

			base := newHandler(&output, format, slog.LevelDebug, includeTime)

			for _, record := range records {
				handler := base

				for _, step := range record.With {
					if step.Group != "" {
						handler = handler.WithGroup(step.Group)
					} else {
						handler = handler.WithAttrs(toSlogAll(t, step.Attrs))
					}
				}

				slogRecord := slog.NewRecord(
					goldenTime(),
					slog.Level(record.Level),
					record.Message,
					0,
				)
				slogRecord.AddAttrs(toSlogAll(t, record.Attrs)...)

				err := handler.Handle(context.Background(), slogRecord)
				if err != nil {
					t.Fatalf("%s: %v", format, err)
				}
			}

			suffix := "notime"
			if includeTime {
				suffix = "time"
			}

			goldencapture.Check(
				t,
				"tests/testdata/logger/"+format+"_"+suffix+".log",
				output.Bytes(),
			)
		}
	}
}
