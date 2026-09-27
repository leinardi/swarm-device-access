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
	"sync"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/observability"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// DockerInspector is the subset of *client.Client used by Processor.
// It exists solely to allow unit tests to inject a fake without standing up a
// real Docker daemon. Policy comes from the global config and the container's
// own labels only, so no service or node inspection is part of it.
type DockerInspector interface {
	ContainerInspect(
		ctx context.Context,
		containerID string,
		options client.ContainerInspectOptions,
	) (client.ContainerInspectResult, error)
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
// production, temp dir in tests). CallTimeout bounds each Docker call the
// processor makes, independent of the caller's context, so every entry point
// (not only the daemon's per-container wrapper) has bounded Docker I/O;
// production sets it to daemon.DockerCallTimeout, and zero leaves the calls
// bounded by the caller's context only.
//
// A Processor must not be copied after first use.
type Processor struct {
	Inspector   DockerInspector
	Cfg         *config.Store
	Metrics     *observability.Recorder
	HostRoot    string
	ProcRoot    string
	CallTimeout time.Duration

	// mu serializes policy evaluation and cgroup mutation across every
	// caller (event loop, startup enumeration, systemd re-apply, reload). It
	// is held from the config load through the mutation, not around the
	// mutation alone: otherwise a worker that computed rules under an old
	// config could apply them after a newer config was published and a
	// newer computation had already been applied. One global lock is enough
	// at this scale; any Docker I/O made while holding it is bounded by
	// CallTimeout.
	mu sync.Mutex

	// ledger records the cgroup v1 grants this processor made, so a later
	// Set can revoke them without touching the runtime's own exceptions.
	ledger cgroup.Ledger

	// newCgroup builds the cgroup API for a version; nil means cgroup.New.
	// Tests replace it to observe or fail the mutation without a kernel.
	newCgroup func(version int, ledger *cgroup.Ledger) (cgroup.Interface, error)

	// afterCompute, when set, runs under mu after the rules are computed
	// and before they are applied. Tests use it to hold a worker mid-flight.
	afterCompute func()
}

// ProcessContainer inspects a container and applies cgroup BPF device-allow
// rules for every bind mount sourced from /dev/...
//
//nolint:cyclop,gocyclo,funlen // inherent: policy-check + inspect + version-detect + path-resolve + mount-filter + collect + apply
func (p *Processor) ProcessContainer(ctx context.Context, containerID string) error {
	log := logger.L()

	inspectCtx, cancelInspect := p.callContext(ctx)
	inspected, inspectErr := p.Inspector.ContainerInspect(
		inspectCtx,
		containerID,
		client.ContainerInspectOptions{},
	)

	cancelInspect()

	if inspectErr != nil {
		return fmt.Errorf("inspect container %q: %w", containerID, inspectErr)
	}

	info := inspected.Container

	if info.State == nil || info.State.Pid == 0 {
		log.Debug("container has no live pid; skipping", "id", containerID)
		p.Metrics.RecordContainerSkipped("no_pid")

		return nil
	}

	var containerLabels map[string]string
	if info.Config != nil {
		containerLabels = info.Config.Labels
	}

	// The container's labels are the only label input, so this is the one
	// place a misspelled key can be reported instead of silently ignored.
	for _, unknownKey := range policy.UnknownLabels(containerLabels) {
		log.Warn("unrecognized swarm-device-access label on container",
			"id", containerID, "label", unknownKey)
	}

	p.mu.Lock()
	defer p.mu.Unlock()

	cfg := p.Cfg.Load()

	cpol, parseErr := policy.ParseContainer(containerLabels)
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

	newCgroup := p.newCgroup
	if newCgroup == nil {
		newCgroup = cgroup.New
	}

	api, apiErr := newCgroup(cgroupVersion, &p.ledger)
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

	collected := collectContainerRules(containerID, pid, info.Mounts, cfg.Policy, cpol)

	for _, deviceErr := range collected.deviceErrs {
		log.Warn("device rule failed", "id", containerID, "err", deviceErr)
	}

	p.Metrics.AddDeviceFilesDiscovered(len(collected.granted))

	if len(collected.deviceErrs) > 0 {
		p.Metrics.AddRuleFailures(len(collected.deviceErrs))
	}

	if p.afterCompute != nil {
		p.afterCompute()
	}

	if len(collected.granted) > 0 {
		applyErr := p.applyRulesToCgroup(api, collected.granted, cgroupPath, pid, cfg.DryRun)
		if applyErr != nil {
			return p.classifyApplyError(containerID, cgroupPath, applyErr)
		}
	}

	if collected.devMounts > 0 {
		log.Info("container processed",
			"id", containerID,
			"pid", pid,
			"devices_granted", len(collected.granted),
			"skipped", collected.skipped,
			"errors", len(collected.deviceErrs),
			"dry_run", cfg.DryRun,
		)
	}

	return nil
}

// callContext derives the context for one Docker call.
func (p *Processor) callContext(ctx context.Context) (context.Context, context.CancelFunc) {
	if p.CallTimeout <= 0 {
		return context.WithCancel(ctx)
	}

	return context.WithTimeout(ctx, p.CallTimeout)
}

// classifyApplyError turns a cgroup mutation failure into the container's
// outcome: nil when the container is skipped for good, otherwise an error
// that names the reason, so the caller retries it.
func (p *Processor) classifyApplyError(containerID, cgroupPath string, applyErr error) error {
	log := logger.L()

	switch {
	case errors.Is(applyErr, cgroup.ErrUnsupportedAttachMode):
		// Permanent for this container: the runtime chose the attach mode.
		// Nothing was attached or detached, so it keeps only the runtime's
		// own device filter.
		log.Error("container skipped: device filter attach mode not supported; "+
			"the runtime must attach device filters with BPF_F_ALLOW_MULTI",
			"id", containerID, "cgroup", cgroupPath, "err", applyErr)
		p.Metrics.RecordContainerSkipped("unsupported_attach_mode")

		return nil

	case errors.Is(applyErr, cgroup.ErrOwnedBlockConflict):
		// Nothing was changed. Only a fresh device filter (a container
		// restart) clears it; retrying keeps the container pending.
		log.Error(
			"device filter carries an invalid swarm-device-access block; "+
				"nothing was changed, restart the container to replace its device filter",
			"id",
			containerID,
			"cgroup",
			cgroupPath,
			"reason",
			"owned_block_conflict",
			"err",
			applyErr,
		)

		return fmt.Errorf(
			"container %q (reason owned_block_conflict, retryable): %w",
			containerID,
			applyErr,
		)

	case errors.Is(applyErr, cgroup.ErrProgramNotWrappable):
		return fmt.Errorf(
			"container %q (reason program_not_wrappable, retryable): %w",
			containerID,
			applyErr,
		)

	case errors.Is(applyErr, cgroup.ErrFiltersInaccessible):
		return fmt.Errorf(
			"container %q (reason filters_inaccessible, retryable): %w",
			containerID,
			applyErr,
		)

	default:
		return applyErr
	}
}

// containerRules aggregates the per-mount results for one container.
type containerRules struct {
	granted    []cgroup.DeviceRule // deduplicated across mounts
	skipped    int
	deviceErrs []error
	devMounts  int
}

// collectContainerRules walks every /dev mount of a container and merges the
// results. Per-device errors are collected, not returned, so one bad entry
// does not prevent the remaining rules from being applied.
func collectContainerRules(
	containerID string,
	pid int,
	mounts []container.MountPoint,
	gpol policy.Global,
	cpol policy.Container,
) containerRules {
	var result containerRules

	seen := make(map[deviceRuleKey]struct{})

	for _, mnt := range mounts {
		if !IsMountSource(mnt.Source) {
			continue
		}

		result.devMounts++

		logger.L().Debug("device mount detected",
			"id", containerID,
			"pid", pid,
			"source", mnt.Source,
			"destination", mnt.Destination,
		)

		mountResult := CollectMountRules(mnt.Source, gpol, cpol)
		result.skipped += mountResult.Skipped
		result.deviceErrs = append(result.deviceErrs, mountResult.Errs...)

		for _, rule := range mountResult.Rules {
			key := deviceRuleKey{rule.Type, *rule.Major, *rule.Minor}
			if _, dup := seen[key]; !dup {
				seen[key] = struct{}{}

				result.granted = append(result.granted, rule)
			}
		}
	}

	return result
}

// applyRulesToCgroup logs and (unless dryRun) sets the collected device rules
// on the cgroup at cgroupPath. The directory is opened once here; the cgroup
// API then works on that descriptor only, so the rules cannot land in a
// different cgroup recreated at the same path.
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

	handle, err := cgroup.OpenCgroup(cgroupPath)
	if err != nil {
		return fmt.Errorf("set device rules: %w", err)
	}

	defer func() {
		closeErr := handle.Close()
		if closeErr != nil {
			log.Warn("close cgroup handle", "cgroup", cgroupPath, "err", closeErr)
		}
	}()

	err = api.SetDeviceRules(handle, rules)
	if err != nil {
		return fmt.Errorf("set device rules: %w", err)
	}

	return nil
}

func hostCGroupPath(hostRoot, sysfsPath, cgroupPrefix, cgroupRoot string) string {
	return filepath.Join(hostRoot, sysfsPath, cgroupPrefix, cgroupRoot)
}
