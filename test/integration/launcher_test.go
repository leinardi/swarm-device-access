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
	"context"
	"slices"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"
	dockerclient "github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/launcher"
)

// launcherStopTimeoutSec matches the compose files' stop_grace_period: the
// launcher needs up to its shutdown budget to stop the daemon.
const launcherStopTimeoutSec = 30

// TestLauncher_RunsDaemonFromItsOwnImage starts the image in launcher mode
// with only the Docker socket, and checks the daemon container it creates,
// a graceful stop, and the replacement of a daemon orphaned by a killed
// launcher.
//
// It needs the daemon image and creates a privileged container named
// swarm-device-access, so it runs with the enforcement tests.
func TestLauncher_RunsDaemonFromItsOwnImage(t *testing.T) {
	if !enforceRequested() {
		t.Skipf(
			"set %s=1 to run: it creates a privileged swarm-device-access container on this host",
			envEnforce,
		)
	}

	cli := requireDocker(t)
	ctx := testCtx(t)
	image := requireDaemonImage(ctx, t, cli)

	if daemons := listDaemons(ctx, t, cli); len(daemons) > 0 {
		t.Fatalf(
			"a %s container already exists (%s); remove it first",
			launcher.DaemonName,
			shortID(daemons[0].ID),
		)
	}

	t.Cleanup(func() { removeTestDaemons(ctx, t, cli) })

	t.Run("spec and graceful stop", func(t *testing.T) {
		launcherID := startLauncher(ctx, t, cli, image)
		requireDaemonSpec(ctx, t, cli, launcherID)

		timeout := launcherStopTimeoutSec

		_, err := cli.ContainerStop(
			ctx,
			launcherID,
			dockerclient.ContainerStopOptions{Timeout: &timeout},
		)
		if err != nil {
			t.Fatalf("stop launcher: %v", err)
		}

		status := waitExit(ctx, t, cli, launcherID)
		if status != 0 {
			t.Errorf("launcher exit status %d, want 0", status)
		}

		if daemons := listDaemons(ctx, t, cli); len(daemons) > 0 {
			t.Errorf("daemon %s still exists after its launcher stopped", shortID(daemons[0].ID))
		}
	})

	t.Run("orphan replaced", func(t *testing.T) {
		first := startLauncher(ctx, t, cli, image)

		_, err := cli.ContainerKill(ctx, first, dockerclient.ContainerKillOptions{Signal: "KILL"})
		if err != nil {
			t.Fatalf("kill launcher: %v", err)
		}

		waitExit(ctx, t, cli, first)

		second := startLauncher(ctx, t, cli, image)

		daemons := listDaemons(ctx, t, cli)
		if len(daemons) != 1 || daemons[0].Labels[launcher.LabelLauncher] != second {
			t.Fatalf(
				"daemons %+v, want exactly one, owned by launcher %s",
				daemons,
				shortID(second),
			)
		}
	})
}

// startLauncher runs the image in launcher mode with only the Docker socket
// mounted and a dry-run daemon behind it, and waits until the daemon's
// startup, as seen in the launcher's own output, is complete.
func startLauncher(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	image string,
) string {
	t.Helper()

	launcherID := startContainer(ctx, t, cli,
		&container.Config{
			Image: image,
			Cmd: []string{
				"launch", "-log-format=json",
				"--", "-dry-run", "-log-format=json", "-log-level=debug",
			},
		},
		&container.HostConfig{Binds: []string{hostDockerSocket + ":/var/run/docker.sock"}},
	)

	followLogs(ctx, t, cli, launcherID).waitReady(ctx, t)

	return launcherID
}

// requireDaemonSpec checks the daemon container against the documented
// privileged set: the launcher's image, host namespaces, no network, and
// exactly the three bind mounts, given as Mounts.
func requireDaemonSpec(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	launcherID string,
) {
	t.Helper()

	self, err := cli.ContainerInspect(ctx, launcherID, dockerclient.ContainerInspectOptions{})
	if err != nil {
		t.Fatalf("inspect launcher: %v", err)
	}

	res, err := cli.ContainerInspect(
		ctx,
		launcher.DaemonName,
		dockerclient.ContainerInspectOptions{},
	)
	if err != nil {
		t.Fatalf("inspect daemon: %v", err)
	}

	daemon := res.Container
	host := daemon.HostConfig

	if daemon.Image != self.Container.Image {
		t.Errorf("daemon image %s, want the launcher's %s", daemon.Image, self.Container.Image)
	}

	if daemon.Config.Labels[launcher.LabelLauncher] != launcherID {
		t.Errorf(
			"daemon launcher label %q, want %s",
			daemon.Config.Labels[launcher.LabelLauncher],
			launcherID,
		)
	}

	type hostSpec struct {
		privileged                 bool
		cgroupns, pid, userns, net string
		autoRemove                 bool
	}

	gotHost := hostSpec{
		privileged: host.Privileged,
		cgroupns:   string(host.CgroupnsMode),
		pid:        string(host.PidMode),
		userns:     string(host.UsernsMode),
		net:        string(host.NetworkMode),
		autoRemove: host.AutoRemove,
	}

	wantHost := hostSpec{true, "host", "host", "host", "none", true}
	if gotHost != wantHost {
		t.Errorf("host config %+v, want %+v", gotHost, wantHost)
	}

	if len(host.Binds) > 0 {
		t.Errorf("binds %v, want Mounts only", host.Binds)
	}

	want := []string{"/var/run/docker.sock:/var/run/docker.sock", "/sys:/host/sys", "/dev:/dev"}

	got := make([]string, 0, len(host.Mounts))

	for _, mnt := range host.Mounts {
		if mnt.Type != mount.TypeBind {
			t.Errorf("mount %s is %s, want bind", mnt.Target, mnt.Type)
		}

		got = append(got, mnt.Source+":"+mnt.Target)
	}

	if !slices.Equal(got, want) {
		t.Errorf("mounts %v, want %v", got, want)
	}
}

// listDaemons returns every container named swarm-device-access.
func listDaemons(ctx context.Context, t *testing.T, cli *dockerclient.Client) []container.Summary {
	t.Helper()

	res, err := cli.ContainerList(ctx, dockerclient.ContainerListOptions{
		All:     true,
		Filters: make(dockerclient.Filters).Add("name", "^/"+launcher.DaemonName+"$"),
	})
	if err != nil {
		t.Fatalf("list daemon containers: %v", err)
	}

	return res.Items
}

// waitExit waits until containerID has stopped and returns its exit status.
func waitExit(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID string,
) int64 {
	t.Helper()

	waitCtx, cancel := context.WithTimeout(ctx, launcherStopTimeoutSec*time.Second+cleanupTimeout)
	defer cancel()

	res := cli.ContainerWait(waitCtx, containerID, dockerclient.ContainerWaitOptions{
		Condition: container.WaitConditionNotRunning,
	})

	select {
	case exit := <-res.Result:
		return exit.StatusCode
	case err := <-res.Error:
		t.Fatalf("wait for %s: %v", shortID(containerID), err)
	}

	return 0
}

// removeTestDaemons removes a daemon a failed test left behind. Its
// launchers are this run's containers, removed by their own cleanups.
func removeTestDaemons(ctx context.Context, t *testing.T, cli *dockerclient.Client) {
	t.Helper()

	listCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), cleanupTimeout)
	defer cancel()

	daemons := listDaemons(listCtx, t, cli)
	for i := range daemons {
		if daemons[i].Labels[launcher.LabelRole] == launcher.RoleDaemon {
			removeContainer(ctx, t, cli, daemons[i].ID)
		}
	}
}
