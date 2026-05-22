/*
 * Copyright 2026 Roberto Leinardi
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

package logger

import (
	"log/slog"
	"testing"
	"time"
)

func TestParseLevel(t *testing.T) {
	cases := []struct {
		input string
		want  slog.Level
	}{
		{"debug", slog.LevelDebug},
		{"DEBUG", slog.LevelDebug},
		{"info", slog.LevelInfo},
		{"INFO", slog.LevelInfo},
		{"warn", slog.LevelWarn},
		{"WARN", slog.LevelWarn},
		{"warning", slog.LevelWarn},
		{"error", slog.LevelError},
		{"fatal", slog.LevelError},
		{"panic", slog.LevelError},
		{"", slog.LevelInfo},
		{"unknown", slog.LevelInfo},
		{"VERBOSE", slog.LevelInfo},
	}
	for _, tc := range cases {
		got := parseLevel(tc.input)
		if got != tc.want {
			t.Errorf("parseLevel(%q) = %v, want %v", tc.input, got, tc.want)
		}
	}
}

func TestTimeStripper_IncludeTime_ReturnsNil(t *testing.T) {
	fn := timeStripper(true)
	if fn != nil {
		t.Error("timeStripper(true) should return nil (no-op)")
	}
}

func TestTimeStripper_ExcludeTime_DropsTimeKey(t *testing.T) {
	fn := timeStripper(false)
	if fn == nil {
		t.Fatal("timeStripper(false) should return a function")
	}

	timeAttr := slog.Time(slog.TimeKey, time.Now())

	result := fn(nil, timeAttr)
	if !result.Equal(slog.Attr{}) {
		t.Errorf("expected time attr to be dropped, got %v", result)
	}
}

func TestTimeStripper_ExcludeTime_PassesOtherAttrs(t *testing.T) {
	fn := timeStripper(false)
	if fn == nil {
		t.Fatal("timeStripper(false) should return a function")
	}

	other := slog.String("service", "myapp")

	result := fn(nil, other)
	if result.Key != "service" || result.Value.String() != "myapp" {
		t.Errorf("non-time attr should pass through, got %v", result)
	}
}
