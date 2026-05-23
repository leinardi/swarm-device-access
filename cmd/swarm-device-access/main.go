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

// Package main wires and runs the swarm-device-access daemon.
// It listens to Docker container-start events and injects cgroup v2 BPF
// device-allow rules for any container that bind-mounts a /dev/... path.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"net/http"
	"net/http/pprof"
	"os"
	"os/signal"
	"path/filepath"
	"strings"
	"sync/atomic"
	"syscall"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/filters"
	"github.com/docker/docker/client"
	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/systemd"
	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/promauto"
	"github.com/prometheus/client_golang/prometheus/promhttp"
	"golang.org/x/sys/unix"
	"gopkg.in/yaml.v3"
)

// containerInspector is the subset of *client.Client used by processContainer.
// It exists solely to allow unit tests to inject a fake without standing up a
// real Docker daemon.
type containerInspector interface {
	ContainerInspect(ctx context.Context, id string) (container.InspectResponse, error)
}

const (
	// hostRootPath is where the host's "/" is expected to be mounted inside
	// the daemon container. Setting cgroup BPF programs requires writing to
	// /sys/fs/cgroup of the host, so the daemon needs the host root visible
	// to it. See the example docker-compose for the bind mount layout.
	hostRootPath = "/host"

	minBackoff      = 1 * time.Second
	maxBackoff      = 30 * time.Second
	shutdownTimeout = 5 * time.Second
)

// stringSliceFlag is a repeatable flag: -device-allow /dev/nvidia* -device-allow /dev/dri/*.
type stringSliceFlag []string

func (f *stringSliceFlag) String() string { return strings.Join(*f, ",") }
func (f *stringSliceFlag) Set(v string) error {
	*f = append(*f, v)

	return nil
}

func (f *stringSliceFlag) contains(path string) bool {
	for _, pattern := range *f {
		if ok, _ := filepath.Match(pattern, path); ok {
			return true
		}
	}

	return false
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
	requireLabel = flag.String(
		"require-label",
		"",
		"Only process containers that have this label (format: key=value). Empty means all containers.",
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

// configFileSchema mirrors the CLI flags that can be set via the config file.
// All fields are optional; zero values mean "not set in file".
// YAML tag names match the CLI flag names (kebab-case) for user-facing consistency.
//
//nolint:tagliatelle // kebab-case tags match CLI flag names intentionally for user-facing consistency
type configFileSchema struct {
	LogFormat    string   `yaml:"log-format"`
	LogLevel     string   `yaml:"log-level"`
	LogTime      *bool    `yaml:"log-time"`
	DockerSocket string   `yaml:"docker-socket"`
	DryRun       *bool    `yaml:"dry-run"`
	RequireLabel string   `yaml:"require-label"`
	DeviceAllow  []string `yaml:"device-allow"`
	DeviceDeny   []string `yaml:"device-deny"`
	MetricsAddr  string   `yaml:"metrics-addr"`
	DebugAddr    string   `yaml:"debug-addr"`
}

// hotConfig holds the settings that can be changed without restarting the daemon.
// Readers call activeCfg.Load(); the SIGHUP handler calls activeCfg.Store().
type hotConfig struct {
	dryRun       bool
	requireLabel string
	deviceAllow  stringSliceFlag
	deviceDeny   stringSliceFlag
}

// activeCfg is the live, hot-reloadable configuration snapshot.
// Written once at startup and again on each SIGHUP.
var activeCfg atomic.Pointer[hotConfig]

// loadConfigFile parses the YAML config file at path and returns its contents.
// Returns a zero-value schema (no error) if path is empty.
func loadConfigFile(path string) (configFileSchema, error) {
	if path == "" {
		return configFileSchema{}, nil
	}

	data, err := os.ReadFile(path)
	if err != nil {
		return configFileSchema{}, fmt.Errorf("read config file %q: %w", path, err)
	}

	var cfg configFileSchema

	err = yaml.Unmarshal(data, &cfg)
	if err != nil {
		return configFileSchema{}, fmt.Errorf("parse config file %q: %w", path, err)
	}

	return cfg, nil
}

// Prometheus metrics — registered once at startup via promauto.
var (
	metricEventsTotal = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "sda_events_total",
		Help: "Total Docker container events received.",
	}, []string{"event"})

	metricRulesApplied = promauto.NewCounterVec(prometheus.CounterOpts{
		Name: "sda_rules_applied_total",
		Help: "Total device rules applied (or skipped in dry-run).",
	}, []string{"result"})

	metricReloadReapplies = promauto.NewCounter(prometheus.CounterOpts{
		Name: "sda_reload_reapplies_total",
		Help: "Times device rules were re-applied after a systemd daemon-reload.",
	})

	metricDockerReconnects = promauto.NewCounter(prometheus.CounterOpts{
		Name: "sda_docker_reconnects_total",
		Help: "Times the Docker event stream reconnected after an error.",
	})

	metricApplyDuration = promauto.NewHistogram(prometheus.HistogramOpts{
		Name:    "sda_apply_duration_seconds",
		Help:    "Time spent applying device rules per container.",
		Buckets: prometheus.DefBuckets,
	})
)

// ptr returns a pointer to v. Generic helper used to inline scalar pointers
// for cgroup.DeviceRule fields.
//
//nolint:modernize // ptr takes address of value; new(T) allocates zero-value
func ptr[T any](v T) *T {
	return &v
}

//nolint:gochecknoinits // flag registration requires init
func init() {
	flag.Var(&deviceAllow, "device-allow",
		"Glob pattern for /dev/... paths to allow (repeatable). Empty means allow all.")
	flag.Var(&deviceDeny, "device-deny",
		"Glob pattern for /dev/... paths to deny (repeatable; takes priority over -device-allow).")
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

	// Load config file; CLI flags override file values.
	fileCfg, fileErr := loadConfigFile(*configFile)
	if fileErr != nil {
		fmt.Fprintf(os.Stderr, "config file error: %v\n", fileErr)

		return 1
	}

	applyFileConfig(&fileCfg)

	logger.Configure(*logFormat, *logLevel, *logTime)

	log := logger.L()

	log.Info("swarm-device-access starting",
		"version", version,
		"commit", commit,
		"date", date,
		"config_file", *configFile,
		"dry_run", activeCfg.Load().dryRun,
		"require_label", activeCfg.Load().requireLabel,
		"device_allow", []string(activeCfg.Load().deviceAllow),
		"device_deny", []string(activeCfg.Load().deviceDeny),
	)

	rootCtx, cancelRoot := signal.NotifyContext(
		context.Background(),
		syscall.SIGINT,
		syscall.SIGTERM,
	)
	defer cancelRoot()

	// SIGHUP: reload the config file and update hot settings + logger.
	go watchSIGHUP(rootCtx)

	cli, err := client.NewClientWithOpts(
		client.WithHost("unix://"+*dockerSocket),
		client.WithAPIVersionNegotiation(),
	)
	if err != nil {
		log.Error("docker client init failed", "err", err)

		return 1
	}
	defer cli.Close()

	// Start optional observability servers before the main loop so they are
	// reachable during startup enumeration.
	readyCh := make(chan struct{})

	if *metricsAddr != "" {
		startMetricsServer(rootCtx, *metricsAddr, readyCh)
	}

	if *debugAddr != "" {
		startDebugServer(rootCtx, *debugAddr)
	}

	// Apply rules to containers that are already running when we start up.
	// Track their IDs so the start-event handler can skip them on the brief
	// window where both events and the initial enumeration overlap.
	processed := make(map[string]struct{})

	processErr := processExistingContainers(
		rootCtx,
		cli,
		processed,
		"/",
		activeCfg.Load().dryRun,
	)
	if processErr != nil {
		log.Warn("could not enumerate existing containers", "err", processErr)
	}

	// Signal readiness: startup enumeration complete, event loop about to start.
	close(readyCh)

	startReloadWatcher(rootCtx, cli)

	listenEvents(rootCtx, cli, processed)

	log.Info("swarm-device-access shutting down")

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
	dryRun bool,
) error {
	log := logger.L()

	containers, err := cli.ContainerList(ctx, container.ListOptions{})
	if err != nil {
		return fmt.Errorf("list containers: %w", err)
	}

	log.Debug("enumerating running containers", "count", len(containers))

	for idx := range containers {
		processErr := processContainer(ctx, cli, containers[idx].ID, procRootPath, dryRun)
		if processErr != nil {
			log.Warn("could not process running container",
				"id", containers[idx].ID, "err", processErr)

			continue
		}

		processed[containers[idx].ID] = struct{}{}
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
			closeErr := watcher.Close()
			if closeErr != nil {
				log.Warn("close systemd watcher", "err", closeErr)
			}
		}()

		watcher.Watch(ctx, func() {
			metricReloadReapplies.Inc()

			fresh := make(map[string]struct{})

			processErr := processExistingContainers(
				ctx,
				cli,
				fresh,
				"/",
				activeCfg.Load().dryRun,
			)
			if processErr != nil {
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

	// The processed map guards the overlap window between startup enumeration
	// and the live event stream. After 2×maxBackoff (60s) the window has
	// certainly passed; any remaining entries are from containers that exited
	// before producing a start event and will never be drained normally.
	clearProcessed := time.After(2 * maxBackoff)

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

		disconnected := consumeEvents(ctx, msgs, errs, processed, &backoff, clearProcessed,
			func(ctx context.Context, id string) error {
				return processContainer(ctx, cli, id, "/", activeCfg.Load().dryRun)
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
	clearProcessed <-chan time.Time,
	apply applyFn,
) bool {
	log := logger.L()

	for {
		select {
		case <-ctx.Done():
			return false

		case <-clearProcessed:
			// Overlap window expired; discard any startup entries that were
			// never matched by a live event (containers that exited during the
			// window). Reassign to a nil channel so the case never fires again.
			for k := range processed {
				delete(processed, k)
			}

			clearProcessed = nil

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
			metricDockerReconnects.Inc()
			sleepCtx(ctx, *backoff)
			*backoff = nextBackoff(*backoff)

			return ctx.Err() == nil

		case msg, ok := <-msgs:
			if !ok {
				log.Warn("docker events channel closed, reconnecting",
					"backoff", *backoff)
				metricDockerReconnects.Inc()
				sleepCtx(ctx, *backoff)
				*backoff = nextBackoff(*backoff)

				return ctx.Err() == nil
			}

			*backoff = minBackoff

			metricEventsTotal.WithLabelValues(string(msg.Action)).Inc()

			if _, alreadyProcessed := processed[msg.Actor.ID]; alreadyProcessed {
				delete(processed, msg.Actor.ID)

				continue
			}

			start := time.Now()

			applyErr := apply(ctx, msg.Actor.ID)
			if applyErr != nil {
				log.Warn("could not process container",
					"id", msg.Actor.ID, "err", applyErr)
				metricRulesApplied.WithLabelValues("error").Inc()
			} else {
				metricRulesApplied.WithLabelValues("ok").Inc()
			}

			metricApplyDuration.Observe(time.Since(start).Seconds())
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

// applyFileConfig merges the file config into the CLI flags (for flags not
// explicitly set by the user) and stores the result in activeCfg.
// Must be called after flag.Parse().
//
//nolint:gocyclo,cyclop // complexity is inherent: merges many independent optional config fields
func applyFileConfig(fileCfg *configFileSchema) {
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

	if !cliSet["require-label"] && fileCfg.RequireLabel != "" {
		*requireLabel = fileCfg.RequireLabel
	}

	if !cliSet["device-allow"] && len(fileCfg.DeviceAllow) > 0 {
		deviceAllow = fileCfg.DeviceAllow
	}

	if !cliSet["device-deny"] && len(fileCfg.DeviceDeny) > 0 {
		deviceDeny = fileCfg.DeviceDeny
	}

	activeCfg.Store(&hotConfig{
		dryRun:       *dryRun,
		requireLabel: *requireLabel,
		deviceAllow:  append(stringSliceFlag(nil), deviceAllow...),
		deviceDeny:   append(stringSliceFlag(nil), deviceDeny...),
	})
}

// watchSIGHUP blocks until ctx is done, reloading the config file and
// updating activeCfg + logger on each SIGHUP. Settings that require a
// restart (docker-socket, metrics-addr, debug-addr) are not reloaded.
//
//nolint:gocyclo,cyclop // complexity is inherent: handles SIGHUP, config reload, logger update, and reload actions
func watchSIGHUP(ctx context.Context) {
	sigCh := make(chan os.Signal, 1)

	signal.Notify(sigCh, syscall.SIGHUP)
	defer signal.Stop(sigCh)

	for {
		select {
		case <-ctx.Done():
			return

		case <-sigCh:
			log := logger.L()
			log.Info("SIGHUP received; reloading config", "config_file", *configFile)

			fileCfg, err := loadConfigFile(*configFile)
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
			newRequireLabel := *requireLabel

			newDeviceAllow := append(stringSliceFlag(nil), deviceAllow...)
			newDeviceDeny := append(stringSliceFlag(nil), deviceDeny...)

			if !cliSet["dry-run"] && fileCfg.DryRun != nil {
				newDryRun = *fileCfg.DryRun
			}

			if !cliSet["require-label"] && fileCfg.RequireLabel != "" {
				newRequireLabel = fileCfg.RequireLabel
			}

			if !cliSet["device-allow"] && len(fileCfg.DeviceAllow) > 0 {
				newDeviceAllow = fileCfg.DeviceAllow
			}

			if !cliSet["device-deny"] && len(fileCfg.DeviceDeny) > 0 {
				newDeviceDeny = fileCfg.DeviceDeny
			}

			activeCfg.Store(&hotConfig{
				dryRun:       newDryRun,
				requireLabel: newRequireLabel,
				deviceAllow:  newDeviceAllow,
				deviceDeny:   newDeviceDeny,
			})

			logger.L().Info("config reloaded",
				"dry_run", newDryRun,
				"require_label", newRequireLabel,
				"device_allow", []string(newDeviceAllow),
				"device_deny", []string(newDeviceDeny),
			)
		}
	}
}

// startMetricsServer starts an HTTP server on addr exposing:
//
//	/metrics  — Prometheus text format
//	/healthz  — 200 OK always (liveness)
//	/readyz   — 200 OK once ready is closed (readiness)
func startMetricsServer(ctx context.Context, addr string, ready <-chan struct{}) {
	log := logger.L()
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.Handler())
	mux.HandleFunc("/healthz", func(w http.ResponseWriter, _ *http.Request) {
		w.WriteHeader(http.StatusOK)
		_, _ = w.Write([]byte("ok"))
	})
	mux.HandleFunc("/readyz", func(resp http.ResponseWriter, _ *http.Request) {
		select {
		case <-ready:
			resp.WriteHeader(http.StatusOK)
			_, _ = resp.Write([]byte("ok"))
		default:
			resp.WriteHeader(http.StatusServiceUnavailable)
			_, _ = resp.Write([]byte("starting"))
		}
	})

	srv := &http.Server{
		Addr:              addr,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		log.Info("metrics server listening", "addr", addr)

		err := srv.ListenAndServe()
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("metrics server error", "err", err)
		}
	}()

	go func() {
		<-ctx.Done()

		shutCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), shutdownTimeout)
		defer cancel()

		err := srv.Shutdown(
			shutCtx,
		)
		if err != nil {
			log.Warn("metrics server shutdown error", "err", err)
		}
	}()
}

// startDebugServer starts an HTTP server on addr exposing pprof endpoints.
func startDebugServer(ctx context.Context, addr string) {
	log := logger.L()
	mux := http.NewServeMux()
	mux.HandleFunc("/debug/pprof/", pprof.Index)
	mux.HandleFunc("/debug/pprof/cmdline", pprof.Cmdline)
	mux.HandleFunc("/debug/pprof/profile", pprof.Profile)
	mux.HandleFunc("/debug/pprof/symbol", pprof.Symbol)
	mux.HandleFunc("/debug/pprof/trace", pprof.Trace)

	srv := &http.Server{
		Addr:              addr,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		log.Info("debug server listening", "addr", addr)

		err := srv.ListenAndServe()
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("debug server error", "err", err)
		}
	}()

	go func() {
		<-ctx.Done()

		shutCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), shutdownTimeout)
		defer cancel()

		err := srv.Shutdown(
			shutCtx,
		)
		if err != nil {
			log.Warn("debug server shutdown error", "err", err)
		}
	}()
}

// containerMatchesLabelPolicy returns true if the container should be processed.
// When requireLabel is empty every container passes. Otherwise the container
// must have a label matching "key=value".
func containerMatchesLabelPolicy(labels map[string]string, policy string) bool {
	if policy == "" {
		return true
	}

	key, value, _ := strings.Cut(policy, "=")

	return labels[key] == value
}

// devicePathAllowed returns true when the /dev/... path is permitted by the
// global allow/deny lists. Deny takes priority over allow. An empty allow list
// means "allow everything".
func devicePathAllowed(path string, allow, deny stringSliceFlag) bool {
	if deny.contains(path) {
		return false
	}

	if len(allow) == 0 {
		return true
	}

	return allow.contains(path)
}

// processContainer inspects a container and applies cgroup BPF device-allow
// rules for every bind mount sourced from /dev/...
// procRootPath is the root used for /proc lookups; "/" in production, temp dir in tests.
// dryRun skips cgroup writes and logs intent at Info level instead.
func processContainer(
	ctx context.Context,
	cli containerInspector,
	containerID string,
	procRootPath string,
	dryRun bool,
) error {
	log := logger.L()

	info, inspectErr := cli.ContainerInspect(ctx, containerID)
	if inspectErr != nil {
		return fmt.Errorf("inspect container %q: %w", containerID, inspectErr)
	}

	if info.State == nil || info.State.Pid == 0 {
		log.Debug("container has no live pid; skipping", "id", containerID)

		return nil
	}

	// Label policy check: skip containers that don't carry the required label.
	var labels map[string]string
	if info.Config != nil {
		labels = info.Config.Labels
	}

	cfg := activeCfg.Load()

	if !containerMatchesLabelPolicy(labels, cfg.requireLabel) {
		log.Debug("container skipped by label policy",
			"id", containerID,
			"require_label", cfg.requireLabel,
		)

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
			"id", containerID,
			"pid", pid,
			"source", mount.Source,
			"destination", mount.Destination,
		)

		applyErr := applyMount(api, mount.Source, cgroupPath, pid, dryRun)
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
func applyMount(api cgroup.Interface, mountPath, cgroupPath string, pid int, dryRun bool) error {
	cfg := activeCfg.Load()
	if !devicePathAllowed(mountPath, cfg.deviceAllow, cfg.deviceDeny) {
		logger.L().Debug("device mount excluded by policy", "path", mountPath)

		return nil
	}

	fileInfo, statErr := os.Stat(mountPath)
	if statErr != nil {
		return fmt.Errorf("stat %q: %w", mountPath, statErr)
	}

	if !fileInfo.IsDir() {
		return applyDeviceRules(api, mountPath, cgroupPath, pid, dryRun)
	}

	walkErr := filepath.Walk(mountPath, func(walkedPath string, info os.FileInfo, err error) error {
		if err != nil {
			return err
		}

		if info.IsDir() {
			return nil
		}

		if !devicePathAllowed(walkedPath, cfg.deviceAllow, cfg.deviceDeny) {
			logger.L().Debug("device file excluded by policy", "path", walkedPath)

			return nil
		}

		ruleErr := applyDeviceRules(api, walkedPath, cgroupPath, pid, dryRun)
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

func applyDeviceRules(
	api cgroup.Interface,
	mountPath, cgroupPath string,
	pid int,
	dryRun bool,
) error {
	log := logger.L()

	deviceType, major, minor, infoErr := getDeviceInfo(mountPath)
	if infoErr != nil {
		return infoErr
	}

	if dryRun {
		log.Info("dry-run: would add device rule",
			"pid", pid,
			"cgroup", cgroupPath,
			"type", deviceType,
			"major", major,
			"minor", minor,
		)

		return nil
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
			Major:  &major,
			Minor:  &minor,
			Type:   deviceType,
			Allow:  true,
		},
	})
	if ruleErr != nil {
		return fmt.Errorf("add device rule: %w", ruleErr)
	}

	return nil
}

func getDeviceInfo(devicePath string) (deviceType string, major, minor int64, statErr error) {
	var stat unix.Stat_t

	statErr = unix.Stat(devicePath, &stat)
	if statErr != nil {
		return "", -1, -1, fmt.Errorf("stat %q: %w", devicePath, statErr)
	}

	switch stat.Mode & unix.S_IFMT {
	case unix.S_IFBLK:
		deviceType = "b"
	case unix.S_IFCHR:
		deviceType = "c"
	default:
		return "", -1, -1, fmt.Errorf( //nolint:err113 // dynamic content includes device path
			"device %q is neither character nor block device",
			devicePath,
		)
	}

	major = int64(unix.Major(stat.Rdev))
	minor = int64(unix.Minor(stat.Rdev))

	return deviceType, major, minor, nil
}
