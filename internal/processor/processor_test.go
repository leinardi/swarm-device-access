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
	"bytes"
	"context"
	"errors"
	"log/slog"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"
	"github.com/moby/moby/api/types/swarm"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// fakeInspector is a test double for DockerInspector.
type fakeInspector struct {
	result        container.InspectResponse
	err           error
	serviceResult swarm.Service
	serviceCalls  int
}

func (f *fakeInspector) ContainerInspect(
	_ context.Context,
	_ string,
	_ client.ContainerInspectOptions,
) (client.ContainerInspectResult, error) {
	return client.ContainerInspectResult{Container: f.result}, f.err
}

// ServiceInspect is not part of DockerInspector. It stays on the fake, with
// a call counter, so a test can prove the processor never reaches for the
// parent service even if the interface were widened again.
func (f *fakeInspector) ServiceInspect(
	_ context.Context,
	_ string,
	_ client.ServiceInspectOptions,
) (client.ServiceInspectResult, error) {
	f.serviceCalls++

	return client.ServiceInspectResult{Service: f.serviceResult}, nil
}

// testCgroupContent and testMountinfoContent describe a cgroup v2 container,
// the only layout the processor tests need.
const (
	testCgroupContent    = "0::/docker/testcontainer\n"
	testMountinfoContent = "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format
)

// buildProcRoot creates a minimal /proc/<pid>/{cgroup,mountinfo} structure
// under a temp dir so Reconcile can resolve the cgroup path without a
// real /proc filesystem.
//

func buildProcRoot(
	t *testing.T,
	pid int,
) string {
	t.Helper()

	root := t.TempDir()
	procDir := filepath.Join(root, "proc", strconv.Itoa(pid))

	err := os.MkdirAll(procDir, 0o755)
	if err != nil {
		t.Fatalf("mkdir proc: %v", err)
	}

	err = os.WriteFile(
		filepath.Join(procDir, "cgroup"),
		[]byte(testCgroupContent),
		0o600,
	)
	if err != nil {
		t.Fatalf("write cgroup: %v", err)
	}

	err = os.WriteFile(
		filepath.Join(procDir, "mountinfo"),
		[]byte(testMountinfoContent),
		0o600,
	)
	if err != nil {
		t.Fatalf("write mountinfo: %v", err)
	}

	return root
}

var errDaemonUnavail = errors.New("daemon unavailable")

func newStore(mode policy.Mode, dryRun bool) *config.Store {
	store, _ := newPublishedStore(mode, dryRun)

	return store
}

func newPublishedStore(mode policy.Mode, dryRun bool) (*config.Store, *config.Publisher) {
	return config.NewStore(config.Runtime{Policy: policy.Global{Mode: mode}, DryRun: dryRun})
}

func TestIsDeviceMountSource(t *testing.T) {
	t.Parallel()

	tests := []struct {
		path string
		want bool
	}{
		{path: "/dev", want: true},
		{path: "/dev/null", want: true},
		{path: "/dev/bus/usb", want: true},
		{path: "/devops/null", want: false},
		{path: "/development/null", want: false},
		{path: "/tmp/dev/null", want: false},
		{path: "", want: false},
	}

	for _, tc := range tests {
		t.Run(tc.path, func(t *testing.T) {
			t.Parallel()

			got := IsMountSource(tc.path)
			if got != tc.want {
				t.Errorf("IsMountSource(%q) = %v, want %v", tc.path, got, tc.want)
			}
		})
	}
}

func TestHostCGroupPath(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name         string
		sysfsPath    string
		cgroupPrefix string
		cgroupRoot   string
		want         string
	}{
		{
			name:         "host cgroup namespace",
			sysfsPath:    "/sys/fs/cgroup",
			cgroupPrefix: "/",
			cgroupRoot:   "/system.slice/docker-abc.scope",
			want:         "/host/sys/fs/cgroup/system.slice/docker-abc.scope",
		},
		{
			name:         "private cgroup namespace",
			sysfsPath:    "/sys/fs/cgroup",
			cgroupPrefix: "/system.slice/docker-abc.scope",
			cgroupRoot:   "/",
			want:         "/host/sys/fs/cgroup/system.slice/docker-abc.scope",
		},
		{
			name:         "mount prefix trimmed from proc cgroup",
			sysfsPath:    "/sys/fs/cgroup",
			cgroupPrefix: "/docker",
			cgroupRoot:   "/abc",
			want:         "/host/sys/fs/cgroup/docker/abc",
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got := hostCGroupPath("/host", tc.sysfsPath, tc.cgroupPrefix, tc.cgroupRoot)
			if got != tc.want {
				t.Errorf("hostCGroupPath() = %q, want %q", got, tc.want)
			}
		})
	}
}

// ---- Reconcile tests ----

func TestReconcile_InspectError(t *testing.T) {
	insp := &fakeInspector{err: errDaemonUnavail}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  "/",
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err == nil {
		t.Fatal("expected error from inspect failure, got nil")
	}
}

func TestReconcile_NilState(t *testing.T) {
	insp := &fakeInspector{result: container.InspectResponse{
		State: nil,
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  "/",
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for nil state, got %v", err)
	}
}

func TestReconcile_ZeroPid(t *testing.T) {
	state := &container.State{Running: true, Pid: 0}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  "/",
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for pid=0, got %v", err)
	}
}

func TestReconcile_NoDevMounts(t *testing.T) {
	const pid = 42

	root := buildProcRoot(t, pid)

	state := &container.State{Running: true, Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
		Mounts: []container.MountPoint{
			{Source: "/tmp/data", Destination: "/data", Type: mount.TypeBind},
			{Source: "/var/log", Destination: "/logs", Type: mount.TypeBind},
		},
	}}
	fake := &failingCgroup{}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  hostRootWithCgroup(t, pid),
		ProcRoot:  root,
		pinner:    &fakePinner{},
		newCgroup: func(int, *cgroup.Ledger, *cgroup.FilterCache) (cgroup.Interface, error) {
			return fake, nil
		},
	}

	buf := captureLogger(t)

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for container with no /dev mounts, got %v", err)
	}

	if fake.calls != 1 || len(fake.rules[0]) != 0 {
		t.Errorf(
			"SetDeviceRules calls = %d with %v, want one call with the empty set",
			fake.calls,
			fake.rules,
		)
	}

	if strings.Contains(buf.String(), "container processed") {
		t.Errorf("expected no summary for container without /dev mounts, got: %s", buf.String())
	}
}

func TestReconcile_DevMountFilterApplied(t *testing.T) {
	const pid = 43

	root := buildProcRoot(t, pid)

	state := &container.State{Running: true, Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
		Mounts: []container.MountPoint{
			{Source: "/tmp/data", Destination: "/data", Type: mount.TypeBind},
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  root,
		pinner:    &fakePinner{},
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err == nil {
		t.Fatal(
			"Reconcile should return error when the cgroup path cannot be opened",
		)
	}
}

func TestReconcile_DevMount_DryRunNoError(t *testing.T) {
	const pid = 44

	root := buildProcRoot(t, pid)

	state := &container.State{Running: true, Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, true),
		HostRoot:  "/host",
		ProcRoot:  root,
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("dry-run Reconcile should not error: %v", err)
	}
}

func TestReconcile_DeduplicatesDuplicateMounts(t *testing.T) {
	const pid = 45

	root := buildProcRoot(t, pid)

	state := &container.State{Running: true, Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
			{Source: "/dev/null", Destination: "/dev/null2", Type: mount.TypeBind},
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, true),
		HostRoot:  "/host",
		ProcRoot:  root,
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("dry-run with duplicate mounts should not error: %v", err)
	}
}

func TestReconcile_OptInProcessesEnabled(t *testing.T) {
	const pid = 51

	root := buildProcRoot(t, pid)

	state := &container.State{Running: true, Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
		Config: &container.Config{
			Labels: map[string]string{policy.LabelEnable: "true"},
		},
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeOptIn, true),
		HostRoot:  "/host",
		ProcRoot:  root,
	}

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("opt-in dry-run with enable=true should not error: %v", err)
	}
}

// TestReconcile_DevDirectorySummary checks that a whole-/dev mount is
// processed without a container-level error and emits one INFO summary.
func TestReconcile_DevDirectorySummary(t *testing.T) {
	const pid = 46

	root := buildProcRoot(t, pid)

	state := &container.State{Running: true, Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		State: state,
		Mounts: []container.MountPoint{
			{Source: "/dev", Destination: "/dev", Type: mount.TypeBind},
		},
	}}

	store, _ := config.NewStore(config.Runtime{
		Policy: policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/null"}},
		DryRun: true,
	})

	proc := &Processor{
		Inspector: insp,
		Cfg:       store,
		HostRoot:  "/host",
		ProcRoot:  root,
	}

	buf := captureLogger(t)

	err := proc.Reconcile(context.Background(), "abc")
	if err != nil {
		t.Fatalf("Reconcile returned error: %v", err)
	}

	logOutput := buf.String()

	summaries := 0

	for line := range strings.SplitSeq(logOutput, "\n") {
		if !strings.Contains(line, `msg="container processed"`) {
			continue
		}

		summaries++

		if !strings.Contains(line, "level=INFO") ||
			!strings.Contains(line, "devices_granted=1") ||
			!strings.Contains(line, "errors=0") ||
			!strings.Contains(line, "dry_run=true") {
			t.Errorf("unexpected summary line: %s", line)
		}
	}

	if summaries != 1 {
		t.Errorf("expected exactly one summary, got %d: %s", summaries, logOutput)
	}
}

// captureLogger sets logger.L() to write to a buffer for the duration of the
// test and restores the previous logger when the test ends.
func captureLogger(t *testing.T) *bytes.Buffer {
	t.Helper()

	prev := logger.L()

	var buf bytes.Buffer
	logger.Set(slog.New(slog.NewTextHandler(&buf, &slog.HandlerOptions{
		Level: slog.LevelDebug,
	})))
	t.Cleanup(func() { logger.Set(prev) })

	return &buf
}

// TestReconcile_IgnoresServiceLevelLabels checks that policy comes
// from the container's own labels only: the parent service is never
// inspected, and a container whose only opt-in is a service-level (deploy.labels)
// label is skipped.
func TestReconcile_IgnoresServiceLevelLabels(t *testing.T) {
	buf := captureLogger(t)

	const pid = 51

	inspector := &fakeInspector{
		result: container.InspectResponse{
			State: &container.State{Running: true, Pid: pid},
			Config: &container.Config{Labels: map[string]string{
				"com.docker.swarm.service.id": "svc456",
			}},
		},
		serviceResult: swarm.Service{Spec: swarm.ServiceSpec{Annotations: swarm.Annotations{
			Name:   "my-service",
			Labels: map[string]string{policy.LabelEnable: "true"},
		}}},
	}

	proc := &Processor{
		Inspector: inspector,
		Cfg:       newStore(policy.ModeOptIn, true),
	}

	err := proc.Reconcile(context.Background(), "abc123")
	if err != nil {
		t.Fatalf("Reconcile: %v", err)
	}

	if inspector.serviceCalls != 0 {
		t.Errorf("ServiceInspect called %d times, want never", inspector.serviceCalls)
	}

	if !strings.Contains(buf.String(), "skipped by policy") {
		t.Errorf("service-level opt-in must not enable the container; log:\n%s", buf.String())
	}
}

func TestReconcile_WarnsOnUnknownContainerLabel(t *testing.T) {
	buf := captureLogger(t)

	inspector := &fakeInspector{
		result: container.InspectResponse{
			State: &container.State{Running: true, Pid: 51},
			Config: &container.Config{Labels: map[string]string{
				policy.LabelPrefix + "enabled": "true",
			}},
		},
	}

	proc := &Processor{Inspector: inspector, Cfg: newStore(policy.ModeOptIn, true)}

	err := proc.Reconcile(context.Background(), "abc123")
	if err != nil {
		t.Fatalf("Reconcile: %v", err)
	}

	if !strings.Contains(buf.String(), "unrecognized swarm-device-access label on container") {
		t.Errorf("expected unknown-label warning, got:\n%s", buf.String())
	}
}
