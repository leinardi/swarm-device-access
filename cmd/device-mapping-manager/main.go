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

// Package main wires and runs the device-mapping-manager daemon.
// It listens to Docker container-start events and injects cgroup v2 BPF
// device-allow rules for any container that bind-mounts a /dev/... path.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"syscall"
	"time"

	dockertypes "github.com/docker/docker/api/types"
	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/filters"
	"github.com/docker/docker/client"
	"github.com/leinardi/device-mapping-manager/internal/cgroup"
	"github.com/leinardi/device-mapping-manager/internal/logger"
	"github.com/leinardi/device-mapping-manager/internal/systemd"
	"golang.org/x/sys/unix"
)

// containerInspector is the subset of *client.Client used by processContainer.
// It exists solely to allow unit tests to inject a fake without standing up a
// real Docker daemon.
type containerInspector interface {
	ContainerInspect(ctx context.Context, id string) (dockertypes.ContainerJSON, error)
}

const (
	// hostRootPath is where the host's "/" is expected to be mounted inside
	// the daemon container. Setting cgroup BPF programs requires writing to
	// /sys/fs/cgroup of the host, so the daemon needs the host root visible
	// to it. See the example docker-compose for the bind mount layout.
	hostRootPath = "/host"

	minBackoff = 1 * time.Second
	maxBackoff = 30 * time.Second
)

var (
	logFormat    = flag.String("log-format", "text", "Either json, text or plain")
	logLevel     = flag.String("log-level", "info", "Either debug, info, warn, error, fatal, panic")
	logTime      = flag.Bool("log-time", false, "Include timestamp in logs")
	dockerSocket = flag.String(
		"docker-socket",
		"/var/run/docker.sock",
		"Path to the Docker UNIX socket",
	)
	help = flag.Bool("help", false, "Display help message")
)

// ptr returns a pointer to v. Generic helper used to inline scalar pointers
// for cgroup.DeviceRule fields.
func ptr[T any](v T) *T {
	return &v
}

func main() {
	os.Exit(run())
}

func run() int {
	flag.Parse()

	if *help {
		flag.PrintDefaults()

		return 0
	}

	if logErr := logger.Configure(*logFormat, *logLevel, *logTime); logErr != nil {
		fmt.Fprintf(os.Stderr, "logger setup failed, falling back to defaults: %v\n", logErr)
	}
	log := logger.L()

	log.Info("device-mapping-manager starting",
		"version", version,
		"commit", commit,
		"date", date,
	)

	rootCtx, cancelRoot := signal.NotifyContext(
		context.Background(),
		syscall.SIGINT,
		syscall.SIGTERM,
	)
	defer cancelRoot()

	cli, err := client.NewClientWithOpts(
		client.WithHost("unix://"+*dockerSocket),
		client.WithAPIVersionNegotiation(),
	)
	if err != nil {
		log.Error("docker client init failed", "err", err)

		return 1
	}
	defer cli.Close()

	// Apply rules to containers that are already running when we start up.
	// Track their IDs so the start-event handler can skip them on the brief
	// window where both events and the initial enumeration overlap.
	processed := make(map[string]struct{})
	if processErr := processExistingContainers(rootCtx, cli, processed, "/"); processErr != nil {
		log.Warn("could not enumerate existing containers", "err", processErr)
	}

	startReloadWatcher(rootCtx, cli)

	listenEvents(rootCtx, cli, processed)

	log.Info("device-mapping-manager shutting down")

	return 0
}

// processExistingContainers iterates the currently running containers and
// applies device rules to each one that bind-mounts /dev/... paths. Fixes
// the upstream behavior of only reacting to future "start" events.
func processExistingContainers(
	ctx context.Context,
	cli *client.Client,
	processed map[string]struct{},
	procRootPath string,
) error {
	log := logger.L()

	containers, err := cli.ContainerList(ctx, container.ListOptions{})
	if err != nil {
		return fmt.Errorf("list containers: %w", err)
	}

	log.Debug("enumerating running containers", "count", len(containers))

	for _, runningContainer := range containers {
		processErr := processContainer(ctx, cli, runningContainer.ID, procRootPath)
		if processErr != nil {
			log.Warn("could not process running container",
				"id", runningContainer.ID, "err", processErr)

			continue
		}

		processed[runningContainer.ID] = struct{}{}
	}

	return nil
}

// startReloadWatcher tries to subscribe to systemd's DBus Reloading signal so
// that when daemon-reload wipes the cgroup BPF programs, we re-apply rules to
// every running container. DBus is optional — on hosts without systemd or
// without the DBus socket mounted, this logs a warning and returns.
func startReloadWatcher(ctx context.Context, cli *client.Client) {
	log := logger.L()

	watcher, err := systemd.Open()
	if err != nil {
		log.Warn("systemd reload handling disabled", "err", err)

		return
	}

	go func() {
		defer func() {
			if closeErr := watcher.Close(); closeErr != nil {
				log.Warn("close systemd watcher", "err", closeErr)
			}
		}()

		watcher.Watch(ctx, func() {
			fresh := make(map[string]struct{})
			if processErr := processExistingContainers(ctx, cli, fresh, "/"); processErr != nil {
				log.Warn("could not re-apply rules after systemd reload",
					"err", processErr)
			}
		})
	}()
}

// listenEvents subscribes to Docker container events and applies device rules.
// "start" covers fresh starts and restart's second phase; "unpause" covers
// resume from a paused state if the cgroup state was cleared. On stream error
// it reconnects with exponential backoff (capped) rather than terminating the
// daemon — replaces the upstream log.Fatal(err) pattern.
func listenEvents(
	ctx context.Context,
	cli *client.Client,
	processed map[string]struct{},
) {
	log := logger.L()
	backoff := minBackoff

	eventFilters := filters.NewArgs(
		filters.Arg("event", "start"),
		filters.Arg("event", "unpause"),
	)

	for {
		if ctx.Err() != nil {
			return
		}

		msgs, errs := cli.Events(
			ctx,
			events.ListOptions{Filters: eventFilters},
		)

		log.Debug("subscribed to docker events")

		disconnected := consumeEvents(ctx, msgs, errs, processed, &backoff,
			func(ctx context.Context, id string) error {
				return processContainer(ctx, cli, id, "/")
			})
		if !disconnected {
			return
		}
	}
}

// applyFn is the per-container rule-application callback injected into consumeEvents.
// In production this wraps processContainer; in tests it is replaced by a fake.
type applyFn func(ctx context.Context, id string) error

// consumeEvents drains one events.Subscribe lifecycle. Returns true if the
// caller should reconnect, false on context cancellation.
func consumeEvents(
	ctx context.Context,
	msgs <-chan events.Message,
	errs <-chan error,
	processed map[string]struct{},
	backoff *time.Duration,
	apply applyFn,
) bool {
	log := logger.L()

	for {
		select {
		case <-ctx.Done():
			return false

		case streamErr := <-errs:
			if streamErr == nil {
				continue
			}

			if errors.Is(streamErr, context.Canceled) ||
				errors.Is(streamErr, context.DeadlineExceeded) {
				return false
			}

			log.Error("docker events stream error, reconnecting",
				"err", streamErr, "backoff", *backoff)
			sleepCtx(ctx, *backoff)
			*backoff = nextBackoff(*backoff)

			return ctx.Err() == nil

		case msg, ok := <-msgs:
			if !ok {
				log.Warn("docker events channel closed, reconnecting",
					"backoff", *backoff)
				sleepCtx(ctx, *backoff)
				*backoff = nextBackoff(*backoff)

				return ctx.Err() == nil
			}

			*backoff = minBackoff

			if _, alreadyProcessed := processed[msg.Actor.ID]; alreadyProcessed {
				delete(processed, msg.Actor.ID)

				continue
			}

			if applyErr := apply(ctx, msg.Actor.ID); applyErr != nil {
				log.Warn("could not process container",
					"id", msg.Actor.ID, "err", applyErr)
			}
		}
	}
}

func nextBackoff(current time.Duration) time.Duration {
	next := current * 2
	if next > maxBackoff {
		return maxBackoff
	}

	return next
}

func sleepCtx(ctx context.Context, duration time.Duration) {
	select {
	case <-ctx.Done():
	case <-time.After(duration):
	}
}

// processContainer inspects a container and applies cgroup BPF device-allow
// rules for every bind mount sourced from /dev/...
// procRootPath is the root used for /proc lookups; it is "/" in production and
// a temp-dir fixture root in tests.
func processContainer(
	ctx context.Context,
	cli containerInspector,
	id string,
	procRootPath string,
) error {
	log := logger.L()

	info, inspectErr := cli.ContainerInspect(ctx, id)
	if inspectErr != nil {
		return fmt.Errorf("inspect container %q: %w", id, inspectErr)
	}

	if info.State == nil || info.State.Pid == 0 {
		log.Debug("container has no live pid; skipping", "id", id)

		return nil
	}

	pid := info.State.Pid

	cgroupVersion, versionErr := cgroup.GetDeviceCGroupVersion(procRootPath, pid)
	if versionErr != nil {
		return fmt.Errorf("detect cgroup version for pid %d: %w", pid, versionErr)
	}

	log.Debug("cgroup version detected", "pid", pid, "version", cgroupVersion)

	api, apiErr := cgroup.New(cgroupVersion)
	if apiErr != nil {
		return fmt.Errorf("init cgroup api (version=%d): %w", cgroupVersion, apiErr)
	}

	cgroupPath, sysfsPath, mountErr := api.GetDeviceCGroupMountPath(procRootPath, pid)
	if mountErr != nil {
		return fmt.Errorf("resolve cgroup mount path: %w", mountErr)
	}

	cgroupPath = filepath.Join(hostRootPath, sysfsPath, cgroupPath)
	log.Debug("cgroup path resolved", "pid", pid, "path", cgroupPath)

	for _, mount := range info.Mounts {
		if !strings.HasPrefix(mount.Source, "/dev") {
			continue
		}

		log.Debug("device mount detected",
			"id", id,
			"pid", pid,
			"source", mount.Source,
			"destination", mount.Destination,
		)

		applyErr := applyMount(api, mount.Source, cgroupPath, pid)
		if applyErr != nil {
			log.Warn("could not apply device rule for mount",
				"source", mount.Source,
				"err", applyErr,
			)
		}
	}

	return nil
}

// applyMount applies a device rule to a single file mount, or walks the
// directory and applies rules to every contained device file.
func applyMount(api cgroup.Interface, mountPath, cgroupPath string, pid int) error {
	fileInfo, statErr := os.Stat(mountPath)
	if statErr != nil {
		return fmt.Errorf("stat %q: %w", mountPath, statErr)
	}

	if !fileInfo.IsDir() {
		return applyDeviceRules(api, mountPath, cgroupPath, pid)
	}

	walkErr := filepath.Walk(mountPath, func(walkedPath string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}

		if info.IsDir() {
			return nil
		}

		ruleErr := applyDeviceRules(api, walkedPath, cgroupPath, pid)
		if ruleErr != nil {
			logger.L().Warn("could not apply device rule",
				"path", walkedPath,
				"err", ruleErr,
			)
		}

		return nil
	})
	if walkErr != nil {
		return fmt.Errorf("walk %q: %w", mountPath, walkErr)
	}

	return nil
}

func applyDeviceRules(api cgroup.Interface, mountPath, cgroupPath string, pid int) error {
	log := logger.L()

	deviceType, major, minor, infoErr := getDeviceInfo(mountPath)
	if infoErr != nil {
		return infoErr
	}

	log.Debug("adding device rule",
		"pid", pid,
		"cgroup", cgroupPath,
		"type", deviceType,
		"major", major,
		"minor", minor,
	)

	ruleErr := api.AddDeviceRules(cgroupPath, []cgroup.DeviceRule{
		{
			Access: "rwm",
			Major:  ptr(major),
			Minor:  ptr(minor),
			Type:   deviceType,
			Allow:  true,
		},
	})
	if ruleErr != nil {
		return fmt.Errorf("add device rule: %w", ruleErr)
	}

	return nil
}

func getDeviceInfo(devicePath string) (string, int64, int64, error) {
	var stat unix.Stat_t

	if statErr := unix.Stat(devicePath, &stat); statErr != nil {
		return "", -1, -1, fmt.Errorf("stat %q: %w", devicePath, statErr)
	}

	var deviceType string

	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFBLK:
		deviceType = "b"
	case unix.S_IFCHR:
		deviceType = "c"
	default:
		return "", -1, -1, fmt.Errorf("device %q is neither character nor block device", devicePath)
	}

	major := int64(unix.Major(stat.Rdev))
	minor := int64(unix.Minor(stat.Rdev))

	return deviceType, major, minor, nil
}
