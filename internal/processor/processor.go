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
// bind-mount /dev/... paths. Inspector and Cfg are required; Publisher is
// required for PublishAndReconcile; Metrics may be nil (calls become
// no-ops). HostRoot is the container-internal path to the
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
	Publisher   *config.Publisher
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

	// filterCache remembers the cgroup v2 runtime device filters seen per
	// cgroup, so a filter wiped by systemd's daemon-reload can be rebuilt.
	filterCache cgroup.FilterCache

	// known maps each container lifecycle to the cgroup it was last
	// verified in (guarded by mu). It is how grants are revoked once the
	// process is gone or Docker cannot be asked: see revokeKnown.
	known map[lifecycleKey]knownCgroup

	// requestPass receives each generation PublishAndReconcile publishes
	// (guarded by mu).
	requestPass PassRequester

	// pinner pins container processes; nil means pidfds. Tests replace it.
	pinner processPinner

	// newCgroup builds the cgroup API for a version; nil means cgroup.New.
	// Tests replace it to observe or fail the mutation without a kernel.
	newCgroup func(version int, ledger *cgroup.Ledger, cache *cgroup.FilterCache) (cgroup.Interface, error)

	// afterCompute, when set, runs under mu after the desired rules are
	// computed and before they are applied. Tests use it to hold a worker
	// mid-flight.
	afterCompute func()
}

// lifecycleKey names one run of a container. A restart under the same ID
// has a new StartedAt, so what is known about the old run never applies to
// the new one.
type lifecycleKey struct {
	containerID string
	startedAt   string
}

// knownCgroup is the cgroup a lifecycle was last verified in: the identity
// of the directory its handle was opened on, the cgroup version, and
// whether the container is privileged (it then has no device filter).
type knownCgroup struct {
	identity   cgroup.Identity
	version    int
	privileged bool
}

// desiredSet is the outcome of evaluating a container's labels, the
// current policy and its /dev mounts.
type desiredSet struct {
	// rules is what the container's cgroup must hold. It is empty whenever
	// the set cannot be established with confidence.
	rules     []cgroup.DeviceRule
	enabled   bool
	collected containerRules
	// incomplete is non-nil when some device of an enabled container could
	// not be resolved; the container is then retried.
	incomplete error
}

// Reconcile makes the device grants of a container's cgroup equal exactly
// what the current config allows it. Start, unpause, startup enumeration,
// systemd re-apply and reload all run this one idempotent path.
//
// Every outcome that cannot establish the desired set with confidence
// (policy disabled or not opted in, invalid labels, an unresolved device)
// is the empty set, and the empty set is applied like any other: returning
// early would keep grants the current config no longer allows. The cost is
// that a transient error revokes the container's grants until a retry
// grants them again.
//
// The container's process is pinned (see applyPinned), so a recycled pid
// cannot direct the rules to another cgroup. A container that is no longer
// running, or that Docker cannot report on, has its grants revoked in the
// cgroup its lifecycle was last verified in.
func (p *Processor) Reconcile(ctx context.Context, containerID string) error {
	log := logger.L()

	info, inspectErr := p.inspect(ctx, containerID)

	p.mu.Lock()
	defer p.mu.Unlock()

	cfg := p.Cfg.Load()

	if inspectErr != nil {
		// The caller canceled (daemon shutdown), not Docker: nothing would
		// retry and grant again, so a running container keeps its grants.
		// A deadline is Docker not answering in time, and does revoke.
		if errors.Is(ctx.Err(), context.Canceled) {
			return inspectErr
		}

		return p.revokeAfterInspectFailure(containerID, cfg.DryRun, inspectErr)
	}

	if info.State == nil || !info.State.Running || info.State.Pid == 0 {
		log.Debug("container has no live pid; skipping", "id", containerID)
		p.Metrics.RecordContainerSkipped("no_pid")

		if cfg.DryRun {
			return nil
		}

		return p.revokeKnown(
			lifecycleKey{containerID: containerID, startedAt: startedAt(info.State)},
		)
	}

	desired := p.computeDesired(containerID, &info, cfg)

	if p.afterCompute != nil {
		p.afterCompute()
	}

	if cfg.DryRun {
		p.logDryRun(containerID, desired.rules)
	} else {
		privileged := info.HostConfig != nil && info.HostConfig.Privileged

		cgroupPath, applyErr := p.applyPinned(
			ctx,
			containerID,
			info.State,
			privileged,
			desired.rules,
		)
		if applyErr != nil {
			// A container skipped for good still reports an incomplete set.
			return errors.Join(
				p.classifyApplyError(containerID, cgroupPath, privileged, applyErr),
				desired.incomplete,
			)
		}
	}

	if desired.enabled && desired.collected.devMounts > 0 {
		log.Info("container processed",
			"id", containerID,
			"pid", info.State.Pid,
			"devices_granted", len(desired.rules),
			"skipped", desired.collected.skipped,
			"errors", len(desired.collected.deviceErrs),
			"dry_run", cfg.DryRun,
		)
	}

	return desired.incomplete
}

// inspect runs one bounded ContainerInspect.
func (p *Processor) inspect(
	ctx context.Context,
	containerID string,
) (container.InspectResponse, error) {
	callCtx, cancel := p.callContext(ctx)
	defer cancel()

	inspected, err := p.Inspector.ContainerInspect(
		callCtx,
		containerID,
		client.ContainerInspectOptions{},
	)
	if err != nil {
		return container.InspectResponse{}, fmt.Errorf("inspect container %q: %w", containerID, err)
	}

	return inspected.Container, nil
}

// computeDesired evaluates the container's labels and /dev mounts under
// cfg. It never touches the container's cgroup.
func (p *Processor) computeDesired(
	containerID string,
	info *container.InspectResponse,
	cfg config.Runtime,
) desiredSet {
	log := logger.L()

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

	cpol, parseErr := policy.ParseContainer(containerLabels)
	if parseErr != nil {
		log.Warn("container skipped: invalid policy labels",
			"id", containerID, "err", parseErr)
		p.Metrics.RecordContainerSkipped("invalid_labels")

		return desiredSet{}
	}

	if !cfg.Policy.Enabled(cpol) {
		log.Debug("container skipped by policy",
			"id", containerID,
			"mode", cfg.Policy.Mode,
		)
		p.Metrics.RecordContainerSkipped("policy")

		return desiredSet{}
	}

	p.Metrics.RecordContainerScanned()

	collected := collectContainerRules(containerID, info.State.Pid, info.Mounts, cfg.Policy, cpol)

	for _, deviceErr := range collected.deviceErrs {
		log.Warn("device rule failed", "id", containerID, "err", deviceErr)
	}

	p.Metrics.AddDeviceFilesDiscovered(len(collected.granted))

	if len(collected.deviceErrs) == 0 {
		return desiredSet{rules: collected.granted, enabled: true, collected: collected}
	}

	p.Metrics.AddRuleFailures(len(collected.deviceErrs))
	log.Warn("device set incomplete; no devices are granted until every device resolves",
		"id", containerID, "errors", len(collected.deviceErrs), "reason", "incomplete_device_set")

	return desiredSet{
		enabled:   true,
		collected: collected,
		incomplete: fmt.Errorf(
			"container %q (reason incomplete_device_set, retryable): %w",
			containerID,
			errors.Join(collected.deviceErrs...),
		),
	}
}

// logDryRun reports what a real run would set. Dry-run stays unprivileged:
// it reads neither /proc nor the cgroup.
func (p *Processor) logDryRun(containerID string, rules []cgroup.DeviceRule) {
	log := logger.L()

	for _, rule := range rules {
		log.Info("dry-run: would add device rule",
			"id", containerID,
			"type", rule.Type,
			"major", *rule.Major,
			"minor", *rule.Minor,
		)
	}

	log.Info("dry-run: would set device rules", "id", containerID, "rules", len(rules))
	p.Metrics.AddDryRunSkips(len(rules))
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
func (p *Processor) classifyApplyError(
	containerID, cgroupPath string,
	privileged bool,
	applyErr error,
) error {
	log := logger.L()

	switch {
	case errors.Is(applyErr, cgroup.ErrFilterMissing) && privileged:
		// A privileged container runs without a device filter by design;
		// there is nothing to add grants to and nothing to restrict.
		log.Info("privileged container has no device filter; nothing to do",
			"id", containerID, "cgroup", cgroupPath)
		p.Metrics.RecordContainerSkipped("privileged_no_filter")

		return nil

	case errors.Is(applyErr, cgroup.ErrFilterMissing):
		// Most likely wiped by systemd's daemon-reload before this process
		// saw the cgroup, so the runtime's own filter cannot be rebuilt.
		log.Warn("device filter missing and no cached original; restart the container",
			"id", containerID, "cgroup", cgroupPath, "reason", "filter_missing")

		return fmt.Errorf(
			"container %q (reason filter_missing, retryable): %w",
			containerID,
			applyErr,
		)

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

func hostCGroupPath(hostRoot, sysfsPath, cgroupPrefix, cgroupRoot string) string {
	return filepath.Join(hostRoot, sysfsPath, cgroupPrefix, cgroupRoot)
}
