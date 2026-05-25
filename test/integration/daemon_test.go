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
//	GOOS=linux go test -tags=integration -timeout=120s ./test/integration/...
//
// Or from the repo root:
//
//	make go-build && go test -tags=integration -timeout=120s ./test/integration/...
package integration

import (
	"bufio"
	"context"
	"io"
	"net/http"
	"os"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/docker/docker/api/types/container"
	"github.com/docker/docker/api/types/image"
	dockerclient "github.com/docker/docker/client"

	"github.com/leinardi/swarm-device-access/internal/policy"
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

	testImage = "docker.io/library/busybox:1.36"
)

var (
	pullTestImageOnce sync.Once
	pullTestImageErr  error
)

// TestDaemon_DryRun_DetectsDeviceMount starts the daemon with -dry-run and
// -policy-mode=opt-in, creates a container with swarm-device-access.enable=true
// that bind-mounts /dev/null, and asserts the daemon logs that it would apply
// a device rule for that mount.
//
// No BPF syscalls or elevated privileges are required: -dry-run skips
// AddDeviceRules and logs intent instead.
func TestDaemon_DryRun_DetectsDeviceMount(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Log("daemon logged dry-run device rule — pipeline verified")
	case <-time.After(detectTimeout):
		t.Errorf("timeout: daemon did not log %q for /dev/null bind mount", logDetected)
	}
}

// TestDaemon_DryRun_PolicyMode_SkipsUnlabelledContainer verifies that
// -policy-mode=opt-in does not apply rules for containers that lack the
// swarm-device-access.enable=true label.
func TestDaemon_DryRun_PolicyMode_SkipsUnlabelledContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, nil, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon applied dry-run rule for unlabelled container; should have skipped")
	case <-time.After(3 * time.Second):
		t.Log("correctly skipped unlabelled container")
	}
}

// TestDaemon_DryRun_PolicyMode_All_ProcessesUnlabelledContainer verifies that
// -policy-mode=all processes containers that do not carry the opt-in label.
func TestDaemon_DryRun_PolicyMode_All_ProcessesUnlabelledContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=all",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, nil, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Log("policy-mode=all processed unlabelled container")
	case <-time.After(detectTimeout):
		t.Error("timeout: policy-mode=all did not process unlabelled container")
	}
}

// TestDaemon_DryRun_PolicyMode_All_SkipsOptedOutContainer verifies that
// -policy-mode=all still skips containers that explicitly set enable=false.
func TestDaemon_DryRun_PolicyMode_All_SkipsOptedOutContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=all",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "false",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon applied rule for enable=false container; should have skipped")
	case <-time.After(3 * time.Second):
		t.Log("correctly skipped opted-out container in policy-mode=all")
	}
}

// TestDaemon_DryRun_GlobalDeviceDeny_BlocksDevice verifies that a device
// matching the global -device-deny list is not included in the dry-run output.
func TestDaemon_DryRun_GlobalDeviceDeny_BlocksDevice(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-device-deny=/dev/null",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon logged a rule for a denied device; deny list not enforced")
	case <-time.After(3 * time.Second):
		t.Log("correctly blocked device matched by global deny list")
	}
}

// TestDaemon_DryRun_GlobalDeviceAllow_BlocksNonMatchingDevice verifies that a
// device NOT in the global -device-allow list is excluded from the dry-run output.
func TestDaemon_DryRun_GlobalDeviceAllow_BlocksNonMatchingDevice(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	// Allow list contains /dev/zero; the container mounts /dev/null — not allowed.
	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-device-allow=/dev/zero",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon logged a rule for a device outside the allow list")
	case <-time.After(3 * time.Second):
		t.Log("correctly excluded device not in global allow list")
	}
}

// TestDaemon_DryRun_LabelDeviceDeny_BlocksDevice verifies that the
// swarm-device-access.device-deny container label filters out matching devices.
func TestDaemon_DryRun_LabelDeviceDeny_BlocksDevice(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable:     "true",
		policy.LabelDeviceDeny: "/dev/null",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon logged a rule for a device denied via container label")
	case <-time.After(3 * time.Second):
		t.Log("correctly blocked device matched by container device-deny label")
	}
}

// TestDaemon_DryRun_LabelDeviceAllow_NarrowsAccess verifies that the
// swarm-device-access.device-allow container label restricts which devices
// receive rules: a mount outside the per-container allow list is skipped.
func TestDaemon_DryRun_LabelDeviceAllow_NarrowsAccess(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	// Per-container allow list is /dev/zero; container mounts /dev/null — excluded.
	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable:      "true",
		policy.LabelDeviceAllow: "/dev/zero",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Error("daemon logged a rule for a device outside the container allow label")
	case <-time.After(3 * time.Second):
		t.Log("correctly excluded device not in container device-allow label")
	}
}

// TestDaemon_DryRun_ProcessesExistingContainers verifies the startup-enumeration
// path: a container already running when the daemon starts must be processed by
// processExistingContainers without waiting for a Docker event.
func TestDaemon_DryRun_ProcessesExistingContainers(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	// Container starts BEFORE the daemon — no Docker event will fire for it.
	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	// processExistingContainers runs before "subscribed to docker events" is logged,
	// so logDetected will already be buffered when launchDaemon returns.
	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	select {
	case <-detected:
		t.Log("startup enumeration detected pre-existing container")
	case <-time.After(detectTimeout):
		t.Error("timeout: daemon did not process pre-existing container at startup")
	}
}

// TestDaemon_DryRun_UnpauseEvent verifies that an "unpause" Docker event
// triggers the same device-rule pipeline as a "start" event.
func TestDaemon_DryRun_UnpauseEvent(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+2*detectTimeout+5*time.Second,
	)
	defer cancel()

	detected := launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	// Wait for the start event to be processed first.
	select {
	case <-detected:
		t.Log("start event processed")
	case <-time.After(detectTimeout):
		t.Fatal("timeout: start event not detected")
	}

	if err := cli.ContainerPause(ctx, containerID); err != nil {
		t.Fatalf("pause container: %v", err)
	}

	if err := cli.ContainerUnpause(ctx, containerID); err != nil {
		t.Fatalf("unpause container: %v", err)
	}

	// The unpause event must trigger a second processing pass.
	select {
	case <-detected:
		t.Log("unpause event processed — daemon handles unpause correctly")
	case <-time.After(detectTimeout):
		t.Error("timeout: daemon did not process the unpause event")
	}
}

// TestDaemon_DryRun_MultipleDeviceMounts verifies that every device bind-mount
// in a container produces a separate dry-run rule log entry.
func TestDaemon_DryRun_MultipleDeviceMounts(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	// Buffer 2: one slot per expected device rule.
	detected := launchDaemon(t, ctx, 2,
		"-dry-run",
		"-policy-mode=opt-in",
		"-log-level=debug",
		"-log-format=text",
	)

	containerID := startTestContainer(t, ctx, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{"/dev/null:/dev/null", "/dev/zero:/dev/zero"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	for range 2 {
		select {
		case <-detected:
		case <-time.After(detectTimeout):
			t.Error("timeout: expected two dry-run device rules, got fewer")
			return
		}
	}

	t.Log("both device rules logged for /dev/null and /dev/zero")
}

// TestDaemon_DryRun_ConfigFile_LoadsPolicyMode verifies that daemon settings
// are correctly loaded from a YAML config file passed via -config, and that
// CLI defaults do not override file-only values.
func TestDaemon_DryRun_ConfigFile_LoadsPolicyMode(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+detectTimeout+5*time.Second,
	)
	defer cancel()

	f, err := os.CreateTemp(t.TempDir(), "sda-config-*.yaml")
	if err != nil {
		t.Fatalf("create temp config: %v", err)
	}

	if _, err := f.WriteString(
		"policy-mode: \"all\"\ndry-run: true\nlog-level: debug\nlog-format: text\n",
	); err != nil {
		t.Fatalf("write temp config: %v", err)
	}

	configPath := f.Name()
	_ = f.Close()

	// No -dry-run or -policy-mode flags on CLI — both come from the config file.
	detected := launchDaemon(t, ctx, 1, "-config="+configPath)

	containerID := startTestContainer(t, ctx, cli, nil, []string{"/dev/null:/dev/null"})
	t.Cleanup(func() { removeContainer(t, cli, containerID) })

	select {
	case <-detected:
		t.Log("config file applied: policy-mode=all processed unlabelled container")
	case <-time.After(detectTimeout):
		t.Error("timeout: config file policy-mode=all did not process unlabelled container")
	}
}

// TestDaemon_DryRun_MetricsEndpoint verifies that -metrics-addr starts a
// Prometheus-compatible HTTP endpoint that serves sda_ metrics.
func TestDaemon_DryRun_MetricsEndpoint(t *testing.T) {
	_ = requireDocker(t)

	ctx, cancel := context.WithTimeout(
		context.Background(),
		startupTimeout+10*time.Second,
	)
	defer cancel()

	const metricsAddr = "127.0.0.1:19091"

	launchDaemon(t, ctx, 1,
		"-dry-run",
		"-policy-mode=opt-in",
		"-metrics-addr="+metricsAddr,
		"-log-level=debug",
		"-log-format=text",
	)

	req, err := http.NewRequestWithContext(
		ctx,
		http.MethodGet,
		"http://"+metricsAddr+"/metrics",
		nil,
	)
	if err != nil {
		t.Fatalf("build metrics request: %v", err)
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("GET /metrics: %v", err)
	}

	defer resp.Body.Close()

	if resp.StatusCode != http.StatusOK {
		t.Fatalf("GET /metrics: want 200, got %d", resp.StatusCode)
	}

	body, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("read /metrics body: %v", err)
	}

	if !strings.Contains(string(body), "# HELP sda_") {
		t.Errorf("GET /metrics: expected Prometheus help text for sda_ metrics;\nbody:\n%s", body)
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

func ensureTestImage(t *testing.T, cli *dockerclient.Client) {
	t.Helper()

	pullTestImageOnce.Do(func() {
		ctx, cancel := context.WithTimeout(context.Background(), 45*time.Second)
		defer cancel()

		reader, err := cli.ImagePull(ctx, testImage, image.PullOptions{})
		if err != nil {
			pullTestImageErr = err

			return
		}
		defer reader.Close()

		_, err = io.Copy(io.Discard, reader)
		if err != nil {
			pullTestImageErr = err
		}
	})

	if pullTestImageErr != nil {
		t.Fatalf("pull test image %s: %v", testImage, pullTestImageErr)
	}
}

// launchDaemon starts the daemon binary with the given flags, waits for it to
// subscribe to Docker events, and returns a channel that receives a struct{}
// each time logDetected appears in the daemon output. detectedBufSize controls
// the channel buffer; use 1 for most tests, 2 when two rules are expected.
func launchDaemon(
	t *testing.T,
	ctx context.Context,
	detectedBufSize int,
	flags ...string,
) chan struct{} {
	t.Helper()

	binary := findBinary(t)
	cmd := exec.CommandContext(ctx, binary, flags...)

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
	detected := make(chan struct{}, detectedBufSize)

	go scanOutput(t, stdout, map[string]chan struct{}{
		logReady:    ready,
		logDetected: detected,
	})

	select {
	case <-ready:
		t.Log("daemon subscribed to Docker events")
	case <-time.After(startupTimeout):
		t.Fatal("timeout waiting for daemon to subscribe to events")
	}

	return detected
}

func startTestContainer(
	t *testing.T,
	ctx context.Context,
	cli *dockerclient.Client,
	labels map[string]string,
	binds []string,
) string {
	t.Helper()

	resp, err := cli.ContainerCreate(ctx,
		&container.Config{
			Image:  testImage,
			Cmd:    []string{"sh", "-c", "sleep 30"},
			Labels: labels,
		},
		&container.HostConfig{
			Binds: binds,
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

// scanOutput reads lines from r and sends a struct{} on each channel in signals
// whenever the corresponding substring is found. A nil channel means "log but
// don't signal". Signals fire for every matching line; channel buffering and
// the non-blocking send determine how many are delivered. Safe to call from a
// goroutine.
func scanOutput(t *testing.T, r io.Reader, signals map[string]chan struct{}) {
	t.Helper()

	scanner := bufio.NewScanner(r)

	for scanner.Scan() {
		line := scanner.Text()
		t.Log("[daemon]", line)

		for substr, ch := range signals {
			if strings.Contains(line, substr) {
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
