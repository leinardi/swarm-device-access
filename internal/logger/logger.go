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
	"os"
	"strings"
	"sync"
	"sync/atomic"
)

// globalLogger holds the configured slog.Logger.
// Access it with L() and set it with Set()/Configure().
var globalLogger atomic.Pointer[slog.Logger]

// initOnce ensures the default logger is initialized exactly once.
var initOnce sync.Once

// L returns the configured slog.Logger. If Configure/Set hasn't been called yet,
// it returns a reasonable default text logger at INFO level to avoid nil panics.
func L() *slog.Logger {
	initOnce.Do(func() {
		if globalLogger.Load() == nil {
			handler := slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{
				Level: slog.LevelInfo,
			})
			globalLogger.Store(slog.New(handler))
		}
	})

	return globalLogger.Load()
}

// Set replaces the global logger (primarily for tests or custom wiring).
func Set(newLogger *slog.Logger) {
	globalLogger.Store(newLogger)
}

// Configure builds and installs a slog.Logger based on CLI flags.
// format: "json" or "text" (unknown -> text)
// level:  "debug", "info", "warn", "error", "fatal", "panic" (fatal/panic -> error)
// includeTime: if false, the time attribute is removed from log records.
func Configure(format, level string, includeTime bool) {
	logLevel := parseLevel(level)

	var handler slog.Handler

	switch strings.ToLower(format) {
	case "json":
		handler = slog.NewJSONHandler(os.Stdout, &slog.HandlerOptions{
			Level:       logLevel,
			ReplaceAttr: timeStripper(includeTime),
		})
	case "plain":
		handler = newPlainTextHandler(os.Stdout, logLevel, includeTime)
	default: // "text"
		handler = slog.NewTextHandler(os.Stdout, &slog.HandlerOptions{
			Level:       logLevel,
			ReplaceAttr: timeStripper(includeTime),
		})
	}

	Set(slog.New(handler))
}

// timeStripper returns a ReplaceAttr function that removes the time attribute
// when includeTime is false. When includeTime is true, it returns nil (no-op).
func timeStripper(includeTime bool) func([]string, slog.Attr) slog.Attr {
	if includeTime {
		return nil
	}

	return func(_ []string, attr slog.Attr) slog.Attr {
		if attr.Key == slog.TimeKey {
			return slog.Attr{} // drop time
		}

		return attr
	}
}

// parseLevel converts a string level to slog.Level.
// Unknown inputs default to INFO; "fatal"/"panic" are treated as ERROR.
func parseLevel(level string) slog.Level {
	switch strings.ToLower(level) {
	case "debug":
		return slog.LevelDebug
	case "info":
		return slog.LevelInfo
	case "warn", "warning":
		return slog.LevelWarn
	case "error", "fatal", "panic":
		return slog.LevelError
	default:
		return slog.LevelInfo
	}
}
