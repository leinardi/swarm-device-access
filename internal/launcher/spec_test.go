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

package launcher

import (
	"errors"
	"maps"
	"reflect"
	"slices"
	"strings"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"
)

func TestDaemonSpec(t *testing.T) {
	t.Parallel()

	socket := mount.Mount{
		Type:   mount.TypeBind,
		Source: "/var/run/docker.sock",
		Target: "/var/run/docker.sock",
	}
	sysfs := mount.Mount{Type: mount.TypeBind, Source: "/sys", Target: "/host/sys"}
	dev := mount.Mount{Type: mount.TypeBind, Source: "/dev", Target: "/dev"}
	dbus := mount.Mount{
		Type:   mount.TypeBind,
		Source: "/run/dbus/system_bus_socket",
		Target: "/var/run/dbus/system_bus_socket",
	}

	cases := []struct {
		name        string
		opts        Options
		wantMounts  []mount.Mount
		wantNetwork container.NetworkMode
	}{
		{
			name:        "base",
			opts:        Options{HostDockerSocket: DefaultHostDockerSocket},
			wantMounts:  []mount.Mount{socket, sysfs, dev},
			wantNetwork: "none",
		},
		{
			name:        "dbus",
			opts:        Options{HostDockerSocket: DefaultHostDockerSocket, DBus: true},
			wantMounts:  []mount.Mount{socket, sysfs, dev, dbus},
			wantNetwork: "none",
		},
		{
			name: "config dir",
			opts: Options{HostDockerSocket: DefaultHostDockerSocket, ConfigDir: "/srv/sda"},
			wantMounts: []mount.Mount{socket, sysfs, dev, {
				Type:     mount.TypeBind,
				Source:   "/srv/sda",
				Target:   "/etc/swarm-device-access",
				ReadOnly: true,
			}},
			wantNetwork: "none",
		},
		{
			name:        "host network",
			opts:        Options{HostDockerSocket: DefaultHostDockerSocket, HostNetwork: true},
			wantMounts:  []mount.Mount{socket, sysfs, dev},
			wantNetwork: "host",
		},
		{
			name: "custom host docker socket",
			opts: Options{HostDockerSocket: "/run/user/0/docker.sock"},
			wantMounts: []mount.Mount{
				{
					Type:   mount.TypeBind,
					Source: "/run/user/0/docker.sock",
					Target: "/var/run/docker.sock",
				},
				sysfs,
				dev,
			},
			wantNetwork: "none",
		},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			spec := daemonSpec(&tc.opts, testImageID, testLauncherID)

			if spec.Name != "swarm-device-access" || spec.Image != "" {
				t.Errorf("name %q, image shortcut %q", spec.Name, spec.Image)
			}

			stopSeconds := 10
			wantHost := &container.HostConfig{
				Privileged:   true,
				CgroupnsMode: container.CgroupnsModeHost,
				PidMode:      "host",
				UsernsMode:   "host",
				NetworkMode:  tc.wantNetwork,
				AutoRemove:   true,
				Mounts:       tc.wantMounts,
			}

			if !reflect.DeepEqual(spec.HostConfig, wantHost) {
				t.Errorf("host config\n got %+v\nwant %+v", spec.HostConfig, wantHost)
			}

			wantConfig := &container.Config{
				Image: testImageID,
				Cmd:   nil,
				Labels: map[string]string{
					"io.github.leinardi.swarm-device-access.role":     "daemon",
					"io.github.leinardi.swarm-device-access.launcher": testLauncherID,
				},
				StopTimeout: &stopSeconds,
			}

			if !reflect.DeepEqual(spec.Config, wantConfig) {
				t.Errorf("config\n got %+v\nwant %+v", spec.Config, wantConfig)
			}
		})
	}
}

func TestDaemonSpec_ArgsPassedVerbatim(t *testing.T) {
	t.Parallel()

	args := []string{"-dry-run", "-device-allow", "/dev/sd*", "--", "-weird"}
	opts := Options{HostDockerSocket: DefaultHostDockerSocket, DaemonArgs: args}

	spec := daemonSpec(&opts, testImageID, testLauncherID)
	if !slices.Equal(spec.Config.Cmd, args) {
		t.Errorf("cmd = %v, want %v", spec.Config.Cmd, args)
	}

	args[0] = "changed"

	if spec.Config.Cmd[0] != "-dry-run" {
		t.Error("spec aliases the caller's argument slice")
	}
}

// The launcher's labels must not sit under the swarm-device-access. prefix,
// where the daemon reports unknown keys as bad policy labels.
func TestDaemonSpec_LabelsOutsidePolicyPrefix(t *testing.T) {
	t.Parallel()

	opts := Options{HostDockerSocket: DefaultHostDockerSocket}
	spec := daemonSpec(&opts, testImageID, testLauncherID)

	for _, key := range slices.Sorted(maps.Keys(spec.Config.Labels)) {
		if strings.HasPrefix(key, "swarm-device-access.") {
			t.Errorf("label %q is under the policy label prefix", key)
		}
	}
}

func TestParseSelfID(t *testing.T) {
	t.Parallel()

	const (
		dockerRoot = "1570 1511 0:420 / / rw,relatime - overlay root rw\n" +
			"1581 1570 8:2 /var/lib/docker/containers/" + testLauncherID + "/resolv.conf " +
			"/etc/resolv.conf rw,relatime - ext4 /dev/sda2 rw\n" +
			"1582 1570 8:2 /var/lib/docker/containers/" + testLauncherID + "/hostname " +
			"/etc/hostname rw,relatime - ext4 /dev/sda2 rw\n" +
			"1583 1570 8:2 /var/lib/docker/containers/" + testLauncherID + "/hosts " +
			"/etc/hosts rw,relatime - ext4 /dev/sda2 rw\n"
		trueNAS = "901 800 0:61 /ix-apps/docker/containers/" + testLauncherID + "/hostname " +
			"/etc/hostname rw,relatime - zfs boot-pool/.ix-apps rw\n"
		separateFS = "901 800 0:61 /docker/containers/" + testLauncherID + "/hosts " +
			"/etc/hosts rw - ext4 /dev/sdb1 rw\n"
		noMatch = "1570 1511 0:420 / / rw,relatime - overlay root rw\n" +
			"1581 1570 8:2 /etc/resolv.conf /etc/resolv.conf rw - ext4 /dev/sda2 rw\n"
		// Another container's file mounted elsewhere says nothing about us.
		otherTarget = "1581 1570 8:2 /var/lib/docker/containers/" + testDaemonID + "/hostname " +
			"/mnt/hostname rw - ext4 /dev/sda2 rw\n"
		ambiguous = "1582 1570 8:2 /var/lib/docker/containers/" + testLauncherID + "/hostname " +
			"/etc/hostname rw - ext4 /dev/sda2 rw\n" +
			"1583 1570 8:2 /var/lib/docker/containers/" + testDaemonID + "/hosts " +
			"/etc/hosts rw - ext4 /dev/sda2 rw\n"
	)

	cases := []struct {
		name    string
		table   string
		want    string
		wantErr error
	}{
		{name: "docker data-root", table: dockerRoot, want: testLauncherID},
		{name: "truenas data-root", table: trueNAS, want: testLauncherID},
		{name: "data-root on its own filesystem", table: separateFS, want: testLauncherID},
		{name: "no match", table: noMatch, wantErr: errSelfNotFound},
		{name: "other target", table: otherTarget, wantErr: errSelfNotFound},
		{name: "ambiguous", table: ambiguous, wantErr: errSelfAmbiguous},
	}

	for _, tc := range cases {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got, err := parseSelfID(strings.NewReader(tc.table))
			if !errors.Is(err, tc.wantErr) || got != tc.want {
				t.Errorf("parseSelfID = %q, %v; want %q, %v", got, err, tc.want, tc.wantErr)
			}
		})
	}
}
