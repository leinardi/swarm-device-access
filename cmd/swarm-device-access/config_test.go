//go:build linux

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

package main

import (
	"bytes"
	"context"
	"errors"
	"log/slog"
	"os"
	"path/filepath"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

// defaultSettings are the flag defaults, as flagSettings returns them
// without any flag on the command line.
func defaultSettings() settings {
	return settings{
		LogFormat:    "text",
		LogLevel:     "info",
		DockerSocket: "/var/run/docker.sock",
		PolicyMode:   string(policy.ModeOptIn),
	}
}

func TestMergeSettings_Precedence(t *testing.T) {
	t.Parallel()

	file := config.FileSchema{
		PolicyMode: "all",
		LogLevel:   "debug",
		DeviceDeny: []string{"/dev/sda"},
	}

	cli := defaultSettings()
	cli.LogLevel = "warn"

	merged := mergeSettings(cli, map[string]bool{"log-level": true}, file)

	if merged.LogLevel != "warn" {
		t.Errorf("log-level = %q, want the command line's warn", merged.LogLevel)
	}

	if merged.PolicyMode != "all" || !slices.Equal(merged.DeviceDeny, []string{"/dev/sda"}) {
		t.Errorf(
			"merged = %+v, want the file's policy-mode and device-deny over the defaults",
			merged,
		)
	}

	if merged.LogFormat != "text" || merged.DockerSocket != "/var/run/docker.sock" {
		t.Errorf("merged = %+v, want defaults where neither sets a value", merged)
	}
}

func TestMergeSettings_DoesNotAlias(t *testing.T) {
	t.Parallel()

	defaults := defaultSettings()
	defaults.DeviceAllow = []string{"/dev/null"}

	file := config.FileSchema{DeviceDeny: []string{"/dev/sda"}}

	merged := mergeSettings(defaults, map[string]bool{}, file)
	merged.DeviceAllow[0] = "/dev/changed"
	file.DeviceDeny[0] = "/dev/changed"

	if defaults.DeviceAllow[0] != "/dev/null" || merged.DeviceDeny[0] != "/dev/sda" {
		t.Error("merged settings share a slice with the defaults or the file")
	}
}

// reloadEnv is a reloader wired to a real store and processor publication,
// with the logger configuration recorded instead of applied.
type reloadEnv struct {
	reloader  *reloader
	store     *config.Store
	path      string
	loggerCfg []string
	logs      *bytes.Buffer
}

func newReloadEnv(
	t *testing.T,
	flags *settings,
	cliSet map[string]bool,
	initialFile string,
) *reloadEnv {
	t.Helper()

	env := &reloadEnv{path: filepath.Join(t.TempDir(), "config.yaml")}
	env.writeFile(t, initialFile)

	file, err := config.LoadFile(env.path)
	if err != nil {
		t.Fatal(err)
	}

	current := mergeSettings(*flags, cliSet, file)

	store, publisher := config.NewStore(current.runtime())
	proc := &processor.Processor{Cfg: store, Publisher: publisher}

	env.store = store
	env.reloader = &reloader{
		path:    env.path,
		flags:   *flags,
		cliSet:  cliSet,
		current: current,
		configureLogger: func(format, level string, _ bool) {
			env.loggerCfg = append(env.loggerCfg, format+"/"+level)
		},
		publish: proc.PublishAndReconcile,
	}

	prev := logger.L()
	env.logs = &bytes.Buffer{}
	logger.Set(slog.New(slog.NewTextHandler(env.logs, nil)))
	t.Cleanup(func() { logger.Set(prev) })

	return env
}

func (e *reloadEnv) writeFile(t *testing.T, content string) {
	t.Helper()

	err := os.WriteFile(e.path, []byte(content), 0o600)
	if err != nil {
		t.Fatal(err)
	}
}

func (e *reloadEnv) reload(t *testing.T, content string) error {
	t.Helper()

	e.writeFile(t, content)

	return e.reloader.reload(context.Background())
}

func TestReload_RemovedKeyFallsBackToFlagOrDefault(t *testing.T) {
	cli := defaultSettings()
	cli.DeviceDeny = []string{"/dev/sdb"}

	env := newReloadEnv(t, &cli, map[string]bool{"device-deny": true}, "policy-mode: all\n")

	if got := env.store.Load().Policy.Mode; got != policy.ModeAll {
		t.Fatalf("startup policy-mode = %q, want all from the file", got)
	}

	err := env.reload(t, "log-level: info\n")
	if err != nil {
		t.Fatal(err)
	}

	got := env.store.Load().Policy
	if got.Mode != policy.ModeOptIn || !slices.Equal(got.DeviceDeny, []string{"/dev/sdb"}) {
		t.Errorf(
			"after the key was removed: policy = %+v, want the default opt-in and the CLI deny",
			got,
		)
	}
}

func TestReload_InvalidKeepsStoreAndLogger(t *testing.T) {
	env := newReloadEnv(t, new(defaultSettings()), map[string]bool{}, "policy-mode: all\n")
	before, beforeGen := env.store.Snapshot()

	for name, content := range map[string]string{
		"unparsable file": "policy-mode: [\n",
		"invalid glob":    "log-level: debug\ndevice-allow: [\"/dev/[\"]\n",
		"unknown value":   "log-level: loud\n",
	} {
		err := env.reload(t, content)
		if err == nil {
			t.Fatalf("%s: reload accepted it", name)
		}

		after, afterGen := env.store.Snapshot()
		if afterGen != beforeGen || !reflect.DeepEqual(after, before) {
			t.Errorf("%s: store changed to %+v", name, after)
		}
	}

	if len(env.loggerCfg) != 0 {
		t.Errorf("logger reconfigured by rejected reloads: %v", env.loggerCfg)
	}
}

func TestReload_ColdFieldChangeIgnored(t *testing.T) {
	env := newReloadEnv(
		t,
		new(defaultSettings()),
		map[string]bool{},
		"metrics-addr: \"127.0.0.1:9090\"\n",
	)

	err := env.reload(t, "metrics-addr: \"127.0.0.1:9999\"\npolicy-mode: all\n")
	if err != nil {
		t.Fatal(err)
	}

	if env.reloader.current.MetricsAddr != "127.0.0.1:9090" {
		t.Errorf("metrics-addr = %q, want the startup value kept", env.reloader.current.MetricsAddr)
	}

	if env.store.Load().Policy.Mode != policy.ModeAll {
		t.Error("the hot settings of the same file were not applied")
	}

	if !strings.Contains(env.logs.String(), "setting requires a restart; ignored") ||
		!strings.Contains(env.logs.String(), "key=metrics-addr") {
		t.Errorf("expected an INFO naming the ignored key, got:\n%s", env.logs.String())
	}
}

func TestReload_DryRunTransitions(t *testing.T) {
	t.Run("true to false is applied", func(t *testing.T) {
		env := newReloadEnv(t, new(defaultSettings()), map[string]bool{}, "dry-run: true\n")

		err := env.reload(t, "dry-run: false\n")
		if err != nil {
			t.Fatal(err)
		}

		if env.store.Load().DryRun {
			t.Error("dry-run still on after a reload turned it off")
		}
	})

	t.Run("false to true is rejected", func(t *testing.T) {
		env := newReloadEnv(
			t,
			new(defaultSettings()),
			map[string]bool{},
			"dry-run: false\npolicy-mode: all\n",
		)
		before, beforeGen := env.store.Snapshot()

		err := env.reload(t, "dry-run: true\npolicy-mode: opt-in\n")
		if !errors.Is(err, errDryRunEnable) {
			t.Fatalf("err = %v, want errDryRunEnable", err)
		}

		after, afterGen := env.store.Snapshot()
		if afterGen != beforeGen || !reflect.DeepEqual(after, before) || len(env.loggerCfg) != 0 {
			t.Errorf(
				"rejected reload changed the store to %+v or the logger %v",
				after,
				env.loggerCfg,
			)
		}
	})
}
