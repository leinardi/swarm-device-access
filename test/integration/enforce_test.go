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

package integration

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"os/exec"
	"strings"
	"testing"
	"time"

	"github.com/moby/moby/api/pkg/stdcopy"
	"github.com/moby/moby/api/types/container"
	dockerclient "github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

const (
	// envRequireReload makes the reload subtest fail, instead of skip, on a
	// host where systemctl daemon-reload does not wipe the device programs.
	envRequireReload = "SDA_IT_REQUIRE_RELOAD"
	// envTestImage names the daemon image under test. The Make target builds
	// defaultTestImage when SDA_IT_ENFORCE=1.
	envTestImage     = "SDA_TEST_IMAGE"
	defaultTestImage = "swarm-device-access:integration"

	// Host paths the daemon container needs, as in .mk/docker-run.mk.
	hostDockerSocket = "/var/run/docker.sock"
	hostDBusSocket   = "/run/dbus/system_bus_socket"
	// dhi.io/static has no /var/run -> /run symlink, so the socket must be
	// mounted under /var/run inside the container.
	containerDBusSocket = "/var/run/dbus/system_bus_socket"

	msgWatcherStarted = "systemd reload watcher started"
	msgEPERM          = "Operation not permitted"

	// wipeTimeout bounds how long daemon-reload may take to remove the
	// device program after systemctl returns.
	wipeTimeout = 5 * time.Second
	// probePollInterval spaces the probes while waiting for the wipe.
	probePollInterval = 250 * time.Millisecond
	// reloadTimeout bounds one systemctl daemon-reload.
	reloadTimeout = 30 * time.Second
	// daemonStopTimeoutSec is the grace period before Docker kills the daemon.
	daemonStopTimeoutSec = 10
)

var (
	errNotDevice     = errors.New("not a device node")
	errControlDenied = errors.New(
		"control container given the device with --device cannot open it",
	)
	errOpenSucceeded  = errors.New("open succeeded, want " + msgEPERM)
	errWrongOpenError = errors.New("open failed with an error other than " + msgEPERM)
)

// enforceDevices are tried in order. Each sits outside Docker's default
// device cgroup allow list, and root with Docker's default capabilities can
// open it once the cgroup allows it, so the daemon's rule is the only thing
// that changes the probe's result. /dev/kmsg is not a candidate: without
// CAP_SYSLOG, dmesg_restrict can still deny it after the cgroup allows it.
var enforceDevices = []string{ //nolint:gochecknoglobals // read-only candidate list
	"/dev/loop-control", // char 10:237
	"/dev/loop0",        // block 7:0, opened read-only
}

// TestEnforce_GrantsDeviceAccess runs the daemon image for real (no -dry-run)
// and checks that an opted-in container can open a bind-mounted device only
// after the daemon attached its BPF program, that an unlabelled sibling still
// cannot, and (in the reload subtest) that the daemon re-applies the rule
// after systemctl daemon-reload wiped it.
//
// It attaches BPF programs and runs daemon-reload on this host, so it only
// runs with SDA_IT_ENFORCE=1.
func TestEnforce_GrantsDeviceAccess(t *testing.T) {
	if !enforceRequested() {
		t.Skipf(
			"set %s=1 to run: it attaches BPF programs and runs systemctl daemon-reload on this host",
			envEnforce,
		)
	}

	cli := requireDocker(t)
	ensureTestImage(t, cli)

	ctx := testCtx(t)
	image := requireDaemonImage(ctx, t, cli)

	requireEnforceHost(t)

	device := pickDevice(ctx, t, cli)
	bind := device + ":" + device

	target := startTestContainer(
		ctx,
		t,
		cli,
		map[string]string{policy.LabelEnable: "true"},
		[]string{bind},
	)
	requireDenied(ctx, t, cli, target, device, "labeled container before the daemon runs")

	daemonID, proc := startDaemonContainer(ctx, t, cli, image)

	requireProcessed(ctx, t, proc, target, 1, 0)
	requireAllowed(ctx, t, cli, target, device, "labeled container after the daemon granted it")

	sibling := startTestContainer(ctx, t, cli, nil, []string{bind})
	requireSkipped(ctx, t, proc, sibling, msgSkippedPolicy)
	requireDenied(ctx, t, cli, sibling, device, "unlabelled sibling")

	t.Run("reload", func(t *testing.T) {
		testReloadReapply(t, cli, image, daemonID, proc, target, device)
	})
}

// testReloadReapply first proves that daemon-reload wipes the device program
// on this host, then that the daemon puts it back. Without the wipe check a
// host that never wipes would pass the re-apply assertion without the daemon
// doing anything.
func testReloadReapply(
	t *testing.T,
	cli *dockerclient.Client,
	image, daemonID string,
	firstProc *daemonProc,
	target, device string,
) {
	t.Helper()

	ctx := testCtx(t)

	stopDaemonContainer(ctx, t, cli, daemonID, firstProc)

	// The program is attached with link.RawAttachProgram, not a link owned by
	// the daemon process, so it outlives the daemon.
	requireAllowed(ctx, t, cli, target, device, "labeled container after the daemon stopped")

	err := daemonReload(ctx)
	if err != nil {
		reloadUnsupported(t, err)
	}

	err = waitDenied(ctx, t, cli, target, device)
	if err != nil {
		reloadUnsupported(t, fmt.Errorf("daemon-reload did not wipe the device program: %w", err))
	}

	_, proc := startDaemonContainer(ctx, t, cli, image)
	proc.wait(ctx, t, "systemd reload watcher", withMsg(msgWatcherStarted))

	// Startup enumeration re-grants the container the reload stripped.
	requireProcessed(ctx, t, proc, target, 1, 0)
	requireAllowed(
		ctx,
		t,
		cli,
		target,
		device,
		"labeled container after the restarted daemon granted it",
	)

	processedTarget := forContainer(msgProcessed, target)
	before := proc.count(processedTarget)

	err = daemonReload(ctx)
	if err != nil {
		t.Fatalf("second daemon-reload failed after the first succeeded: %v", err)
	}

	// Wait for the re-apply pass's own record. The "re-applying device rules"
	// line is logged before the callback runs, so waiting on it would race the
	// re-apply.
	records := proc.waitN(ctx, t, "container processed after daemon-reload",
		processedTarget, before+1)
	checkCounts(t, records[before], 1, 0)

	// Exactly one probe, no retry: a failure means the re-apply did not take
	// or systemd wiped the rule again after it. Both are product bugs.
	requireAllowed(
		ctx,
		t,
		cli,
		target,
		device,
		"labeled container after daemon-reload and re-apply",
	)
}

// reloadUnsupported stops the reload subtest on a host where the reload
// scenario cannot prove anything. SDA_IT_REQUIRE_RELOAD=1 turns it into a
// failure so that CI cannot pass without reload coverage.
func reloadUnsupported(t *testing.T, err error) {
	t.Helper()

	if os.Getenv(envRequireReload) == "1" {
		t.Fatalf("reload scenario required by %s=1: %v", envRequireReload, err)
	}

	t.Skipf(
		"reload scenario not supported on this host (set %s=1 to fail instead): %v",
		envRequireReload,
		err,
	)
}

// daemonReload runs systemctl daemon-reload through non-interactive sudo.
func daemonReload(ctx context.Context) error {
	reloadCtx, cancel := context.WithTimeout(ctx, reloadTimeout)
	defer cancel()

	out, err := exec.CommandContext(reloadCtx, "sudo", "-n", "systemctl", "daemon-reload").
		CombinedOutput()
	if err != nil {
		return fmt.Errorf("sudo -n systemctl daemon-reload: %w: %s", err, bytes.TrimSpace(out))
	}

	return nil
}

// ---- prerequisites ----

func requireDaemonImage(ctx context.Context, t *testing.T, cli *dockerclient.Client) string {
	t.Helper()

	image := os.Getenv(envTestImage)
	if image == "" {
		image = defaultTestImage
	}

	_, err := cli.ImageInspect(ctx, image)
	if err != nil {
		requireOrSkip(t, enforceRequested(), fmt.Errorf(
			"daemon image %q (set %s or run make go-test-integration %s=1): %w",
			image,
			envTestImage,
			envEnforce,
			err,
		))
	}

	return image
}

// requireEnforceHost checks what the daemon container needs from the host:
// cgroup v2 for the BPF device filter, and systemd with its DBus socket for
// the reload watcher.
func requireEnforceHost(t *testing.T) {
	t.Helper()

	checks := []struct{ path, what string }{
		{"/sys/fs/cgroup/cgroup.controllers", "cgroup v2"},
		{"/run/systemd/system", "systemd as init"},
		{hostDBusSocket, "systemd DBus socket"},
	}

	for _, check := range checks {
		_, err := os.Stat(check.path)
		if err != nil {
			requireOrSkip(t, enforceRequested(), fmt.Errorf("%s: %w", check.what, err))
		}
	}
}

// pickDevice returns the first candidate that exists on the host as a device
// node and that a control container given it with --device can open. The
// control proves the probe fails only because of the device cgroup.
func pickDevice(ctx context.Context, t *testing.T, cli *dockerclient.Client) string {
	t.Helper()

	var problems []error

	for _, device := range enforceDevices {
		info, err := os.Stat(device)
		if err != nil {
			problems = append(problems, err)

			continue
		}

		if info.Mode()&os.ModeDevice == 0 {
			problems = append(
				problems,
				fmt.Errorf("%s (mode %s): %w", device, info.Mode(), errNotDevice),
			)

			continue
		}

		control := startContainer(ctx, t, cli,
			&container.Config{Image: testImage, Cmd: []string{"sleep", "300"}},
			&container.HostConfig{Resources: container.Resources{Devices: []container.DeviceMapping{
				{PathOnHost: device, PathInContainer: device, CgroupPermissions: "rwm"},
			}}})

		exitCode, stderr := probeOpen(ctx, t, cli, control, device)
		if exitCode != 0 {
			problems = append(
				problems,
				fmt.Errorf("%s: exit %d: %s: %w", device, exitCode, stderr, errControlDenied),
			)

			continue
		}

		t.Logf("using device %s", device)

		return device
	}

	requireOrSkip(
		t,
		enforceRequested(),
		fmt.Errorf("no usable device: %w", errors.Join(problems...)),
	)

	return ""
}

// ---- daemon container ----

// startDaemonContainer runs the daemon image with the runtime options of
// .mk/docker-run.mk, waits until it subscribes to Docker events and returns
// its ID and log collector. It is removed when the test ends.
func startDaemonContainer(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	image string,
) (string, *daemonProc) {
	t.Helper()

	daemonID := startContainer(ctx, t, cli,
		&container.Config{
			Image: image,
			Cmd:   []string{"-policy-mode=opt-in", "-log-format=json", "-log-level=debug"},
		},
		&container.HostConfig{
			Privileged:   true,
			CgroupnsMode: container.CgroupnsModeHost,
			PidMode:      "host",
			UsernsMode:   "host",
			Binds: []string{
				hostDockerSocket + ":/var/run/docker.sock",
				"/sys:/host/sys",
				// Without /dev the daemon cannot stat the device node and
				// never grants it.
				"/dev:/dev",
				hostDBusSocket + ":" + containerDBusSocket,
			},
		})

	logs, err := cli.ContainerLogs(ctx, daemonID, dockerclient.ContainerLogsOptions{
		ShowStdout: true,
		ShowStderr: true,
		Follow:     true,
	})
	if err != nil {
		t.Fatalf("follow daemon container logs: %v", err)
	}

	proc := newDaemonProc()
	reader, writer := io.Pipe()

	go func() {
		_, copyErr := stdcopy.StdCopy(writer, writer, logs)
		_ = logs.Close()
		_ = writer.CloseWithError(copyErr)
	}()

	go proc.collect(t.Log, reader)

	// Registered after startContainer's removal, so it runs first: the log
	// stream ends once the container is gone, and collect must be finished
	// before the test completes.
	t.Cleanup(func() {
		stopDaemonContainer(context.WithoutCancel(ctx), t, cli, daemonID, proc)
	})

	proc.waitReady(ctx, t)

	return daemonID, proc
}

// stopDaemonContainer stops the daemon and waits until its log stream ends.
// Stopping an already stopped container is a no-op.
func stopDaemonContainer(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	daemonID string,
	proc *daemonProc,
) {
	t.Helper()

	stopCtx, cancel := context.WithTimeout(ctx, cleanupTimeout+daemonStopTimeoutSec*time.Second)
	defer cancel()

	timeout := daemonStopTimeoutSec

	_, err := cli.ContainerStop(
		stopCtx,
		daemonID,
		dockerclient.ContainerStopOptions{Timeout: &timeout},
	)
	if err != nil {
		t.Logf("stop daemon container %s: %v", shortID(daemonID), err)
	}

	select {
	case <-proc.exited:
	case <-stopCtx.Done():
		t.Errorf("daemon container %s log stream did not end: %v", shortID(daemonID), stopCtx.Err())
	}
}

// ---- device probe ----

// probeOpen opens device read-only inside containerID and returns the exit
// code and stderr of the attempt.
func probeOpen(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID, device string,
) (exitCode int, stderrText string) {
	t.Helper()

	created, err := cli.ExecCreate(ctx, containerID, dockerclient.ExecCreateOptions{
		Cmd:          []string{"sh", "-c", `exec 3< "$1"`, "probe", device},
		AttachStdout: true,
		AttachStderr: true,
	})
	if err != nil {
		t.Fatalf("create probe exec in %s: %v", shortID(containerID), err)
	}

	attached, err := cli.ExecAttach(ctx, created.ID, dockerclient.ExecAttachOptions{})
	if err != nil {
		t.Fatalf("attach probe exec in %s: %v", shortID(containerID), err)
	}

	var stdout, stderr bytes.Buffer

	_, err = stdcopy.StdCopy(&stdout, &stderr, attached.Reader)

	attached.Close()

	if err != nil {
		t.Fatalf("read probe output in %s: %v", shortID(containerID), err)
	}

	// The stream ends when the process exits, but the exit code can lag.
	for {
		inspected, inspectErr := cli.ExecInspect(ctx, created.ID, dockerclient.ExecInspectOptions{})
		if inspectErr != nil {
			t.Fatalf("inspect probe exec in %s: %v", shortID(containerID), inspectErr)
		}

		if !inspected.Running {
			return inspected.ExitCode, strings.TrimSpace(stderr.String())
		}

		select {
		case <-ctx.Done():
			t.Fatalf("probe exec in %s still running: %v", shortID(containerID), context.Cause(ctx))
		case <-time.After(probePollInterval):
		}
	}
}

func requireAllowed(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID, device, what string,
) {
	t.Helper()

	exitCode, stderr := probeOpen(ctx, t, cli, containerID, device)
	if exitCode != 0 {
		t.Fatalf("%s: open %s failed (exit %d): %s", what, device, exitCode, stderr)
	}
}

func requireDenied(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID, device, what string,
) {
	t.Helper()

	err := checkDenied(ctx, t, cli, containerID, device)
	if err != nil {
		t.Fatalf("%s: %v", what, err)
	}
}

// checkDenied reports nil when opening device fails because the device cgroup
// denies it.
func checkDenied(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID, device string,
) error {
	t.Helper()

	exitCode, stderr := probeOpen(ctx, t, cli, containerID, device)
	if exitCode == 0 {
		return fmt.Errorf("%s: %w", device, errOpenSucceeded)
	}

	if !strings.Contains(stderr, msgEPERM) {
		return fmt.Errorf("%s: exit %d: %q: %w", device, exitCode, stderr, errWrongOpenError)
	}

	return nil
}

// waitDenied polls until opening device is denied or wipeTimeout passes.
func waitDenied(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID, device string,
) error {
	t.Helper()

	deadline := time.Now().Add(wipeTimeout)

	for {
		err := checkDenied(ctx, t, cli, containerID, device)
		if err == nil || !time.Now().Before(deadline) {
			return err
		}

		select {
		case <-ctx.Done():
			return fmt.Errorf("waiting for the wipe: %w", context.Cause(ctx))
		case <-time.After(probePollInterval):
		}
	}
}
