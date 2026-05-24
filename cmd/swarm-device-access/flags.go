//go:build linux

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

package main

import (
	"flag"
	"strings"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

// stringSliceFlag is a repeatable flag: -device-allow /dev/nvidia* -device-allow /dev/dri/*.
type stringSliceFlag []string

func (f *stringSliceFlag) String() string { return strings.Join(*f, ",") }
func (f *stringSliceFlag) Set(val string) error {
	*f = append(*f, val)

	return nil
}

var (
	logFormat    = flag.String("log-format", "text", "Either json, text or plain")
	logLevel     = flag.String("log-level", "info", "Either debug, info, warn, error, fatal, panic")
	logTime      = flag.Bool("log-time", false, "Include timestamp in logs")
	dockerSocket = flag.String(
		"docker-socket",
		"/var/run/docker.sock",
		"Path to the Docker UNIX socket",
	)
	dryRun = flag.Bool("dry-run", false,
		"Log device rules that would be applied without writing to the cgroup",
	)
	policyMode = flag.String(
		"policy-mode",
		string(policy.ModeOptIn),
		`Container processing mode: "opt-in" processes only containers with label `+
			`swarm-device-access.enable=true; "all" processes all containers unless `+
			`label swarm-device-access.enable=false.`,
	)

	deviceAllow stringSliceFlag
	deviceDeny  stringSliceFlag

	metricsAddr = flag.String("metrics-addr", "",
		"Address for the Prometheus metrics + health endpoints (e.g. :9090). Empty = disabled.")
	debugAddr = flag.String("debug-addr", "",
		"Address for the pprof debug endpoints (e.g. :6060). Empty = disabled.")

	configFile = flag.String("config", "",
		"Path to a YAML config file. CLI flags override file values. Reload with SIGHUP.")

	help = flag.Bool("help", false, "Display help message")
)

//nolint:gochecknoinits // flag registration requires init
func init() {
	flag.Var(&deviceAllow, "device-allow",
		"Glob pattern for /dev/... paths to allow (repeatable). Empty means allow all.")
	flag.Var(&deviceDeny, "device-deny",
		"Glob pattern for /dev/... paths to deny (repeatable; takes priority over -device-allow).")
}
