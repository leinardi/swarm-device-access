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

package processor

import (
	"context"
	"errors"
	"fmt"
	"path/filepath"

	"github.com/docker/docker/api/types/container"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// ContainerInspector is the subset of *client.Client used by Processor.
// It exists solely to allow unit tests to inject a fake without standing up a
// real Docker daemon.
type ContainerInspector interface {
	ContainerInspect(ctx context.Context, containerID string) (container.InspectResponse, error)
}

// deviceRuleKey is the deduplication key for cgroup device rules collected
// within a single container processing pass.
type deviceRuleKey struct {
	typ   string
	major int64
	minor int64
}

// Processor applies cgroup BPF device-allow rules to containers that
// bind-mount /dev/... paths. Inspector and Cfg are required; Metrics may be
// nil (calls become no-ops). HostRoot is the container-internal path to the
// host root (typically "/host"). ProcRoot is used for /proc lookups ("/" in
// production, temp dir in tests).
type Processor struct {
	Inspector ContainerInspector
	Cfg       *config.Store
	Metrics   *observability.Recorder
	HostRoot  string
	ProcRoot  string
}

// ProcessContainer inspects a container and applies cgroup BPF device-allow
// rules for every bind mount sourced from /dev/...
//
//nolint:cyclop,gocyclo,funlen // inherent: policy-check + inspect + version-detect + path-resolve + mount-filter + collect + apply
func (p *Processor) ProcessContainer(ctx context.Context, containerID string) error {
	log := logger.L()

	info, inspectErr := p.Inspector.ContainerInspect(ctx, containerID)
	if inspectErr != nil {
		return fmt.Errorf("inspect container %q: %w", containerID, inspectErr)
	}

	if info.State == nil || info.State.Pid == 0 {
		log.Debug("container has no live pid; skipping", "id", containerID)
		p.Metrics.RecordContainerSkipped("no_pid")

		return nil
	}

	var labels map[string]string
	if info.Config != nil {
		labels = info.Config.Labels
	}

	cfg := p.Cfg.Load()

	cpol, parseErr := policy.ParseContainer(labels)
	if parseErr != nil {
		log.Warn("container skipped: invalid policy labels",
			"id", containerID, "err", parseErr)
		p.Metrics.RecordContainerSkipped("invalid_labels")

		return nil
	}

	if !cfg.Policy.Enabled(cpol) {
		log.Debug("container skipped by policy",
			"id", containerID,
			"mode", cfg.Policy.Mode,
		)
		p.Metrics.RecordContainerSkipped("policy")

		return nil
	}

	p.Metrics.RecordContainerScanned()

	pid := info.State.Pid

	cgroupVersion, versionErr := cgroup.GetDeviceCGroupVersion(p.ProcRoot, pid)
	if versionErr != nil {
		return fmt.Errorf("detect cgroup version for pid %d: %w", pid, versionErr)
	}

	log.Debug("cgroup version detected", "pid", pid, "version", cgroupVersion)

	api, apiErr := cgroup.New(cgroupVersion)
	if apiErr != nil {
		return fmt.Errorf("init cgroup api (version=%d): %w", cgroupVersion, apiErr)
	}

	cgroupPrefix, sysfsPath, mountErr := api.GetDeviceCGroupMountPath(p.ProcRoot, pid)
	if mountErr != nil {
		return fmt.Errorf("resolve cgroup mount path: %w", mountErr)
	}

	cgroupRoot, rootErr := api.GetDeviceCGroupRootPath(p.ProcRoot, cgroupPrefix, pid)
	if rootErr != nil {
		return fmt.Errorf("resolve cgroup root path: %w", rootErr)
	}

	cgroupPath := hostCGroupPath(p.HostRoot, sysfsPath, cgroupPrefix, cgroupRoot)
	log.Debug("cgroup path resolved", "pid", pid, "path", cgroupPath)

	var (
		allRules []cgroup.DeviceRule
		allErrs  []error
	)

	seen := make(map[deviceRuleKey]struct{})

	for _, mnt := range info.Mounts {
		if !IsMountSource(mnt.Source) {
			continue
		}

		log.Debug("device mount detected",
			"id", containerID,
			"pid", pid,
			"source", mnt.Source,
			"destination", mnt.Destination,
		)

		rules, errs := CollectMountRules(mnt.Source, cfg.Policy, cpol)
		allErrs = append(allErrs, errs...)

		for _, rule := range rules {
			key := deviceRuleKey{rule.Type, *rule.Major, *rule.Minor}
			if _, dup := seen[key]; !dup {
				seen[key] = struct{}{}

				allRules = append(allRules, rule)
			}
		}
	}

	p.Metrics.AddDeviceFilesDiscovered(len(allRules))

	if len(allRules) > 0 {
		applyErr := p.applyRulesToCgroup(api, allRules, cgroupPath, pid, cfg.DryRun)
		if applyErr != nil {
			allErrs = append(allErrs, applyErr)
		}
	}

	if len(allErrs) > 0 {
		p.Metrics.AddRuleFailures(len(allErrs))
	}

	return errors.Join(allErrs...)
}

// applyRulesToCgroup logs and (unless dryRun) attaches the collected device
// rules to the cgroup at cgroupPath via a single AddDeviceRules call.
func (p *Processor) applyRulesToCgroup(
	api cgroup.Interface,
	rules []cgroup.DeviceRule,
	cgroupPath string,
	pid int,
	dryRun bool,
) error {
	log := logger.L()

	for _, rule := range rules {
		if dryRun {
			log.Info("dry-run: would add device rule",
				"pid", pid,
				"cgroup", cgroupPath,
				"type", rule.Type,
				"major", *rule.Major,
				"minor", *rule.Minor,
			)
		} else {
			log.Debug("adding device rule",
				"pid", pid,
				"cgroup", cgroupPath,
				"type", rule.Type,
				"major", *rule.Major,
				"minor", *rule.Minor,
			)
		}
	}

	if dryRun {
		p.Metrics.AddDryRunSkips(len(rules))

		return nil
	}

	err := api.AddDeviceRules(cgroupPath, rules)
	if err != nil {
		return fmt.Errorf("add device rules: %w", err)
	}

	return nil
}

func hostCGroupPath(hostRoot, sysfsPath, cgroupPrefix, cgroupRoot string) string {
	return filepath.Join(hostRoot, sysfsPath, cgroupPrefix, cgroupRoot)
}
