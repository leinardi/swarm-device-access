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
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"slices"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

var errDryRunEnable = errors.New(
	"dry-run cannot be enabled on a running daemon; to stop enforcing, restart the daemon " +
		"live with a narrower policy, or restart the affected containers",
)

// settings is every value the flags and the config file control.
type settings struct {
	LogFormat    string
	LogLevel     string
	LogTime      bool
	DockerSocket string
	MetricsAddr  string
	DebugAddr    string
	DryRun       bool
	PolicyMode   string
	DeviceAllow  []string
	DeviceDeny   []string
}

// flagSettings snapshots the parsed flags (defaults included) and the set
// of flags given on the command line. Call it right after flag.Parse; the
// slices are copied, so nothing later aliases the flag variables.
func flagSettings() (flags settings, cliSet map[string]bool) {
	cliSet = make(map[string]bool)

	flag.Visit(func(f *flag.Flag) { cliSet[f.Name] = true })

	return settings{
		LogFormat:    *logFormat,
		LogLevel:     *logLevel,
		LogTime:      *logTime,
		DockerSocket: *dockerSocket,
		MetricsAddr:  *metricsAddr,
		DebugAddr:    *debugAddr,
		DryRun:       *dryRun,
		PolicyMode:   *policyMode,
		DeviceAllow:  slices.Clone(deviceAllow),
		DeviceDeny:   slices.Clone(deviceDeny),
	}, cliSet
}

// mergeSettings returns the effective settings: a flag given on the command
// line wins, then a key set in the file, then the flag's default (whatever
// defaults holds). It is pure and never aliases file's slices, so a reload
// that drops a key from the file falls back to the command line or the
// default, not to what an earlier file said.
//
//nolint:gocritic // hugeParam: settings is passed by value on purpose, the result must not share it
func mergeSettings(defaults settings, cliSet map[string]bool, file config.FileSchema) settings {
	merged := defaults
	merged.DeviceAllow = slices.Clone(defaults.DeviceAllow)
	merged.DeviceDeny = slices.Clone(defaults.DeviceDeny)

	fileString := func(name, value string, target *string) {
		if !cliSet[name] && value != "" {
			*target = value
		}
	}

	fileBool := func(name string, value *bool, target *bool) {
		if !cliSet[name] && value != nil {
			*target = *value
		}
	}

	fileList := func(name string, value []string, target *[]string) {
		if !cliSet[name] && value != nil {
			*target = slices.Clone(value)
		}
	}

	fileString("log-format", file.LogFormat, &merged.LogFormat)
	fileString("log-level", file.LogLevel, &merged.LogLevel)
	fileBool("log-time", file.LogTime, &merged.LogTime)
	fileString("docker-socket", file.DockerSocket, &merged.DockerSocket)
	fileString("metrics-addr", file.MetricsAddr, &merged.MetricsAddr)
	fileString("debug-addr", file.DebugAddr, &merged.DebugAddr)
	fileBool("dry-run", file.DryRun, &merged.DryRun)
	fileString("policy-mode", file.PolicyMode, &merged.PolicyMode)
	fileList("device-allow", file.DeviceAllow, &merged.DeviceAllow)
	fileList("device-deny", file.DeviceDeny, &merged.DeviceDeny)

	return merged
}

// policy returns the global policy the settings describe.
func (s *settings) policy() policy.Global {
	return policy.Global{
		Mode:        policy.Mode(s.PolicyMode),
		DeviceAllow: slices.Clone(s.DeviceAllow),
		DeviceDeny:  slices.Clone(s.DeviceDeny),
	}
}

// runtime returns the hot-reloadable part the processor reads.
func (s *settings) runtime() config.Runtime {
	return config.Runtime{DryRun: s.DryRun, Policy: s.policy()}
}

// validate checks every enumerated value and the policy; it runs before
// anything (the logger included) is configured from the settings.
func (s *settings) validate() error {
	return errors.Join(
		config.ValidateEnums(s.LogFormat, s.LogLevel, s.PolicyMode, "setting"),
		s.policy().Validate(),
	)
}

// coldChanges names the restart-only settings that differ between s and
// next.
func (s *settings) coldChanges(next *settings) []string {
	var changed []string

	if next.DockerSocket != s.DockerSocket {
		changed = append(changed, "docker-socket")
	}

	if next.MetricsAddr != s.MetricsAddr {
		changed = append(changed, "metrics-addr")
	}

	if next.DebugAddr != s.DebugAddr {
		changed = append(changed, "debug-addr")
	}

	return changed
}

// reloader applies a re-read config file on SIGHUP.
type reloader struct {
	path   string
	flags  settings
	cliSet map[string]bool
	// current is what is in force: the startup settings, then those of
	// the last reload that succeeded.
	current settings

	configureLogger func(format, level string, includeTime bool)
	publish         func(ctx context.Context, rt config.Runtime) uint64
}

// reload re-reads the file and applies the hot-reloadable settings (logging,
// dry-run, policy). On any error nothing is applied: the store, the logger
// and current keep their previous values. A changed restart-only setting
// is logged and ignored. dry-run may be turned off but not on: turning it
// on would leave every grant in place while no longer enforcing narrower
// policy.
func (r *reloader) reload(ctx context.Context) error {
	file, err := config.LoadFile(r.path)
	if err != nil {
		return fmt.Errorf("config reload failed: %w", err)
	}

	next := mergeSettings(r.flags, r.cliSet, file)

	err = next.validate()
	if err != nil {
		return fmt.Errorf("config reload: invalid setting: %w", err)
	}

	if next.DryRun && !r.current.DryRun {
		return fmt.Errorf("config reload: %w", errDryRunEnable)
	}

	for _, key := range r.current.coldChanges(&next) {
		logger.L().Info("config reload: setting requires a restart; ignored", "key", key)
	}

	next.DockerSocket = r.current.DockerSocket
	next.MetricsAddr = r.current.MetricsAddr
	next.DebugAddr = r.current.DebugAddr

	r.configureLogger(next.LogFormat, next.LogLevel, next.LogTime)

	generation := r.publish(ctx, next.runtime())
	r.current = next

	logger.L().Info("config reloaded",
		"generation", generation,
		"dry_run", next.DryRun,
		"policy_mode", next.PolicyMode,
		"device_allow", next.DeviceAllow,
		"device_deny", next.DeviceDeny,
	)

	return nil
}

// watchSIGHUP blocks until ctx is done, reloading the config on each
// SIGHUP. sigCh must already be registered for SIGHUP (see run), so a
// SIGHUP during startup is queued instead of terminating the process.
func watchSIGHUP(ctx context.Context, sigCh <-chan os.Signal, reload *reloader) {
	for {
		select {
		case <-ctx.Done():
			return

		case <-sigCh:
			logger.L().Info("SIGHUP received; reloading config", "config_file", reload.path)

			err := reload.reload(ctx)
			if err != nil {
				logger.L().Error("config reload rejected; keeping previous config", "err", err)
			}
		}
	}
}
