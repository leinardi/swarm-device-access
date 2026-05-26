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
	"maps"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/mount"
	"github.com/docker/docker/api/types/swarm"

	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// fakeInspector is a test double for DockerInspector.
type fakeInspector struct {
	result        container.InspectResponse
	err           error
	serviceResult swarm.Service
	serviceErr    error
	serviceCalls  int
}

func (f *fakeInspector) ContainerInspect(
	_ context.Context,
	_ string,
) (container.InspectResponse, error) {
	return f.result, f.err
}

func (f *fakeInspector) ServiceInspectWithRaw(
	_ context.Context,
	_ string,
	_ swarm.ServiceInspectOptions,
) (swarm.Service, []byte, error) {
	f.serviceCalls++

	return f.serviceResult, nil, f.serviceErr
}

// buildProcRoot creates a minimal /proc/<pid>/{cgroup,mountinfo} structure
// under a temp dir so ProcessContainer can resolve the cgroup path without a
// real /proc filesystem.
//

func buildProcRoot(
	t *testing.T,
	pid int,
	cgroupContent, mountinfoContent string,
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
		[]byte(cgroupContent),
		0o600,
	)
	if err != nil {
		t.Fatalf("write cgroup: %v", err)
	}

	err = os.WriteFile(
		filepath.Join(procDir, "mountinfo"),
		[]byte(mountinfoContent),
		0o600,
	)
	if err != nil {
		t.Fatalf("write mountinfo: %v", err)
	}

	return root
}

var errDaemonUnavail = errors.New("daemon unavailable")

func newStore(mode policy.Mode, dryRun bool) *config.Store {
	s := config.NewStore()
	s.Set(config.Runtime{Policy: policy.Global{Mode: mode}, DryRun: dryRun})

	return s
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

// ---- ProcessContainer tests ----

func TestProcessContainer_InspectError(t *testing.T) {
	insp := &fakeInspector{err: errDaemonUnavail}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  "/",
	}

	err := proc.ProcessContainer(context.Background(), "abc")
	if err == nil {
		t.Fatal("expected error from inspect failure, got nil")
	}
}

func TestProcessContainer_NilState(t *testing.T) {
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{
			State: nil,
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  "/",
	}

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for nil state, got %v", err)
	}
}

func TestProcessContainer_ZeroPid(t *testing.T) {
	state := &container.State{Pid: 0}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  "/",
	}

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for pid=0, got %v", err)
	}
}

func TestProcessContainer_NoDevMounts(t *testing.T) {
	const pid = 42

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
		Mounts: []container.MountPoint{
			{Source: "/tmp/data", Destination: "/data", Type: mount.TypeBind},
			{Source: "/var/log", Destination: "/logs", Type: mount.TypeBind},
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeAll, false),
		HostRoot:  "/host",
		ProcRoot:  root,
	}

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for container with no /dev mounts, got %v", err)
	}
}

func TestProcessContainer_DevMountFilterApplied(t *testing.T) {
	const pid = 43

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
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
	}

	err := proc.ProcessContainer(context.Background(), "abc")
	if err == nil {
		t.Fatal(
			"ProcessContainer should return error when AddDeviceRules fails on fake cgroup path",
		)
	}
}

func TestProcessContainer_DevMount_DryRunNoError(t *testing.T) {
	const pid = 44

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
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

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("dry-run ProcessContainer should not error: %v", err)
	}
}

func TestProcessContainer_DeduplicatesDuplicateMounts(t *testing.T) {
	const pid = 45

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
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

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("dry-run with duplicate mounts should not error: %v", err)
	}
}

func TestProcessContainer_OptInSkipsUnlabelled(t *testing.T) {
	const pid = 50

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}
	proc := &Processor{
		Inspector: insp,
		Cfg:       newStore(policy.ModeOptIn, false),
		HostRoot:  "/host",
		ProcRoot:  root,
	}

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("opt-in skip should return nil, got: %v", err)
	}
}

func TestProcessContainer_OptInProcessesEnabled(t *testing.T) {
	const pid = 51

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
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

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("opt-in dry-run with enable=true should not error: %v", err)
	}
}

// ---- CollectMountRules tests ----

func TestCollectMountRules_ExcludedByPolicy(t *testing.T) {
	t.Parallel()

	gpol := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/null"}}
	rules, errs := CollectMountRules("/dev/null", gpol, policy.Container{})

	if len(rules) != 0 || len(errs) != 0 {
		t.Errorf("expected no rules/errors for denied path, got rules=%v errs=%v", rules, errs)
	}
}

func TestCollectMountRules_File(t *testing.T) {
	t.Parallel()

	gpol := policy.Global{Mode: policy.ModeAll}
	rules, errs := CollectMountRules("/dev/null", gpol, policy.Container{})

	if len(errs) != 0 {
		t.Fatalf("unexpected errors: %v", errs)
	}

	if len(rules) != 1 {
		t.Fatalf("expected 1 rule, got %d", len(rules))
	}

	if !rules[0].Allow || rules[0].Access != "rwm" {
		t.Errorf("rule has unexpected allow/access: %+v", rules[0])
	}
}

func TestCollectMountRules_BadPath(t *testing.T) {
	t.Parallel()

	gpol := policy.Global{Mode: policy.ModeAll}
	rules, errs := CollectMountRules("/dev/nonexistent-device-xyzzy", gpol, policy.Container{})

	if len(rules) != 0 {
		t.Errorf("expected no rules for bad path, got %v", rules)
	}

	if len(errs) == 0 {
		t.Error("expected errors for bad path, got none")
	}
}

// captureLogger sets logger.L() to write to a buffer for the duration of the
// test and restores the previous logger when the test ends.
func captureLogger(t *testing.T) *bytes.Buffer {
	t.Helper()

	var buf bytes.Buffer
	logger.Set(slog.New(slog.NewTextHandler(&buf, &slog.HandlerOptions{
		Level: slog.LevelDebug,
	})))
	t.Cleanup(func() { logger.Set(nil) })

	return &buf
}

//nolint:tparallel // subtests share the global logger via captureLogger; parallel would cause log interleaving
func TestProcessContainer_SwarmServiceLabels(t *testing.T) {
	t.Parallel()

	const (
		cid       = "abc123"
		serviceID = "svc456"
		pid       = 51
	)

	makeSwarmContainer := func(extraLabels map[string]string) container.InspectResponse {
		labels := map[string]string{swarmServiceIDLabel: serviceID}
		maps.Copy(labels, extraLabels)

		return container.InspectResponse{
			ContainerJSONBase: &container.ContainerJSONBase{
				State: &container.State{Pid: pid},
			},
			Config: &container.Config{Labels: labels},
		}
	}

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	cases := []struct {
		name            string
		containerInfo   container.InspectResponse
		svcResult       swarm.Service
		svcErr          error
		store           *config.Store
		wantServiceCall bool
		wantLogMsg      string
		wantSkip        bool
	}{
		{
			name:          "deploy.labels-only grants opt-in",
			containerInfo: makeSwarmContainer(nil),
			svcResult: swarm.Service{
				Spec: swarm.ServiceSpec{
					Annotations: swarm.Annotations{
						Name:   "my-service",
						Labels: map[string]string{policy.LabelEnable: "true"},
					},
				},
			},
			store:           newStore(policy.ModeOptIn, true),
			wantServiceCall: true,
			wantLogMsg:      "opt-in granted via service-level label",
		},
		{
			name: "container labels override service",
			containerInfo: makeSwarmContainer(map[string]string{
				policy.LabelEnable: "false",
			}),
			svcResult: swarm.Service{
				Spec: swarm.ServiceSpec{
					Annotations: swarm.Annotations{
						Name:   "my-service",
						Labels: map[string]string{policy.LabelEnable: "true"},
					},
				},
			},
			store:           newStore(policy.ModeOptIn, true),
			wantServiceCall: true,
			wantSkip:        true,
		},
		{
			name: "non-Swarm passthrough",
			containerInfo: container.InspectResponse{
				ContainerJSONBase: &container.ContainerJSONBase{
					State: &container.State{Pid: pid},
				},
				Config: &container.Config{Labels: map[string]string{
					policy.LabelEnable: "true",
				}},
			},
			store:           newStore(policy.ModeOptIn, true),
			wantServiceCall: false,
		},
		{
			name: "service inspect error is non-fatal",
			containerInfo: makeSwarmContainer(map[string]string{
				policy.LabelEnable: "true",
			}),
			svcErr:          errDaemonUnavail,
			store:           newStore(policy.ModeOptIn, true),
			wantServiceCall: true,
			wantLogMsg:      "could not inspect parent service",
		},
		{
			name: "typo WARN on service label",
			containerInfo: makeSwarmContainer(map[string]string{
				policy.LabelEnable: "true",
			}),
			svcResult: swarm.Service{
				Spec: swarm.ServiceSpec{
					Annotations: swarm.Annotations{
						Name:   "my-service",
						Labels: map[string]string{policy.LabelPrefix + "enabled": "true"},
					},
				},
			},
			store:           newStore(policy.ModeOptIn, true),
			wantServiceCall: true,
			wantLogMsg:      "unrecognized swarm-device-access label on parent service",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			buf := captureLogger(t)

			procRoot := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

			inspector := &fakeInspector{
				result:        tc.containerInfo,
				serviceResult: tc.svcResult,
				serviceErr:    tc.svcErr,
			}

			proc := &Processor{
				Inspector: inspector,
				Cfg:       tc.store,
				HostRoot:  t.TempDir(),
				ProcRoot:  procRoot,
			}

			_ = proc.ProcessContainer(context.Background(), cid)

			logOutput := buf.String()

			if tc.wantServiceCall && inspector.serviceCalls == 0 {
				t.Error("expected ServiceInspectWithRaw to be called, was not")
			}

			if !tc.wantServiceCall && inspector.serviceCalls > 0 {
				t.Errorf("expected no ServiceInspectWithRaw call, got %d", inspector.serviceCalls)
			}

			if tc.wantLogMsg != "" && !strings.Contains(logOutput, tc.wantLogMsg) {
				t.Errorf("expected log to contain %q, got:\n%s", tc.wantLogMsg, logOutput)
			}

			if tc.wantSkip && !strings.Contains(logOutput, "skipped by policy") {
				t.Errorf("expected skip-by-policy log, got:\n%s", logOutput)
			}
		})
	}
}
