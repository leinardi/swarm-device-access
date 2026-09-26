//go:build integration

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

// Package integration tests the swarm-device-access daemon end-to-end.
//
// Tests start the daemon binary as a subprocess with -dry-run (no BPF/root
// required) and verify that the event → inspect → device-detect → apply
// pipeline works against a real Docker daemon.
//
// The daemon sees every container on the host, not only the ones a test
// starts, so assertions only look at JSON log records whose id is the test's
// own container.
//
// Prerequisites:
//   - A Linux host with a running Docker daemon.
//   - The daemon binary built at $SDA_TEST_BINARY or ../../dist/swarm-device-access.
//
// Run from the repo root:
//
//	make go-test-integration
package integration

import (
	"io"
	"net/http"
	"os"
	"strings"
	"testing"

	dockerclient "github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

const (
	bindNull = "/dev/null:/dev/null"
	bindZero = "/dev/zero:/dev/zero"
)

// TestDaemon_DryRun_DetectsDeviceMount starts the daemon with -dry-run and
// -policy-mode=opt-in, creates a container with swarm-device-access.enable=true
// that bind-mounts /dev/null, and asserts the daemon grants that one device.
//
// No BPF syscalls or elevated privileges are required: -dry-run skips
// AddDeviceRules and logs intent instead.
func TestDaemon_DryRun_DetectsDeviceMount(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{bindNull})

	requireProcessed(ctx, t, proc, containerID, 1, 0)
}

// TestDaemon_DryRun_PolicyMode_SkipsUnlabelledContainer verifies that
// -policy-mode=opt-in does not apply rules for containers that lack the
// swarm-device-access.enable=true label.
func TestDaemon_DryRun_PolicyMode_SkipsUnlabelledContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	containerID := startTestContainer(ctx, t, cli, nil, []string{bindNull})

	requireSkipped(ctx, t, proc, containerID, msgSkippedPolicy)
}

// TestDaemon_DryRun_PolicyMode_All_ProcessesUnlabelledContainer verifies that
// -policy-mode=all processes containers that do not carry the opt-in label.
func TestDaemon_DryRun_PolicyMode_All_ProcessesUnlabelledContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=all")

	containerID := startTestContainer(ctx, t, cli, nil, []string{bindNull})

	requireProcessed(ctx, t, proc, containerID, 1, 0)
}

// TestDaemon_DryRun_PolicyMode_All_SkipsOptedOutContainer verifies that
// -policy-mode=all still skips containers that explicitly set enable=false.
func TestDaemon_DryRun_PolicyMode_All_SkipsOptedOutContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=all")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "false",
	}, []string{bindNull})

	requireSkipped(ctx, t, proc, containerID, msgSkippedPolicy)
}

// TestDaemon_DryRun_InvalidLabel_SkipsContainer verifies that a malformed
// policy label fails closed: the container is skipped, not granted.
func TestDaemon_DryRun_InvalidLabel_SkipsContainer(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=all")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "maybe",
	}, []string{bindNull})

	requireSkipped(ctx, t, proc, containerID, msgInvalidLabels)
}

// TestDaemon_DryRun_GlobalDeviceDeny_BlocksDevice verifies that a device
// matching the global -device-deny list is skipped, not granted.
func TestDaemon_DryRun_GlobalDeviceDeny_BlocksDevice(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in", "-device-deny=/dev/null")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{bindNull})

	requireProcessed(ctx, t, proc, containerID, 0, 1)
}

// TestDaemon_DryRun_GlobalDeviceAllow_BlocksNonMatchingDevice verifies that a
// device NOT in the global -device-allow list is skipped while a listed one is
// granted.
func TestDaemon_DryRun_GlobalDeviceAllow_BlocksNonMatchingDevice(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in", "-device-allow=/dev/zero")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{bindNull, bindZero})

	requireProcessed(ctx, t, proc, containerID, 1, 1)
}

// TestDaemon_DryRun_LabelDeviceDeny_BlocksDevice verifies that the
// swarm-device-access.device-deny container label filters out matching devices.
func TestDaemon_DryRun_LabelDeviceDeny_BlocksDevice(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable:     "true",
		policy.LabelDeviceDeny: "/dev/null",
	}, []string{bindNull})

	requireProcessed(ctx, t, proc, containerID, 0, 1)
}

// TestDaemon_DryRun_LabelDeviceAllow_NarrowsAccess verifies that the
// swarm-device-access.device-allow container label restricts which devices
// receive rules: a mount outside the per-container allow list is skipped.
func TestDaemon_DryRun_LabelDeviceAllow_NarrowsAccess(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable:      "true",
		policy.LabelDeviceAllow: "/dev/zero",
	}, []string{bindNull, bindZero})

	requireProcessed(ctx, t, proc, containerID, 1, 1)
}

// TestDaemon_DryRun_ProcessesExistingContainers verifies the startup-enumeration
// path: a container already running when the daemon starts must be processed by
// processExistingContainers without waiting for a Docker event.
func TestDaemon_DryRun_ProcessesExistingContainers(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)

	// Container starts BEFORE the daemon — no Docker event will fire for it.
	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{bindNull})

	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	requireProcessed(ctx, t, proc, containerID, 1, 0)
}

// TestDaemon_DryRun_UnpauseEvent verifies that an "unpause" Docker event
// triggers the same device-rule pipeline as a "start" event.
func TestDaemon_DryRun_UnpauseEvent(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{bindNull})

	requireProcessed(ctx, t, proc, containerID, 1, 0)

	_, err := cli.ContainerPause(ctx, containerID, dockerclient.ContainerPauseOptions{})
	if err != nil {
		t.Fatalf("pause container: %v", err)
	}

	_, err = cli.ContainerUnpause(ctx, containerID, dockerclient.ContainerUnpauseOptions{})
	if err != nil {
		t.Fatalf("unpause container: %v", err)
	}

	// The unpause event must trigger a second processing pass.
	records := proc.waitN(ctx, t, "second container processed (unpause)",
		forContainer(msgProcessed, containerID), 2)
	checkCounts(t, records[1], 1, 0)
}

// TestDaemon_DryRun_MultipleDeviceMounts verifies that every device bind-mount
// in a container is granted.
func TestDaemon_DryRun_MultipleDeviceMounts(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	proc := launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in")

	containerID := startTestContainer(ctx, t, cli, map[string]string{
		policy.LabelEnable: "true",
	}, []string{bindNull, bindZero})

	requireProcessed(ctx, t, proc, containerID, 2, 0)
}

// TestDaemon_DryRun_ConfigFile_LoadsPolicyMode verifies that daemon settings
// are correctly loaded from a YAML config file passed via -config, and that
// CLI defaults do not override file-only values.
func TestDaemon_DryRun_ConfigFile_LoadsPolicyMode(t *testing.T) {
	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)

	f, err := os.CreateTemp(t.TempDir(), "sda-config-*.yaml")
	if err != nil {
		t.Fatalf("create temp config: %v", err)
	}

	_, err = f.WriteString("policy-mode: \"all\"\ndry-run: true\n")
	if err != nil {
		t.Fatalf("write temp config: %v", err)
	}

	configPath := f.Name()
	_ = f.Close()

	// No -dry-run or -policy-mode flags on CLI — both come from the config file.
	proc := launchDaemon(ctx, t, "-config="+configPath)

	containerID := startTestContainer(ctx, t, cli, nil, []string{bindNull})

	rec := proc.wait(ctx, t, "container processed for "+shortID(containerID),
		forContainer(msgProcessed, containerID))
	checkCounts(t, rec, 1, 0)

	if dryRun, _ := rec["dry_run"].(bool); !dryRun {
		t.Errorf("config file dry-run: true not applied; record %v", rec)
	}
}

// TestDaemon_DryRun_MetricsEndpoint verifies that -metrics-addr starts a
// Prometheus-compatible HTTP endpoint that serves sda_ metrics.
func TestDaemon_DryRun_MetricsEndpoint(t *testing.T) {
	_ = requireDocker(t)

	ctx := testCtx(t)
	metricsAddr := freeLocalAddr(ctx, t)

	launchDaemon(ctx, t, "-dry-run", "-policy-mode=opt-in", "-metrics-addr="+metricsAddr)

	req, err := http.NewRequestWithContext(
		ctx,
		http.MethodGet,
		"http://"+metricsAddr+"/metrics",
		http.NoBody,
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
