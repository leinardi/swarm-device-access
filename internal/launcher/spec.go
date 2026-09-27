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
	"slices"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"
	"github.com/moby/moby/client"
)

const (
	// DaemonName is the name of the daemon container. One name per node is
	// what lets the next launcher find, and replace, a daemon its
	// predecessor left behind.
	DaemonName = "swarm-device-access"

	// LabelRole marks the containers a launcher creates. Both labels sit
	// outside the swarm-device-access. prefix, where the daemon would report
	// them as unknown policy labels.
	LabelRole = "io.github.leinardi.swarm-device-access.role"
	// LabelLauncher holds the ID of the launcher container that owns the
	// daemon.
	LabelLauncher = "io.github.leinardi.swarm-device-access.launcher"
	// RoleDaemon is the LabelRole value of a daemon container.
	RoleDaemon = "daemon"

	// DefaultHostDockerSocket is the host path of the Docker socket bound
	// into the daemon.
	DefaultHostDockerSocket = "/var/run/docker.sock"

	// containerDockerSocket is where both the launcher and the daemon find
	// the Docker socket: the daemon's -docker-socket default.
	containerDockerSocket = "/var/run/docker.sock"
	hostSysfs             = "/sys"
	containerSysfs        = "/host/sys"
	devPath               = "/dev"
	hostDBusSocket        = "/run/dbus/system_bus_socket"
	// dhi.io/static has no /var/run -> /run symlink, so the socket must be
	// mounted under /var/run inside the container.
	containerDBusSocket = "/var/run/dbus/system_bus_socket"
	// ContainerConfigDir is where -config-dir is mounted in the daemon.
	ContainerConfigDir = "/etc/swarm-device-access"

	// hostMode shares a namespace (pid, user, network) with the host.
	hostMode    = "host"
	networkNone = "none"

	// stopTimeout is how long Docker waits after SIGTERM before it kills
	// the daemon.
	stopTimeout = 10 * time.Second
)

// Options describe the daemon container a launcher creates.
type Options struct {
	// HostDockerSocket is the host path of the Docker socket bound into the
	// daemon. The launcher itself always uses its own /var/run/docker.sock.
	HostDockerSocket string
	// DBus binds the host's system bus socket, which enables the systemd
	// reload watcher.
	DBus bool
	// ConfigDir, when set, is a host directory bound read-only at
	// ContainerConfigDir.
	ConfigDir string
	// HostNetwork runs the daemon in the host network namespace instead of
	// none; only the metrics and debug servers need a network.
	HostNetwork bool
	// DaemonArgs are passed to the daemon verbatim; it validates them.
	DaemonArgs []string
}

// daemonSpec is the complete daemon container: the privileged set the
// daemon needs and nothing else. Bind sources are given as Mounts, not
// Binds, so a missing host path fails the create instead of Docker creating
// an empty directory in its place.
func daemonSpec(opts *Options, imageID, launcherID string) client.ContainerCreateOptions {
	stopSeconds := int(stopTimeout / time.Second)

	mounts := []mount.Mount{
		bind(opts.HostDockerSocket, containerDockerSocket, false),
		bind(hostSysfs, containerSysfs, false),
		bind(devPath, devPath, false),
	}

	if opts.DBus {
		mounts = append(mounts, bind(hostDBusSocket, containerDBusSocket, false))
	}

	if opts.ConfigDir != "" {
		mounts = append(mounts, bind(opts.ConfigDir, ContainerConfigDir, true))
	}

	network := container.NetworkMode(networkNone)
	if opts.HostNetwork {
		network = hostMode
	}

	return client.ContainerCreateOptions{
		Name: DaemonName,
		Config: &container.Config{
			Image: imageID,
			Cmd:   slices.Clone(opts.DaemonArgs),
			Labels: map[string]string{
				LabelRole:     RoleDaemon,
				LabelLauncher: launcherID,
			},
			StopTimeout: &stopSeconds,
		},
		HostConfig: &container.HostConfig{
			Privileged:   true,
			CgroupnsMode: container.CgroupnsModeHost,
			PidMode:      hostMode,
			UsernsMode:   hostMode,
			NetworkMode:  network,
			AutoRemove:   true,
			Mounts:       mounts,
		},
	}
}

func bind(source, target string, readOnly bool) mount.Mount {
	return mount.Mount{Type: mount.TypeBind, Source: source, Target: target, ReadOnly: readOnly}
}
