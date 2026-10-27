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

//! The daemon container, Go's `daemonSpec`: the privileged set the daemon needs and nothing
//! else, and the create body moby v0.6.0 sends for it.
//!
//! The body is written by hand, field by field in the order and form of Go's `json.Marshal` of
//! moby's `CreateRequest` (every field of `Config` and `HostConfig` that is not `omitempty`, at
//! its zero value, `null` for a nil slice or map), so the Engine receives the bytes the Go
//! launcher sent; the tests compare it with bodies a Go program printed and with the captured
//! create.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::docker::push_json_string;

/// The name of the daemon container. One name per node is what lets the next launcher find, and
/// replace, a daemon its predecessor left behind.
pub const DAEMON_NAME: &str = "swarm-device-access";

/// Marks the containers a launcher creates. Both labels sit outside the `swarm-device-access.`
/// prefix, where the daemon would report them as unknown policy labels.
pub const LABEL_ROLE: &str = "io.github.leinardi.swarm-device-access.role";

/// Holds the ID of the launcher container that owns the daemon.
pub const LABEL_LAUNCHER: &str = "io.github.leinardi.swarm-device-access.launcher";

/// The [`LABEL_ROLE`] value of a daemon container.
pub const ROLE_DAEMON: &str = "daemon";

/// The host path of the Docker socket bound into the daemon, by default.
pub const DEFAULT_HOST_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// Where both the launcher and the daemon find the Docker socket: the daemon's
/// `--docker-socket` default.
pub const CONTAINER_DOCKER_SOCKET: &str = "/var/run/docker.sock";

/// Where `--config-dir` is mounted in the daemon.
pub const CONTAINER_CONFIG_DIR: &str = "/etc/swarm-device-access";

/// How long Docker waits after `SIGTERM` before it kills the daemon.
pub const STOP_TIMEOUT: Duration = Duration::from_secs(10);

const HOST_SYSFS: &str = "/sys";
const CONTAINER_SYSFS: &str = "/host/sys";
const DEV_PATH: &str = "/dev";
const HOST_DBUS_SOCKET: &str = "/run/dbus/system_bus_socket";
/// `dhi.io/static` has no `/var/run -> /run` symlink, so the socket must be mounted under
/// `/var/run` inside the container.
const CONTAINER_DBUS_SOCKET: &str = "/var/run/dbus/system_bus_socket";

/// Shares a namespace (PID, user, network, cgroup) with the host.
const HOST_MODE: &str = "host";
const NETWORK_NONE: &str = "none";

/// The daemon container a launcher creates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Options {
    /// The host path of the Docker socket bound into the daemon. The launcher itself always uses
    /// its own `/var/run/docker.sock`.
    pub host_docker_socket: String,
    /// Binds the host's system bus socket, which enables the systemd reload watcher.
    pub dbus: bool,
    /// When not empty, a host directory bound read-only at [`CONTAINER_CONFIG_DIR`].
    pub config_dir: String,
    /// Runs the daemon in the host network namespace instead of none; only the metrics server
    /// needs a network.
    pub host_network: bool,
    /// Passed to the daemon verbatim; it validates them. `None` is Go's nil slice, a `null`
    /// command; the launch subcommand always passes a list.
    pub daemon_args: Option<Vec<String>>,
}

/// A bind mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Mount {
    /// The host path.
    pub source: String,
    /// The path in the container.
    pub target: String,
    /// Mounted read-only.
    pub read_only: bool,
}

/// The daemon container: the fields the Go spec sets, every other one at Go's zero value.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DaemonSpec {
    /// `Config.Image`: the launcher's content-addressed image ID.
    pub image: String,
    /// `Config.Cmd`.
    pub cmd: Option<Vec<String>>,
    /// `Config.Labels`.
    pub labels: BTreeMap<String, String>,
    /// `Config.StopTimeout`, in seconds.
    pub stop_timeout: u64,
    /// `HostConfig.Privileged`.
    pub privileged: bool,
    /// `HostConfig.CgroupnsMode`.
    pub cgroupns_mode: &'static str,
    /// `HostConfig.PidMode`.
    pub pid_mode: &'static str,
    /// `HostConfig.UsernsMode`.
    pub userns_mode: &'static str,
    /// `HostConfig.NetworkMode`.
    pub network_mode: &'static str,
    /// `HostConfig.AutoRemove`.
    pub auto_remove: bool,
    /// `HostConfig.Mounts`, all of type `bind`.
    pub mounts: Vec<Mount>,
}

/// Go's `daemonSpec`: the complete daemon container. Bind sources are given as mounts, not
/// binds, so a missing host path fails the create instead of Docker creating an empty directory
/// in its place.
#[must_use]
pub fn daemon_spec(options: &Options, image_id: &str, launcher_id: &str) -> DaemonSpec {
    let mut mounts = vec![
        bind(&options.host_docker_socket, CONTAINER_DOCKER_SOCKET, false),
        bind(HOST_SYSFS, CONTAINER_SYSFS, false),
        bind(DEV_PATH, DEV_PATH, false),
    ];
    if options.dbus {
        mounts.push(bind(HOST_DBUS_SOCKET, CONTAINER_DBUS_SOCKET, false));
    }
    if !options.config_dir.is_empty() {
        mounts.push(bind(&options.config_dir, CONTAINER_CONFIG_DIR, true));
    }
    DaemonSpec {
        image: image_id.to_owned(),
        cmd: options.daemon_args.clone(),
        labels: BTreeMap::from([
            (LABEL_ROLE.to_owned(), ROLE_DAEMON.to_owned()),
            (LABEL_LAUNCHER.to_owned(), launcher_id.to_owned()),
        ]),
        stop_timeout: STOP_TIMEOUT.as_secs(),
        privileged: true,
        cgroupns_mode: HOST_MODE,
        pid_mode: HOST_MODE,
        userns_mode: HOST_MODE,
        network_mode: if options.host_network {
            HOST_MODE
        } else {
            NETWORK_NONE
        },
        auto_remove: true,
        mounts,
    }
}

fn bind(source: &str, target: &str, read_only: bool) -> Mount {
    Mount {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only,
    }
}

impl DaemonSpec {
    /// The create body: Go's `json.Marshal` of moby's `CreateRequest` for this spec.
    #[must_use]
    pub fn body(&self) -> String {
        let mut out = String::from(
            "{\"Hostname\":\"\",\"Domainname\":\"\",\"User\":\"\",\"AttachStdin\":false,\
             \"AttachStdout\":false,\"AttachStderr\":false,\"Tty\":false,\"OpenStdin\":false,\
             \"StdinOnce\":false,\"Env\":null,\"Cmd\":",
        );
        match &self.cmd {
            None => out.push_str("null"),
            Some(args) => push_json_strings(&mut out, args),
        }
        out.push_str(",\"Image\":");
        push_json_string(&mut out, &self.image);
        out.push_str(",\"Volumes\":null,\"WorkingDir\":\"\",\"Entrypoint\":null,\"Labels\":{");
        for (index, (key, value)) in self.labels.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            push_json_string(&mut out, key);
            out.push(':');
            push_json_string(&mut out, value);
        }
        out.push_str("},\"StopTimeout\":");
        out.push_str(&self.stop_timeout.to_string());
        out.push_str(
            ",\"HostConfig\":{\"Binds\":null,\"ContainerIDFile\":\"\",\
             \"LogConfig\":{\"Type\":\"\",\"Config\":null},\"NetworkMode\":",
        );
        push_json_string(&mut out, self.network_mode);
        out.push_str(
            ",\"PortBindings\":null,\"RestartPolicy\":{\"Name\":\"\",\"MaximumRetryCount\":0},\
             \"AutoRemove\":",
        );
        out.push_str(json_bool(self.auto_remove));
        out.push_str(
            ",\"VolumeDriver\":\"\",\"VolumesFrom\":null,\"ConsoleSize\":[0,0],\"CapAdd\":null,\
             \"CapDrop\":null,\"CgroupnsMode\":",
        );
        push_json_string(&mut out, self.cgroupns_mode);
        out.push_str(
            ",\"Dns\":null,\"DnsOptions\":null,\"DnsSearch\":null,\"ExtraHosts\":null,\
             \"GroupAdd\":null,\"IpcMode\":\"\",\"Cgroup\":\"\",\"Links\":null,\"OomScoreAdj\":0,\
             \"PidMode\":",
        );
        push_json_string(&mut out, self.pid_mode);
        out.push_str(",\"Privileged\":");
        out.push_str(json_bool(self.privileged));
        out.push_str(
            ",\"PublishAllPorts\":false,\"ReadonlyRootfs\":false,\"SecurityOpt\":null,\
             \"UTSMode\":\"\",\"UsernsMode\":",
        );
        push_json_string(&mut out, self.userns_mode);
        out.push_str(
            ",\"ShmSize\":0,\"Isolation\":\"\",\"CpuShares\":0,\"Memory\":0,\"NanoCpus\":0,\
             \"CgroupParent\":\"\",\"BlkioWeight\":0,\"BlkioWeightDevice\":null,\
             \"BlkioDeviceReadBps\":null,\"BlkioDeviceWriteBps\":null,\
             \"BlkioDeviceReadIOps\":null,\"BlkioDeviceWriteIOps\":null,\"CpuPeriod\":0,\
             \"CpuQuota\":0,\"CpuRealtimePeriod\":0,\"CpuRealtimeRuntime\":0,\
             \"CpusetCpus\":\"\",\"CpusetMems\":\"\",\"Devices\":null,\
             \"DeviceCgroupRules\":null,\"DeviceRequests\":null,\"MemoryReservation\":0,\
             \"MemorySwap\":0,\"MemorySwappiness\":null,\"OomKillDisable\":null,\
             \"PidsLimit\":null,\"Ulimits\":null,\"CpuCount\":0,\"CpuPercent\":0,\
             \"IOMaximumIOps\":0,\"IOMaximumBandwidth\":0,\"Mounts\":[",
        );
        for (index, mount) in self.mounts.iter().enumerate() {
            if index > 0 {
                out.push(',');
            }
            out.push_str("{\"Type\":\"bind\",\"Source\":");
            push_json_string(&mut out, &mount.source);
            out.push_str(",\"Target\":");
            push_json_string(&mut out, &mount.target);
            if mount.read_only {
                out.push_str(",\"ReadOnly\":true");
            }
            out.push('}');
        }
        out.push_str("],\"MaskedPaths\":null,\"ReadonlyPaths\":null}}");
        out
    }
}

fn push_json_strings(out: &mut String, values: &[String]) {
    out.push('[');
    for (index, value) in values.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        push_json_string(out, value);
    }
    out.push(']');
}

const fn json_bool(value: bool) -> &'static str {
    if value { "true" } else { "false" }
}

#[cfg(test)]
mod tests;
