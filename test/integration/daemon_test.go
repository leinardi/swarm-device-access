//go:build integration

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

// Package integration tests the swarm-device-access daemon end-to-end.
//
// Tests start the daemon binary as a subprocess with -dry-run (no BPF/root
// required) and verify that the event → inspect → device-detect → apply
// pipeline works against a real Docker daemon.
//
// Prerequisites:
//   - A Linux host with a running Docker daemon.
//   - The daemon binary built at $SDA_TEST_BINARY or ../../dist/swarm-device-access.
//
// Run:
//
//	GOOS=linux go test -tags=integration -timeout=60s ./test/integration/...
//
// Or from the repo root:
//
//	make go-build && go test -tags=integration -timeout=60s ./test/integration/...
package integration

import (
	"bufio"
	"context"
	"io"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	"github.com/docker/docker/api/types/container"
	dockerclient "github.com/docker/docker/client"
)

const (
	// envBinary overrides the daemon binary path. Defaults to defaultBinary.
	envBinary = "SDA_TEST_BINARY"
	// defaultBinary is the path produced by `make go-build`.
	defaultBinary = "../../dist/swarm-device-access"

	// startupTimeout is how long to wait for the daemon to subscribe to events.
	startupTimeout = 15 * time.Second
	// detectTimeout is how long to wait for the dry-run log after the container starts.
	detectTimeout = 15 * time.Second

	// logReady is a substring of the log line emitted once the daemon is subscribed.
	logReady = "subscribed to docker events"
	// logDetected is a substring of the dry-run log line for a device rule.
	logDetected = "dry-run: would add device rule"
)

// TestDaemon_DryRun_DetectsDeviceMount starts the daemon with -dry-run,
// creates a container that bind-mounts /dev/null, and asserts the daemon logs
// that it would apply a device rule for that mount.
//
// No BPF syscalls or elevated privileges are required: -dry-run skips
// AddDeviceRules and logs intent instead.
func TestDaemon_DryRun_DetectsDeviceMount(t *testing.T) {
	t.Parallel()

	binary := findBinary(t)
	cli := requireDocker(t)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	// ---- start daemon ----
	cmd := exec.CommandContext(ctx, binary,
		"-dry-run",
		"-log-level=debug",
		"-log-format=text",
	)

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("stdout pipe: %v", err)
	}

	if err := cmd.Start(); err != nil {
		t.Fatalf("start daemon: %v", err)
	}

	t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	})

	// ---- scan daemon output ----
	ready := make(chan struct{}, 1)
	detected := make(chan struct{}, 1)

	go scanOutput(t, stdout, map[string]chan struct{}{
		logReady:    ready,
		logDetected: detected,
	})

	// ---- wait for daemon ready ----
	select {
	case <-ready:
		t.Log("daemon subscribed to Docker events")
	case <-time.After(startupTimeout):
		t.Fatal("timeout waiting for daemon to subscribe to events")
	}

	// ---- create container with /dev/null bind mount ----
	containerID := startTestContainer(t, ctx, cli)
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	// ---- assert dry-run log appears ----
	select {
	case <-detected:
		t.Log("daemon logged dry-run device rule — pipeline verified")
	case <-time.After(detectTimeout):
		t.Errorf("timeout: daemon did not log %q for /dev/null bind mount", logDetected)
	}
}

// TestDaemon_DryRun_LabelFilter_SkipsUnlabelledContainer verifies that
// -require-label causes the daemon to skip containers without the label.
func TestDaemon_DryRun_LabelFilter_SkipsUnlabelledContainer(t *testing.T) {
	t.Parallel()

	binary := findBinary(t)
	cli := requireDocker(t)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	cmd := exec.CommandContext(ctx, binary,
		"-dry-run",
		"-log-level=debug",
		"-log-format=text",
		"-require-label=sda.enable=true",
	)

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("stdout pipe: %v", err)
	}

	if err := cmd.Start(); err != nil {
		t.Fatalf("start daemon: %v", err)
	}

	t.Cleanup(func() {
		_ = cmd.Process.Kill()
		_ = cmd.Wait()
	})

	ready := make(chan struct{}, 1)
	detected := make(chan struct{}, 1)

	go scanOutput(t, stdout, map[string]chan struct{}{
		logReady:       ready,
		logDetected:    detected,
		"label policy": nil, // just log, don't signal
	})

	select {
	case <-ready:
	case <-time.After(startupTimeout):
		t.Fatal("timeout waiting for daemon ready")
	}

	// Container without the required label — daemon should skip it.
	containerID := startTestContainer(t, ctx, cli)
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon applied dry-run rule for unlabelled container; should have skipped")
	case <-time.After(3 * time.Second):
		t.Log("correctly skipped unlabelled container")
	}
}

// ---- helpers ----

func findBinary(t *testing.T) string {
	t.Helper()

	path := os.Getenv(envBinary)
	if path == "" {
		path = defaultBinary
	}

	if _, err := os.Stat(path); err != nil {
		t.Skipf("daemon binary not found at %q (set %s or run make go-build): %v",
			path, envBinary, err)
	}

	// Probe execability: a cross-compiled Linux binary on macOS returns
	// "exec format error" which would crash the test later. Skip instead.
	if err := exec.Command(path, "-help").Run(); err != nil {
		if strings.Contains(err.Error(), "exec format error") ||
			strings.Contains(err.Error(), "cannot execute") {
			t.Skipf("daemon binary %q is not executable on this platform (cross-compiled?): %v",
				path, err)
		}
		// Non-zero exit is fine: -help exits 0, but any other error means it ran.
	}

	return path
}

func requireDocker(t *testing.T) *dockerclient.Client {
	t.Helper()

	cli, err := dockerclient.NewClientWithOpts(
		dockerclient.FromEnv,
		dockerclient.WithAPIVersionNegotiation(),
	)
	if err != nil {
		t.Skipf("Docker client init failed: %v", err)
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	if _, err := cli.Ping(ctx); err != nil {
		t.Skipf("Docker daemon not reachable: %v", err)
	}

	t.Cleanup(func() { _ = cli.Close() })

	return cli
}

func startTestContainer(t *testing.T, ctx context.Context, cli *dockerclient.Client) string {
	t.Helper()

	resp, err := cli.ContainerCreate(ctx,
		&container.Config{
			Image: "busybox:latest",
			Cmd:   []string{"sh", "-c", "sleep 30"},
		},
		&container.HostConfig{
			Binds: []string{"/dev/null:/dev/null"},
		},
		nil, nil, "",
	)
	if err != nil {
		t.Fatalf("create test container: %v", err)
	}

	if err := cli.ContainerStart(ctx, resp.ID, container.StartOptions{}); err != nil {
		_ = cli.ContainerRemove(ctx, resp.ID, container.RemoveOptions{Force: true})
		t.Fatalf("start test container: %v", err)
	}

	t.Logf("started test container %s", resp.ID[:12])

	return resp.ID
}

func removeContainer(t *testing.T, cli *dockerclient.Client, containerID string) {
	t.Helper()

	ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer cancel()

	_ = cli.ContainerRemove(ctx, containerID, container.RemoveOptions{Force: true})
}

// scanOutput reads lines from r and closes each channel in signals when the
// corresponding substring is found in a log line. A nil channel value means
// "log but don't signal". Safe to call from a goroutine.
func scanOutput(t *testing.T, r io.Reader, signals map[string]chan struct{}) {
	t.Helper()

	scanner := bufio.NewScanner(r)
	closed := make(map[string]bool)

	for scanner.Scan() {
		line := scanner.Text()
		t.Log("[daemon]", line)

		for substr, ch := range signals {
			if !closed[substr] && strings.Contains(line, substr) {
				closed[substr] = true

				if ch != nil {
					select {
					case ch <- struct{}{}:
					default:
					}
				}
			}
		}
	}
}
