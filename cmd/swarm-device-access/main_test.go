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

package main

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"strconv"
	"sync/atomic"
	"testing"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/mount"
)

// fakeInspector is a test double for containerInspector.
type fakeInspector struct {
	result container.InspectResponse
	err    error
}

func (f *fakeInspector) ContainerInspect(
	_ context.Context,
	_ string,
) (container.InspectResponse, error) {
	return f.result, f.err
}

// buildProcRoot creates a minimal /proc/<pid>/{cgroup,mountinfo} structure
// under a temp dir so processContainer can resolve the cgroup path without a
// real /proc filesystem.
//
//nolint:unparam // cgroupContent varies across test cases; linter sees current call sites only
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

var (
	errTransportEOF  = errors.New("transport EOF")
	errDaemonUnavail = errors.New("daemon unavailable")
)

func TestMain(m *testing.M) {
	activeCfg.Store(&hotConfig{})
	os.Exit(m.Run())
}

func TestNextBackoff_Doubles(t *testing.T) {
	got := nextBackoff(1 * time.Second)
	if got != 2*time.Second {
		t.Errorf("nextBackoff(1s) = %v, want 2s", got)
	}
}

func TestNextBackoff_Caps(t *testing.T) {
	got := nextBackoff(maxBackoff)
	if got != maxBackoff {
		t.Errorf("nextBackoff(maxBackoff) = %v, want %v (capped)", got, maxBackoff)
	}

	got = nextBackoff(maxBackoff - 1*time.Second)
	if got != maxBackoff {
		t.Errorf("nextBackoff(maxBackoff-1s) = %v, want %v (capped)", got, maxBackoff)
	}
}

func TestPtr_ReturnsAddressOfValue(t *testing.T) {
	v := int64(42)
	//nolint:modernize // ptr takes address of v; new(int64) returns zero-value pointer, not &v
	p := ptr(v)

	if *p != 42 {
		t.Errorf("*ptr = %d, want 42", *p)
	}
}

func TestSleepCtx_RespectsCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	start := time.Now()

	sleepCtx(ctx, 5*time.Second)

	elapsed := time.Since(start)
	if elapsed > 100*time.Millisecond {
		t.Errorf("sleepCtx took %v with canceled ctx; want immediate return", elapsed)
	}
}

func TestSleepCtx_WaitsForDuration(t *testing.T) {
	ctx := context.Background()
	target := 50 * time.Millisecond

	start := time.Now()

	sleepCtx(ctx, target)

	elapsed := time.Since(start)
	if elapsed < target {
		t.Errorf("sleepCtx returned in %v; want at least %v", elapsed, target)
	}
}

// makeChans returns buffered event/error channels for driving consumeEvents in tests.
func makeChans(msgBuf, errBuf int) (msgs chan events.Message, errs chan error) {
	return make(chan events.Message, msgBuf), make(chan error, errBuf)
}

func noopApply(_ context.Context, _ string) error { return nil }

func TestConsumeEvents_ContextCancelledReturnsNoReconnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, noopApply)
	if got {
		t.Error(
			"consumeEvents should return false (no reconnect) when context is already canceled",
		)
	}
}

func TestConsumeEvents_StreamErrorReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 1)
	backoff := minBackoff

	errs <- errTransportEOF

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, noopApply)
	if !got {
		t.Error("consumeEvents should return true (reconnect) on stream error")
	}

	if backoff != nextBackoff(minBackoff) {
		t.Errorf("backoff = %v, want %v after one error", backoff, nextBackoff(minBackoff))
	}
}

func TestConsumeEvents_ContextErrFromStreamErrorNoReconnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	msgs, errs := makeChans(0, 1)
	backoff := minBackoff

	errs <- context.Canceled

	cancel()

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, noopApply)
	if got {
		t.Error("consumeEvents should return false when stream error is context.Canceled")
	}
}

func TestConsumeEvents_ChannelCloseReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	close(msgs)

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, noopApply)
	if !got {
		t.Error("consumeEvents should return true (reconnect) on channel close")
	}
}

func TestConsumeEvents_EventCallsApply(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(2, 0)
	backoff := minBackoff

	var called atomic.Int32

	apply := func(_ context.Context, _ string) error {
		called.Add(1)

		return nil
	}

	msgs <- events.Message{Actor: events.Actor{ID: "container-1"}}

	msgs <- events.Message{Actor: events.Actor{ID: "container-2"}}

	// Cancel after a brief delay so consumeEvents exits cleanly.
	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, apply)

	if called.Load() != 2 {
		t.Errorf("apply called %d times, want 2", called.Load())
	}
}

func TestConsumeEvents_DeduplicatesProcessedIDs(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := minBackoff
	processed := map[string]struct{}{
		"already-seen": {},
	}

	var called atomic.Int32

	apply := func(_ context.Context, _ string) error {
		called.Add(1)

		return nil
	}

	msgs <- events.Message{Actor: events.Actor{ID: "already-seen"}}

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, processed, &backoff, nil, apply)

	if called.Load() != 0 {
		t.Errorf("apply called %d times for deduplicated ID, want 0", called.Load())
	}

	if _, stillPresent := processed["already-seen"]; stillPresent {
		t.Error("processed map should have entry removed after deduplication")
	}
}

// ---- policy helper tests ----

func TestContainerMatchesLabelPolicy(t *testing.T) {
	cases := []struct {
		labels map[string]string
		policy string
		want   bool
	}{
		{nil, "", true},
		{map[string]string{"foo": "bar"}, "", true},
		{map[string]string{"sda.enable": "true"}, "sda.enable=true", true},
		{map[string]string{"sda.enable": "false"}, "sda.enable=true", false},
		{map[string]string{}, "sda.enable=true", false},
		{nil, "sda.enable=true", false},
		{map[string]string{"sda.enable": ""}, "sda.enable=", true},
	}

	for _, tc := range cases {
		got := containerMatchesLabelPolicy(tc.labels, tc.policy)
		if got != tc.want {
			t.Errorf("policy=%q labels=%v → got %v, want %v", tc.policy, tc.labels, got, tc.want)
		}
	}
}

func TestDevicePathAllowed(t *testing.T) {
	cases := []struct {
		path  string
		allow stringSliceFlag
		deny  stringSliceFlag
		want  bool
	}{
		{"/dev/nvidia0", nil, nil, true},
		{"/dev/nvidia0", stringSliceFlag{"/dev/nvidia*"}, nil, true},
		{"/dev/sda", stringSliceFlag{"/dev/nvidia*"}, nil, false},
		{"/dev/sda", nil, stringSliceFlag{"/dev/sd*"}, false},
		// deny takes priority over allow
		{"/dev/nvidia0", stringSliceFlag{"/dev/nvidia*"}, stringSliceFlag{"/dev/nvidia*"}, false},
		{"/dev/nvidia0", nil, stringSliceFlag{"/dev/sd*"}, true},
	}

	for _, tc := range cases {
		got := devicePathAllowed(tc.path, tc.allow, tc.deny)
		if got != tc.want {
			t.Errorf("path=%q allow=%v deny=%v → got %v, want %v",
				tc.path, tc.allow, tc.deny, got, tc.want)
		}
	}
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

			got := isDeviceMountSource(tc.path)
			if got != tc.want {
				t.Errorf("isDeviceMountSource(%q) = %v, want %v", tc.path, got, tc.want)
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

			got := hostCGroupPath(tc.sysfsPath, tc.cgroupPrefix, tc.cgroupRoot)
			if got != tc.want {
				t.Errorf("hostCGroupPath() = %q, want %q", got, tc.want)
			}
		})
	}
}

// ---- processContainer tests ----

func TestProcessContainer_InspectError(t *testing.T) {
	insp := &fakeInspector{err: errDaemonUnavail}

	err := processContainer(context.Background(), insp, "abc", "/", false)
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

	err := processContainer(context.Background(), insp, "abc", "/", false)
	if err != nil {
		t.Fatalf("expected nil error for nil state, got %v", err)
	}
}

func TestProcessContainer_ZeroPid(t *testing.T) {
	state := &container.State{Pid: 0}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
	}}

	err := processContainer(context.Background(), insp, "abc", "/", false)
	if err != nil {
		t.Fatalf("expected nil error for pid=0, got %v", err)
	}
}

func TestProcessContainer_NoDevMounts(t *testing.T) {
	const pid = 42

	// cgroup v2 fixture: unified hierarchy
	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	// Container has only non-/dev mounts — processContainer should return nil
	// without attempting any cgroup writes (cgroup write would fail here since
	// the path doesn't exist, but we never reach that code).
	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
		Mounts: []container.MountPoint{
			{Source: "/tmp/data", Destination: "/data", Type: mount.TypeBind},
			{Source: "/var/log", Destination: "/logs", Type: mount.TypeBind},
		},
	}}

	err := processContainer(context.Background(), insp, "abc", root, false)
	if err != nil {
		t.Fatalf("expected nil error for container with no /dev mounts, got %v", err)
	}
}

func TestProcessContainer_DevMountFilterApplied(t *testing.T) {
	const pid = 43

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	// Mixed mounts: one /dev, one not. /dev/null is a real device so the rule is
	// collected, but AddDeviceRules fails because the fake cgroup path does not
	// exist. The aggregated error is now returned (not swallowed).
	state := &container.State{Pid: pid}
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
		Mounts: []container.MountPoint{
			{Source: "/tmp/data", Destination: "/data", Type: mount.TypeBind},
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}

	err := processContainer(context.Background(), insp, "abc", root, false)
	if err == nil {
		t.Fatal(
			"processContainer should return error when AddDeviceRules fails on fake cgroup path",
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

	// dry-run=true: rules are logged but AddDeviceRules is never called, so no
	// error from the missing cgroup path.
	err := processContainer(context.Background(), insp, "abc", root, true)
	if err != nil {
		t.Fatalf("dry-run processContainer should not error: %v", err)
	}
}

func TestProcessContainer_DeduplicatesDuplicateMounts(t *testing.T) {
	const pid = 45

	cgroupContent := "0::/docker/testcontainer\n"
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n" //nolint:dupword // cgroup2 appears twice: fs type and superblock type in mountinfo format

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	state := &container.State{Pid: pid}
	// Two mounts that both resolve to /dev/null (same major:minor device).
	insp := &fakeInspector{result: container.InspectResponse{
		ContainerJSONBase: &container.ContainerJSONBase{State: state},
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
			{Source: "/dev/null", Destination: "/dev/null2", Type: mount.TypeBind},
		},
	}}

	// dry-run verifies the collect+dedupe path returns nil (no cgroup write).
	err := processContainer(context.Background(), insp, "abc", root, true)
	if err != nil {
		t.Fatalf("dry-run with duplicate mounts should not error: %v", err)
	}
}

func TestValidatePatterns(t *testing.T) {
	t.Parallel()

	cases := []struct {
		patterns stringSliceFlag
		wantErr  bool
	}{
		{patterns: nil, wantErr: false},
		{patterns: stringSliceFlag{"/dev/nvidia*", "/dev/dri/*"}, wantErr: false},
		{patterns: stringSliceFlag{"/dev/nvidia["}, wantErr: true},
		{patterns: stringSliceFlag{"/dev/valid", "/dev/bad["}, wantErr: true},
	}

	for _, tc := range cases {
		err := validatePatterns(tc.patterns)
		if (err != nil) != tc.wantErr {
			t.Errorf("validatePatterns(%v) err=%v, wantErr=%v", tc.patterns, err, tc.wantErr)
		}
	}
}

func TestValidateRequireLabel(t *testing.T) {
	t.Parallel()

	cases := []struct {
		policy  string
		wantErr bool
	}{
		{policy: "", wantErr: false},
		{policy: "foo=bar", wantErr: false},
		{policy: "foo=", wantErr: false},
		{policy: "foo", wantErr: true},
		{policy: "=bar", wantErr: true},
	}

	for _, tc := range cases {
		err := validateRequireLabel(tc.policy)
		if (err != nil) != tc.wantErr {
			t.Errorf("validateRequireLabel(%q) err=%v, wantErr=%v", tc.policy, err, tc.wantErr)
		}
	}
}

func TestCollectMountRules_ExcludedByPolicy(t *testing.T) {
	t.Parallel()

	cfg := &hotConfig{deviceDeny: stringSliceFlag{"/dev/null"}}
	rules, errs := collectMountRules("/dev/null", cfg)

	if len(rules) != 0 || len(errs) != 0 {
		t.Errorf("expected no rules/errors for denied path, got rules=%v errs=%v", rules, errs)
	}
}

func TestCollectMountRules_File(t *testing.T) {
	t.Parallel()

	cfg := &hotConfig{}
	rules, errs := collectMountRules("/dev/null", cfg)

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

	cfg := &hotConfig{}
	rules, errs := collectMountRules("/dev/nonexistent-device-xyzzy", cfg)

	if len(rules) != 0 {
		t.Errorf("expected no rules for bad path, got %v", rules)
	}

	if len(errs) == 0 {
		t.Error("expected errors for bad path, got none")
	}
}

func TestConsumeEvents_ClearProcessedOnTTL(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(0, 0)
	backoff := minBackoff
	processed := map[string]struct{}{
		"stale-a": {},
		"stale-b": {},
	}

	// Fire the TTL immediately via a closed channel.
	clearCh := make(chan time.Time)
	close(clearCh)

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, processed, &backoff, clearCh, noopApply)

	if len(processed) != 0 {
		t.Errorf("processed map has %d entries after TTL, want 0", len(processed))
	}
}

func TestConsumeEvents_BackoffResetsOnSuccessfulEvent(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()

	msgs, errs := makeChans(1, 0)
	backoff := maxBackoff // start with a high backoff

	msgs <- events.Message{Actor: events.Actor{ID: "c1"}}

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, nil, noopApply)

	if backoff != minBackoff {
		t.Errorf("backoff = %v after successful event, want %v (reset)", backoff, minBackoff)
	}
}
