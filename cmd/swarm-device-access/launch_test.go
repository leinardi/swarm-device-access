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
	"errors"
	"flag"
	"io"
	"os"
	"slices"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/internal/launcher"
)

func TestParseLaunchArgs_SplitsAtDoubleDash(t *testing.T) {
	t.Parallel()

	parsed, err := parseLaunchArgs([]string{
		"-dbus", "-host-network", "-config-dir", "/srv/sda", "-log-format", "json",
		"--", "-dry-run", "-device-allow", "/dev/sd*", "--", "-x",
	}, io.Discard)
	if err != nil {
		t.Fatal(err)
	}

	want := launcher.Options{
		HostDockerSocket: "/var/run/docker.sock",
		DBus:             true,
		ConfigDir:        "/srv/sda",
		HostNetwork:      true,
		DaemonArgs:       []string{"-dry-run", "-device-allow", "/dev/sd*", "--", "-x"},
	}

	got := parsed.Opts
	if got.HostDockerSocket != want.HostDockerSocket || got.DBus != want.DBus ||
		got.ConfigDir != want.ConfigDir || got.HostNetwork != want.HostNetwork ||
		!slices.Equal(got.DaemonArgs, want.DaemonArgs) {
		t.Errorf("options = %+v, want %+v", got, want)
	}

	if parsed.LogFormat != "json" || parsed.LogLevel != "info" {
		t.Errorf("log settings = %q, %q", parsed.LogFormat, parsed.LogLevel)
	}
}

func TestParseLaunchArgs_NoDaemonArgs(t *testing.T) {
	t.Parallel()

	parsed, err := parseLaunchArgs(nil, io.Discard)
	if err != nil {
		t.Fatal(err)
	}

	if len(parsed.Opts.DaemonArgs) != 0 || parsed.Opts.DBus || parsed.Opts.ConfigDir != "" {
		t.Errorf("defaults = %+v", parsed.Opts)
	}
}

func TestParseLaunchArgs_Rejects(t *testing.T) {
	t.Parallel()

	cases := map[string]struct {
		args    []string
		wantErr error
		wantMsg string
	}{
		"relative config dir": {
			[]string{"-config-dir", "etc/sda"},
			errPathNotAbsolute,
			`"-config-dir"`,
		},
		"unclean config dir": {
			[]string{"-config-dir", "/srv/../etc"},
			errPathNotAbsolute,
			`"-config-dir"`,
		},
		"trailing slash": {
			[]string{"-config-dir", "/srv/sda/"},
			errPathNotAbsolute,
			`"-config-dir"`,
		},
		"relative socket": {
			[]string{"-host-docker-socket", "docker.sock"},
			errPathNotAbsolute,
			`"-host-docker-socket"`,
		},
		"empty socket": {
			[]string{"-host-docker-socket", ""},
			errPathNotAbsolute,
			`"-host-docker-socket"`,
		},
		"daemon flag before --": {
			[]string{"-dbus", "-dry-run"},
			nil,
			"flag provided but not defined",
		},
		"stray argument":     {[]string{"extra", "--", "-dry-run"}, errUnexpectedArg, `"extra"`},
		"unknown log format": {[]string{"-log-format", "xml"}, nil, `flag "log-format"`},
		"unknown log level":  {[]string{"-log-level", "trace"}, nil, `flag "log-level"`},
	}

	for name, tc := range cases {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			_, err := parseLaunchArgs(tc.args, io.Discard)
			if err == nil {
				t.Fatal("accepted")
			}

			if tc.wantErr != nil && !errors.Is(err, tc.wantErr) {
				t.Errorf("err = %v, want %v", err, tc.wantErr)
			}

			if !strings.Contains(err.Error(), tc.wantMsg) {
				t.Errorf("err = %v, want it to mention %s", err, tc.wantMsg)
			}
		})
	}
}

func TestParseLaunchArgs_Help(t *testing.T) {
	t.Parallel()

	var out bytes.Buffer

	_, err := parseLaunchArgs([]string{"-help"}, &out)
	if !errors.Is(err, flag.ErrHelp) {
		t.Fatalf("err = %v, want flag.ErrHelp", err)
	}

	for _, want := range []string{"launch [flags] -- [daemon flags]", "-host-docker-socket", "-config-dir"} {
		if !strings.Contains(out.String(), want) {
			t.Errorf("help does not mention %q:\n%s", want, out.String())
		}
	}
}

// run dispatches to launcher mode on a leading "launch", before the daemon's
// flags are parsed: -help and a bad -config-dir are the launcher's answers.
// Not parallel: it sets os.Args.
func TestRun_DispatchesLaunch(t *testing.T) {
	saved := os.Args

	t.Cleanup(func() { os.Args = saved })

	os.Args = []string{"swarm-device-access", "launch", "-help"}

	if status := run(); status != 0 {
		t.Errorf("launch -help: status %d, want 0", status)
	}

	os.Args = []string{"swarm-device-access", "launch", "-config-dir", "relative"}

	if status := run(); status != 1 {
		t.Errorf("launch -config-dir relative: status %d, want 1", status)
	}
}
