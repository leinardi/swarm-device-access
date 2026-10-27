//go:build wirecapture

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

package wirecapture_test

import (
	"context"
	"io"
	"os"
	"path/filepath"
	"strings"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/launcher"
	"github.com/leinardi/swarm-device-access/internal/wirecapture"
)

const (
	// fakeDaemon is what the daemon container runs instead of the daemon: the launcher passes the
	// arguments after -- as the container's command, and busybox has no entrypoint. It writes to
	// both streams, so the attach carries both frame kinds, and exits 0 on SIGTERM. The ready line
	// is printed from a format, so the launcher's log of these arguments does not hold it.
	fakeDaemon = "trap 'echo fake daemon stopping; exit 0' TERM; printf 'fake %s ready\\n' daemon; " +
		"echo fake daemon stderr >&2; while :; do sleep 1; done"
	fakeReady = "fake daemon ready"

	// launcherBinary is where the launcher container sees the Go binary.
	launcherBinary = "/swarm-device-access"

	// binaryMode lets the launcher container run the copied binary.
	binaryMode = 0o755

	// launcherStopSeconds is the grace the launcher container gets, above its 20 s budget.
	launcherStopSeconds = 30
)

// TestCaptureLauncherWire runs the Go binary's launch mode against the proxy with fake inputs and
// records every Engine call it makes: its own inspect, the name-filtered list with an orphaned
// daemon in it, the inspect of the orphan's missing launcher (404), the forced remove, the create
// with the daemon spec, the attach upgrade with both stream kinds, the wait for removal, the
// start, and on SIGTERM the stop. The launcher runs in a busybox container with the static binary
// and the proxy socket bound in, so it identifies itself from its mount table as in production.
// The daemon container it creates from that image carries the real spec, so it is privileged,
// in the host namespaces and with the host's /dev, /sys and Docker socket, but it runs only
// fakeDaemon, from the digest-pinned busybox.
func TestCaptureLauncherWire(t *testing.T) {
	binary := os.Getenv(binaryEnv)
	if binary == "" {
		t.Fatalf("set %s to the Go daemon binary (make wire-capture does)", binaryEnv)
	}

	cli := wirecapture.Docker(t)
	version := wirecapture.APIVersion(t, cli)

	refuseExistingDaemon(t, cli)

	// A copy in the test's directory, so the launcher's inspect of itself names no checkout path.
	binary = copyBinary(t, binary)

	wirecapture.Create(t, cli, launcher.DaemonName,
		&container.Config{
			Image: wirecapture.TestImage,
			Cmd:   []string{"sleep", "300"},
			Labels: map[string]string{
				launcher.LabelRole:     launcher.RoleDaemon,
				launcher.LabelLauncher: strings.Repeat("0", 64),
			},
		},
		&container.HostConfig{},
	)

	proxy, err := wirecapture.Start(
		t.Context(),
		filepath.Join(t.TempDir(), "docker.sock"),
		wirecapture.Options{},
	)
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(proxy.Close)

	self := wirecapture.Create(t, cli, "sda-wire-launcher",
		&container.Config{
			Image:      wirecapture.TestImage,
			Entrypoint: []string{launcherBinary},
			Cmd:        []string{"launch", "-log-format=json", "--", "sh", "-c", fakeDaemon},
		},
		&container.HostConfig{Binds: []string{
			binary + ":" + launcherBinary + ":ro",
			proxy.Socket + ":" + wirecapture.DockerSocket,
		}},
	)

	t.Cleanup(func() { removeOwnedDaemon(t, cli, self) })

	wirecapture.StartContainer(t, cli, self)

	wirecapture.Eventually(t, stepTimeout, "the fake daemon's output", func() bool {
		return strings.Contains(containerLogs(t, cli, self), fakeReady)
	})

	ctx, cancel := context.WithTimeout(t.Context(), stepTimeout+launcherStopSeconds)
	defer cancel()

	stopSeconds := launcherStopSeconds

	_, err = cli.ContainerStop(ctx, self, client.ContainerStopOptions{Timeout: &stopSeconds})
	if err != nil {
		t.Fatalf("stop the launcher: %v", err)
	}

	inspected, err := cli.ContainerInspect(ctx, self, client.ContainerInspectOptions{})
	if err != nil {
		t.Fatal(err)
	}

	if code := inspected.Container.State.ExitCode; code != 0 {
		t.Fatalf("launcher exit code %d; output:\n%s", code, containerLogs(t, cli, self))
	}

	proxy.AwaitQuiet(t, settle)

	err = proxy.Write(wirecapture.Fixture(t, version, "launcher.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
}

// refuseExistingDaemon fails the test when a container already holds the daemon's name: the
// capture would make the launcher remove it.
func refuseExistingDaemon(t *testing.T, cli *client.Client) {
	t.Helper()

	ctx, cancel := context.WithTimeout(t.Context(), stepTimeout)
	defer cancel()

	list, err := cli.ContainerList(ctx, client.ContainerListOptions{
		All:     true,
		Filters: make(client.Filters).Add("name", "^/"+launcher.DaemonName+"$"),
	})
	if err != nil {
		t.Fatal(err)
	}

	if len(list.Items) > 0 {
		t.Fatalf("a %s container already exists (%s); the capture would remove it",
			launcher.DaemonName, list.Items[0].ID)
	}
}

// removeOwnedDaemon removes a daemon container the launcher left behind on a failed capture, only
// when this capture's launcher owns it.
func removeOwnedDaemon(t *testing.T, cli *client.Client, self string) {
	t.Helper()

	ctx, cancel := context.WithTimeout(context.WithoutCancel(t.Context()), stepTimeout)
	defer cancel()

	list, err := cli.ContainerList(ctx, client.ContainerListOptions{
		All:     true,
		Filters: make(client.Filters).Add("label", launcher.LabelLauncher+"="+self),
	})
	if err != nil {
		return
	}

	for index := range list.Items {
		_, _ = cli.ContainerRemove(
			ctx,
			list.Items[index].ID,
			client.ContainerRemoveOptions{Force: true},
		)
	}
}

// containerLogs is everything containerID has written so far, both streams demultiplexed.
func containerLogs(t *testing.T, cli *client.Client, containerID string) string {
	t.Helper()

	ctx, cancel := context.WithTimeout(t.Context(), stepTimeout)
	defer cancel()

	reader, err := cli.ContainerLogs(ctx, containerID, client.ContainerLogsOptions{
		ShowStdout: true,
		ShowStderr: true,
	})
	if err != nil {
		t.Fatal(err)
	}

	defer func() { _ = reader.Close() }()

	raw, err := io.ReadAll(reader)
	if err != nil {
		t.Fatal(err)
	}

	return string(raw)
}

// copyBinary copies the binary at path into the test's temporary directory.
func copyBinary(t *testing.T, path string) string {
	t.Helper()

	content, err := os.ReadFile(path)
	if err != nil {
		t.Fatal(err)
	}

	copied := filepath.Join(t.TempDir(), "swarm-device-access")

	err = os.WriteFile(copied, content, binaryMode)
	if err != nil {
		t.Fatal(err)
	}

	return copied
}
