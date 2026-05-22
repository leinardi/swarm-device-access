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
	"fmt"
	"os"
	"path/filepath"
	"sync/atomic"
	"testing"
	"time"

	dockertypes "github.com/docker/docker/api/types"
	"github.com/docker/docker/api/types/events"
	"github.com/docker/docker/api/types/mount"
)

// fakeInspector is a test double for containerInspector.
type fakeInspector struct {
	result dockertypes.ContainerJSON
	err    error
}

func (f *fakeInspector) ContainerInspect(
	_ context.Context,
	_ string,
) (dockertypes.ContainerJSON, error) {
	return f.result, f.err
}

// buildProcRoot creates a minimal /proc/<pid>/{cgroup,mountinfo} structure
// under a temp dir so processContainer can resolve the cgroup path without a
// real /proc filesystem.
func buildProcRoot(t *testing.T, pid int, cgroupContent, mountinfoContent string) string {
	t.Helper()

	root := t.TempDir()
	procDir := filepath.Join(root, "proc", fmt.Sprintf("%d", pid))

	if err := os.MkdirAll(procDir, 0o755); err != nil {
		t.Fatalf("mkdir proc: %v", err)
	}

	if err := os.WriteFile(
		filepath.Join(procDir, "cgroup"),
		[]byte(cgroupContent),
		0o644,
	); err != nil {
		t.Fatalf("write cgroup: %v", err)
	}

	if err := os.WriteFile(
		filepath.Join(procDir, "mountinfo"),
		[]byte(mountinfoContent),
		0o644,
	); err != nil {
		t.Fatalf("write mountinfo: %v", err)
	}

	return root
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
	p := ptr(v)

	if p == nil {
		t.Fatal("ptr returned nil")
	}

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
		t.Errorf("sleepCtx took %v with cancelled ctx; want immediate return", elapsed)
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
func makeChans(msgBuf, errBuf int) (chan events.Message, chan error) {
	return make(chan events.Message, msgBuf), make(chan error, errBuf)
}

func noopApply(_ context.Context, _ string) error { return nil }

func TestConsumeEvents_ContextCancelledReturnsNoReconnect(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, noopApply)
	if got {
		t.Error(
			"consumeEvents should return false (no reconnect) when context is already cancelled",
		)
	}
}

func TestConsumeEvents_StreamErrorReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 1)
	backoff := minBackoff

	errs <- errors.New("transport EOF")

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, noopApply)
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

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, noopApply)
	if got {
		t.Error("consumeEvents should return false when stream error is context.Canceled")
	}
}

func TestConsumeEvents_ChannelCloseReturnsReconnect(t *testing.T) {
	ctx := context.Background()
	msgs, errs := makeChans(0, 0)
	backoff := minBackoff

	close(msgs)

	got := consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, noopApply)
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

	consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, apply)

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
	apply := func(_ context.Context, id string) error {
		called.Add(1)
		return nil
	}

	msgs <- events.Message{Actor: events.Actor{ID: "already-seen"}}

	go func() {
		time.Sleep(20 * time.Millisecond)
		cancel()
	}()

	consumeEvents(ctx, msgs, errs, processed, &backoff, apply)

	if called.Load() != 0 {
		t.Errorf("apply called %d times for deduplicated ID, want 0", called.Load())
	}

	if _, stillPresent := processed["already-seen"]; stillPresent {
		t.Error("processed map should have entry removed after deduplication")
	}
}

// ---- processContainer tests ----

func TestProcessContainer_InspectError(t *testing.T) {
	insp := &fakeInspector{err: errors.New("daemon unavailable")}
	err := processContainer(context.Background(), insp, "abc", "/", false)

	if err == nil {
		t.Fatal("expected error from inspect failure, got nil")
	}
}

func TestProcessContainer_NilState(t *testing.T) {
	insp := &fakeInspector{result: dockertypes.ContainerJSON{
		ContainerJSONBase: &dockertypes.ContainerJSONBase{
			State: nil,
		},
	}}

	err := processContainer(context.Background(), insp, "abc", "/", false)
	if err != nil {
		t.Fatalf("expected nil error for nil state, got %v", err)
	}
}

func TestProcessContainer_ZeroPid(t *testing.T) {
	state := &dockertypes.ContainerState{Pid: 0}
	insp := &fakeInspector{result: dockertypes.ContainerJSON{
		ContainerJSONBase: &dockertypes.ContainerJSONBase{State: state},
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
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n"

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	// Container has only non-/dev mounts — processContainer should return nil
	// without attempting any cgroup writes (cgroup write would fail here since
	// the path doesn't exist, but we never reach that code).
	state := &dockertypes.ContainerState{Pid: pid}
	insp := &fakeInspector{result: dockertypes.ContainerJSON{
		ContainerJSONBase: &dockertypes.ContainerJSONBase{State: state},
		Mounts: []dockertypes.MountPoint{
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
	mountinfoContent := "35 22 0:29 / /sys/fs/cgroup rw,nosuid,nodev shared:11 - cgroup2 cgroup2 rw\n"

	root := buildProcRoot(t, pid, cgroupContent, mountinfoContent)

	// Mixed mounts: one /dev, one not. The /dev mount will reach applyMount
	// which will fail (stat on a fake path) but processContainer logs and
	// continues — so the overall return is still nil.
	state := &dockertypes.ContainerState{Pid: pid}
	insp := &fakeInspector{result: dockertypes.ContainerJSON{
		ContainerJSONBase: &dockertypes.ContainerJSONBase{State: state},
		Mounts: []dockertypes.MountPoint{
			{Source: "/tmp/data", Destination: "/data", Type: mount.TypeBind},
			{Source: "/dev/null", Destination: "/dev/null", Type: mount.TypeBind},
		},
	}}

	// processContainer returns nil even when applyMount fails (warns instead).
	err := processContainer(context.Background(), insp, "abc", root, false)
	if err != nil {
		t.Fatalf(
			"processContainer should not return error when applyMount fails (logs warning): %v",
			err,
		)
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

	consumeEvents(ctx, msgs, errs, map[string]struct{}{}, &backoff, noopApply)

	if backoff != minBackoff {
		t.Errorf("backoff = %v after successful event, want %v (reset)", backoff, minBackoff)
	}
}
