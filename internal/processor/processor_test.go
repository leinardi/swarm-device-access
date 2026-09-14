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

	buf := captureLogger(t)

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("expected nil error for container with no /dev mounts, got %v", err)
	}

	if strings.Contains(buf.String(), "container processed") {
		t.Errorf("expected no summary for container without /dev mounts, got: %s", buf.String())
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
	result := CollectMountRules("/dev/null", gpol, policy.Container{})
	rules, errs := result.Rules, result.Errs

	if len(rules) != 0 || len(errs) != 0 {
		t.Errorf("expected no rules/errors for denied path, got rules=%v errs=%v", rules, errs)
	}

	if result.Skipped != 1 {
		t.Errorf("expected Skipped=1 for denied path, got %d", result.Skipped)
	}
}

func TestCollectMountRules_File(t *testing.T) {
	t.Parallel()

	gpol := policy.Global{Mode: policy.ModeAll}
	result := CollectMountRules("/dev/null", gpol, policy.Container{})
	rules, errs := result.Rules, result.Errs

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
	result := CollectMountRules("/dev/nonexistent-device-xyzzy", gpol, policy.Container{})
	rules, errs := result.Rules, result.Errs

	if len(rules) != 0 {
		t.Errorf("expected no rules for bad path, got %v", rules)
	}

	if len(errs) == 0 {
		t.Error("expected errors for bad path, got none")
	}
}

// TestCollectMountRules_DirectoryMount_NoChildrenMatch checks that a WARN is emitted
// when a directory mount has children but none match the allow/deny policy.
func TestCollectMountRules_DirectoryMount_NoChildrenMatch(t *testing.T) {
	dir := t.TempDir()

	err := os.WriteFile(filepath.Join(dir, "card0"), []byte{}, 0o600)
	if err != nil {
		t.Fatalf("create card0: %v", err)
	}

	err = os.WriteFile(filepath.Join(dir, "renderD128"), []byte{}, 0o600)
	if err != nil {
		t.Fatalf("create renderD128: %v", err)
	}

	gpol := policy.Global{
		Mode:        policy.ModeAll,
		DeviceAllow: []string{filepath.Join(dir, "nonexistent")},
	}

	buf := captureLogger(t)

	result := CollectMountRules(dir, gpol, policy.Container{})
	rules, errs := result.Rules, result.Errs

	if len(rules) != 0 {
		t.Errorf("expected no rules, got %v", rules)
	}

	if len(errs) != 0 {
		t.Errorf("expected no errors, got %v", errs)
	}

	logOutput := buf.String()
	if !strings.Contains(logOutput, "mount excluded: no children matched") {
		t.Errorf("expected WARN log about no children matched, got: %s", logOutput)
	}

	if !strings.Contains(logOutput, "card0") || !strings.Contains(logOutput, "renderD128") {
		t.Errorf("expected child names in WARN log, got: %s", logOutput)
	}
}

// TestCollectMountRules_DirectoryMount_SymlinkToDirSkipped checks that a symlink inside
// a directory mount that points to another directory is not recursed into.
func TestCollectMountRules_DirectoryMount_SymlinkToDirSkipped(t *testing.T) {
	dir := t.TempDir()
	subDir := filepath.Join(dir, "subdir")

	err := os.Mkdir(subDir, 0o755)
	if err != nil {
		t.Fatalf("create subdir: %v", err)
	}

	err = os.Symlink(subDir, filepath.Join(dir, "linktodir"))
	if err != nil {
		t.Fatalf("create symlink: %v", err)
	}

	gpol := policy.Global{Mode: policy.ModeAll}

	buf := captureLogger(t)

	result := CollectMountRules(dir, gpol, policy.Container{})
	rules, errs := result.Rules, result.Errs

	if len(rules) != 0 {
		t.Errorf("expected no rules for dir-only mount, got %v", rules)
	}

	if len(errs) != 0 {
		t.Errorf("unexpected errors: %v", errs)
	}

	if !strings.Contains(buf.String(), "symlink to directory skipped") {
		t.Errorf("expected debug log about skipped dir symlink, got: %s", buf.String())
	}
}

// TestCollectMountRules_DirectoryMount_SymlinkToDevice is the core regression test:
// a directory mount whose children include a symlink to a real device gets a cgroup rule
// injected even though the allow-glob targets the resolved path, not the mount source.
func TestCollectMountRules_DirectoryMount_SymlinkToDevice(t *testing.T) {
	dir := t.TempDir()

	err := os.Symlink("/dev/null", filepath.Join(dir, "null"))
	if err != nil {
		t.Fatalf("create symlink: %v", err)
	}

	gpol := policy.Global{
		Mode:        policy.ModeAll,
		DeviceAllow: []string{"/dev/null"},
	}

	result := CollectMountRules(dir, gpol, policy.Container{})
	rules, errs := result.Rules, result.Errs

	if len(errs) != 0 {
		t.Fatalf("unexpected errors: %v", errs)
	}

	if len(rules) != 1 {
		t.Fatalf("expected 1 rule for /dev/null, got %d", len(rules))
	}

	if !rules[0].Allow || rules[0].Access != "rwm" {
		t.Errorf("rule has unexpected allow/access: %+v", rules[0])
	}
}

// globQuote backslash-escapes glob metacharacters so a t.TempDir() path can be
// used as a literal prefix inside a filepath.Match pattern.
func globQuote(s string) string {
	var sb strings.Builder

	for _, r := range s {
		if strings.ContainsRune(`\*?[`, r) {
			sb.WriteRune('\\')
		}

		sb.WriteRune(r)
	}

	return sb.String()
}

// buildSymlinkTree creates a directory with valid device symlinks, dangling
// symlinks and a regular file, mirroring what a host /dev bind mount looks like
// inside the daemon container (e.g. /dev/log -> /run/... does not resolve).
func buildSymlinkTree(t *testing.T) string {
	t.Helper()

	dir := t.TempDir()

	links := map[string]string{
		"null":    "/dev/null",
		"zero":    "/dev/zero",
		"log":     "/run/systemd/journal/dev-log-xyzzy",
		"nullish": filepath.Join(dir, "missing-xyzzy"),
	}

	for name, target := range links {
		err := os.Symlink(target, filepath.Join(dir, name))
		if err != nil {
			t.Fatalf("create symlink %s: %v", name, err)
		}
	}

	err := os.WriteFile(filepath.Join(dir, "regular.txt"), []byte("not a device"), 0o600)
	if err != nil {
		t.Fatalf("create regular.txt: %v", err)
	}

	return dir
}

func ruleSet(rules []cgroup.DeviceRule) map[string]struct{} {
	set := make(map[string]struct{}, len(rules))
	for _, rule := range rules {
		set[rule.Type+" "+strconv.FormatInt(*rule.Major, 10)+":"+strconv.FormatInt(*rule.Minor, 10)] = struct{}{}
	}

	return set
}

func warnLines(logOutput string) []string {
	var lines []string

	for line := range strings.SplitSeq(logOutput, "\n") {
		if strings.Contains(line, "level=WARN") {
			lines = append(lines, line)
		}
	}

	return lines
}

func TestGlobQuote(t *testing.T) {
	t.Parallel()

	quoted := globQuote(`/tmp/a*b?c[d\e`)
	if quoted != `/tmp/a\*b\?c\[d\\e` {
		t.Fatalf("globQuote = %q", quoted)
	}

	ok, err := filepath.Match(quoted, `/tmp/a*b?c[d\e`)
	if err != nil || !ok {
		t.Fatalf("quoted pattern does not match literal path: ok=%v err=%v", ok, err)
	}
}

// TestCollectMountRules_UnresolvableSymlinks_ExplicitAllow checks that dangling
// symlinks are skipped (never errors) and only the one matched by an explicit
// allow glob produces a WARN.
func TestCollectMountRules_UnresolvableSymlinks_ExplicitAllow(t *testing.T) {
	dir := buildSymlinkTree(t)

	gpol := policy.Global{
		Mode:        policy.ModeAll,
		DeviceAllow: []string{"/dev/null", globQuote(dir) + "/nullish"},
	}

	buf := captureLogger(t)

	result := CollectMountRules(dir, gpol, policy.Container{})

	if len(result.Errs) != 0 {
		t.Fatalf("unexpected errors: %v", result.Errs)
	}

	got := ruleSet(result.Rules)
	if len(got) != 1 || len(result.Rules) != 1 {
		t.Fatalf("expected exactly {c 1:3}, got %v", got)
	}

	if _, ok := got["c 1:3"]; !ok {
		t.Fatalf("expected {c 1:3}, got %v", got)
	}

	// zero (excluded by policy), log, nullish (unresolvable), regular.txt (excluded by policy).
	if result.Skipped != 4 {
		t.Errorf("expected Skipped=4, got %d", result.Skipped)
	}

	warns := warnLines(buf.String())
	if len(warns) != 1 {
		t.Fatalf("expected exactly one WARN, got %d: %v", len(warns), warns)
	}

	if !strings.Contains(warns[0], "path="+filepath.Join(dir, "nullish")) ||
		!strings.Contains(warns[0], "device symlink matches allow policy but cannot be resolved") {
		t.Errorf("expected WARN for nullish, got: %s", warns[0])
	}

	if strings.Contains(warns[0], "path="+filepath.Join(dir, "log")) {
		t.Errorf("unexpected WARN for log: %s", warns[0])
	}
}

// TestCollectMountRules_UnresolvableSymlinks_NoAllowGlobs checks that without
// allow globs dangling symlinks and non-device entries are skipped silently.
func TestCollectMountRules_UnresolvableSymlinks_NoAllowGlobs(t *testing.T) {
	dir := buildSymlinkTree(t)

	gpol := policy.Global{Mode: policy.ModeAll}

	buf := captureLogger(t)

	result := CollectMountRules(dir, gpol, policy.Container{})

	if len(result.Errs) != 0 {
		t.Fatalf("unexpected errors: %v", result.Errs)
	}

	got := ruleSet(result.Rules)
	_, hasNull := got["c 1:3"]
	_, hasZero := got["c 1:5"]

	if len(got) != 2 || !hasNull || !hasZero {
		t.Fatalf("expected {c 1:3, c 1:5}, got %v", got)
	}

	// log, nullish (unresolvable), regular.txt (non-device).
	if result.Skipped != 3 {
		t.Errorf("expected Skipped=3, got %d", result.Skipped)
	}

	logOutput := buf.String()

	warns := warnLines(logOutput)
	if len(warns) != 0 {
		t.Errorf("expected no WARN, got: %v", warns)
	}

	if !strings.Contains(logOutput, "unresolvable symlink skipped") {
		t.Errorf("expected DEBUG about unresolvable symlink, got: %s", logOutput)
	}

	if !strings.Contains(logOutput, "non-device entry skipped") {
		t.Errorf("expected DEBUG about non-device entry, got: %s", logOutput)
	}
}

// TestCollectMountRules_SingleFileNonDevice checks that an explicitly mounted
// single file that is not a device is still reported as an error.
func TestCollectMountRules_SingleFileNonDevice(t *testing.T) {
	t.Parallel()

	file := filepath.Join(t.TempDir(), "regular.txt")

	err := os.WriteFile(file, []byte("not a device"), 0o600)
	if err != nil {
		t.Fatalf("create file: %v", err)
	}

	result := CollectMountRules(file, policy.Global{Mode: policy.ModeAll}, policy.Container{})

	if len(result.Rules) != 0 || len(result.Errs) != 1 {
		t.Fatalf("expected one error and no rules, got rules=%v errs=%v", result.Rules, result.Errs)
	}

	if !errors.Is(result.Errs[0], errNotDevice) {
		t.Errorf("expected errNotDevice, got %v", result.Errs[0])
	}
}

// TestProcessContainer_DevDirectorySummary checks that a whole-/dev mount is
// processed without a container-level error and emits one INFO summary.
func TestProcessContainer_DevDirectorySummary(t *testing.T) {
	const pid = 46

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
		Mounts: []container.MountPoint{
			{Source: "/dev", Destination: "/dev", Type: mount.TypeBind},
		},
	}}

	store := config.NewStore()
	store.Set(config.Runtime{
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

	err := proc.ProcessContainer(context.Background(), "abc")
	if err != nil {
		t.Fatalf("ProcessContainer returned error: %v", err)
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
		isSwarmManager  bool
		wantServiceCall bool
		wantLogMsg      string
		wantSkip        bool
	}{
		{
			name:          "deploy.labels-only grants opt-in on manager",
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
			isSwarmManager:  true,
			wantServiceCall: true,
			wantLogMsg:      "opt-in granted via service-level label",
		},
		{
			name: "container labels override service on manager",
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
			isSwarmManager:  true,
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
			isSwarmManager:  true,
			wantServiceCall: false,
		},
		{
			name: "service inspect error is non-fatal on manager",
			containerInfo: makeSwarmContainer(map[string]string{
				policy.LabelEnable: "true",
			}),
			svcErr:          errDaemonUnavail,
			store:           newStore(policy.ModeOptIn, true),
			isSwarmManager:  true,
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
			isSwarmManager:  true,
			wantServiceCall: true,
			wantLogMsg:      "unrecognized swarm-device-access label on parent service",
		},
		{
			name: "worker node skips service inspect",
			containerInfo: makeSwarmContainer(map[string]string{
				policy.LabelEnable: "true",
			}),
			store:           newStore(policy.ModeOptIn, true),
			isSwarmManager:  false,
			wantServiceCall: false,
		},
		{
			name:          "manager warns when known label set via deploy.labels",
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
			isSwarmManager:  true,
			wantServiceCall: true,
			wantLogMsg:      "swarm-device-access label set via deploy.labels on parent service",
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
				Inspector:      inspector,
				Cfg:            tc.store,
				HostRoot:       t.TempDir(),
				ProcRoot:       procRoot,
				IsSwarmManager: tc.isSwarmManager,
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
