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
	"flag"
	"os"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
	"github.com/leinardi/swarm-device-access/internal/processor"
)

// applyFileConfig merges the file config into the CLI flags (for flags not
// explicitly set by the user) and returns the resulting startup Runtime.
// Must be called after flag.Parse().
//
//nolint:gocyclo,cyclop // complexity is inherent: merges many independent optional config fields
func applyFileConfig(fileCfg *config.FileSchema) config.Runtime {
	cliSet := make(map[string]bool)

	flag.Visit(func(f *flag.Flag) { cliSet[f.Name] = true })

	if !cliSet["log-format"] && fileCfg.LogFormat != "" {
		*logFormat = fileCfg.LogFormat
	}

	if !cliSet["log-level"] && fileCfg.LogLevel != "" {
		*logLevel = fileCfg.LogLevel
	}

	if !cliSet["log-time"] && fileCfg.LogTime != nil {
		*logTime = *fileCfg.LogTime
	}

	if !cliSet["docker-socket"] && fileCfg.DockerSocket != "" {
		*dockerSocket = fileCfg.DockerSocket
	}

	if !cliSet["metrics-addr"] && fileCfg.MetricsAddr != "" {
		*metricsAddr = fileCfg.MetricsAddr
	}

	if !cliSet["debug-addr"] && fileCfg.DebugAddr != "" {
		*debugAddr = fileCfg.DebugAddr
	}

	if !cliSet["dry-run"] && fileCfg.DryRun != nil {
		*dryRun = *fileCfg.DryRun
	}

	if !cliSet["policy-mode"] && fileCfg.PolicyMode != "" {
		*policyMode = fileCfg.PolicyMode
	}

	if !cliSet["device-allow"] && fileCfg.DeviceAllow != nil {
		deviceAllow = fileCfg.DeviceAllow
	}

	if !cliSet["device-deny"] && fileCfg.DeviceDeny != nil {
		deviceDeny = fileCfg.DeviceDeny
	}

	return config.Runtime{
		DryRun: *dryRun,
		Policy: policy.Global{
			Mode:        policy.Mode(*policyMode),
			DeviceAllow: append([]string(nil), deviceAllow...),
			DeviceDeny:  append([]string(nil), deviceDeny...),
		},
	}
}

// watchSIGHUP blocks until ctx is done, reloading the config file and
// updating the logger on each SIGHUP, then publishing the new config through
// proc, which reconciles every running container under it. Settings that
// require a restart (docker-socket, metrics-addr, debug-addr) are not
// reloaded.
//
// sigCh must already be registered for SIGHUP (see run), so a SIGHUP during
// startup is queued instead of terminating the process.
//
//nolint:gocyclo,cyclop,gocognit // complexity is inherent: handles SIGHUP, reload, merge, validate, and logger update
func watchSIGHUP(ctx context.Context, sigCh <-chan os.Signal, proc *processor.Processor) {
	for {
		select {
		case <-ctx.Done():
			return

		case <-sigCh:
			log := logger.L()
			log.Info("SIGHUP received; reloading config", "config_file", *configFile)

			fileCfg, err := config.LoadFile(*configFile)
			if err != nil {
				log.Error("config reload failed", "err", err)

				continue
			}

			cliSet := make(map[string]bool)

			flag.Visit(func(f *flag.Flag) { cliSet[f.Name] = true })

			effectiveLogFormat := *logFormat
			effectiveLogLevel := *logLevel
			effectiveLogTime := *logTime

			if !cliSet["log-format"] && fileCfg.LogFormat != "" {
				effectiveLogFormat = fileCfg.LogFormat
			}

			if !cliSet["log-level"] && fileCfg.LogLevel != "" {
				effectiveLogLevel = fileCfg.LogLevel
			}

			if !cliSet["log-time"] && fileCfg.LogTime != nil {
				effectiveLogTime = *fileCfg.LogTime
			}

			logger.Configure(effectiveLogFormat, effectiveLogLevel, effectiveLogTime)

			newDryRun := *dryRun
			newPolicyMode := *policyMode

			newDeviceAllow := append([]string(nil), deviceAllow...)
			newDeviceDeny := append([]string(nil), deviceDeny...)

			if !cliSet["dry-run"] && fileCfg.DryRun != nil {
				newDryRun = *fileCfg.DryRun
			}

			if !cliSet["policy-mode"] && fileCfg.PolicyMode != "" {
				newPolicyMode = fileCfg.PolicyMode
			}

			if !cliSet["device-allow"] && fileCfg.DeviceAllow != nil {
				newDeviceAllow = fileCfg.DeviceAllow
			}

			if !cliSet["device-deny"] && fileCfg.DeviceDeny != nil {
				newDeviceDeny = fileCfg.DeviceDeny
			}

			newPolicy := policy.Global{
				Mode:        policy.Mode(newPolicyMode),
				DeviceAllow: newDeviceAllow,
				DeviceDeny:  newDeviceDeny,
			}

			valErr := newPolicy.Validate()
			if valErr != nil {
				log.Error(
					"config reload: invalid policy; keeping previous config",
					"err", valErr,
				)

				continue
			}

			generation := proc.PublishAndReconcile(ctx, config.Runtime{
				DryRun: newDryRun,
				Policy: newPolicy,
			})

			logger.L().Info("config reloaded",
				"generation", generation,
				"dry_run", newDryRun,
				"policy_mode", newPolicy.Mode,
				"device_allow", newPolicy.DeviceAllow,
				"device_deny", newPolicy.DeviceDeny,
			)
		}
	}
}
