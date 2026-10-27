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

//! The spec tests of `spec_test.go`, and the create body against the bodies a Go program
//! printed: moby's `ContainerCreate` request body for `daemonSpec`, marshalled by
//! `encoding/json` as moby v0.6.0 sends it.

use super::super::fake::{TEST_IMAGE_ID, TEST_LAUNCHER_ID};
use super::{DEFAULT_HOST_DOCKER_SOCKET, Mount, Options, daemon_spec};

fn bind(source: &str, target: &str) -> Mount {
    Mount {
        source: source.to_owned(),
        target: target.to_owned(),
        read_only: false,
    }
}

fn base() -> Options {
    Options {
        host_docker_socket: DEFAULT_HOST_DOCKER_SOCKET.to_owned(),
        ..Options::default()
    }
}

/// Go: `TestDaemonSpec`. The whole host config and config, field by field, for each option.
#[test]
fn daemon_spec_fields() {
    let socket = bind("/var/run/docker.sock", "/var/run/docker.sock");
    let sysfs = bind("/sys", "/host/sys");
    let dev = bind("/dev", "/dev");
    let dbus = bind(
        "/run/dbus/system_bus_socket",
        "/var/run/dbus/system_bus_socket",
    );
    let config = Mount {
        source: "/srv/sda".to_owned(),
        target: "/etc/swarm-device-access".to_owned(),
        read_only: true,
    };
    let cases = [
        (
            "base",
            base(),
            vec![socket.clone(), sysfs.clone(), dev.clone()],
            "none",
        ),
        (
            "dbus",
            Options {
                dbus: true,
                ..base()
            },
            vec![socket.clone(), sysfs.clone(), dev.clone(), dbus],
            "none",
        ),
        (
            "config dir",
            Options {
                config_dir: "/srv/sda".to_owned(),
                ..base()
            },
            vec![socket.clone(), sysfs.clone(), dev.clone(), config],
            "none",
        ),
        (
            "host network",
            Options {
                host_network: true,
                ..base()
            },
            vec![socket, sysfs.clone(), dev.clone()],
            "host",
        ),
        (
            "custom host docker socket",
            Options {
                host_docker_socket: "/run/user/0/docker.sock".to_owned(),
                ..Options::default()
            },
            vec![
                bind("/run/user/0/docker.sock", "/var/run/docker.sock"),
                sysfs,
                dev,
            ],
            "none",
        ),
    ];
    for (name, options, mounts, network) in cases {
        let spec = daemon_spec(&options, TEST_IMAGE_ID, TEST_LAUNCHER_ID);
        assert!(spec.privileged, "{name}");
        assert_eq!(spec.cgroupns_mode, "host", "{name}");
        assert_eq!(spec.pid_mode, "host", "{name}");
        assert_eq!(spec.userns_mode, "host", "{name}");
        assert_eq!(spec.network_mode, network, "{name}");
        assert!(spec.auto_remove, "{name}");
        assert_eq!(spec.mounts, mounts, "{name}");
        assert_eq!(spec.image, TEST_IMAGE_ID, "{name}");
        assert_eq!(spec.cmd, None, "{name}");
        assert_eq!(
            spec.labels.iter().collect::<Vec<_>>(),
            [
                (
                    &"io.github.leinardi.swarm-device-access.launcher".to_owned(),
                    &TEST_LAUNCHER_ID.to_owned()
                ),
                (
                    &"io.github.leinardi.swarm-device-access.role".to_owned(),
                    &"daemon".to_owned()
                ),
            ],
            "{name}"
        );
        assert_eq!(spec.stop_timeout, 10, "{name}");
    }
}

/// Go: `TestDaemonSpec_ArgsPassedVerbatim`. The arguments go to the daemon as given, and the
/// spec owns its copy.
#[test]
fn daemon_spec_args_passed_verbatim() {
    let args: Vec<String> = ["-dry-run", "-device-allow", "/dev/sd*", "--", "-weird"]
        .map(str::to_owned)
        .to_vec();
    let mut options = Options {
        daemon_args: Some(args.clone()),
        ..base()
    };
    let spec = daemon_spec(&options, TEST_IMAGE_ID, TEST_LAUNCHER_ID);
    assert_eq!(spec.cmd.as_deref(), Some(args.as_slice()));

    if let Some(given) = &mut options.daemon_args {
        "changed".clone_into(&mut given[0]);
    }
    assert_eq!(spec.cmd.unwrap()[0], "-dry-run");
}

/// Go: `TestDaemonSpec_LabelsOutsidePolicyPrefix`. The launcher's labels must not sit under the
/// `swarm-device-access.` prefix, where the daemon reports unknown keys as bad policy labels.
#[test]
fn daemon_spec_labels_outside_policy_prefix() {
    let spec = daemon_spec(&base(), TEST_IMAGE_ID, TEST_LAUNCHER_ID);
    for key in spec.labels.keys() {
        assert!(
            !key.starts_with("swarm-device-access."),
            "label {key:?} is under the policy label prefix"
        );
    }
}

/// The body up to the command, the image and the labels, then the host config up to the
/// network, as the Go program printed them.
const HEAD: &str = "{\"Hostname\":\"\",\"Domainname\":\"\",\"User\":\"\",\"AttachStdin\":false,\
    \"AttachStdout\":false,\"AttachStderr\":false,\"Tty\":false,\"OpenStdin\":false,\
    \"StdinOnce\":false,\"Env\":null,\"Cmd\":";
const AFTER_CMD: &str = ",\"Image\":\"sha256:2222222222222222222222222222222222222222222222222222222222222222\",\
    \"Volumes\":null,\"WorkingDir\":\"\",\"Entrypoint\":null,\"Labels\":{\
    \"io.github.leinardi.swarm-device-access.launcher\":\
    \"1111111111111111111111111111111111111111111111111111111111111111\",\
    \"io.github.leinardi.swarm-device-access.role\":\"daemon\"},\"StopTimeout\":10,\
    \"HostConfig\":{\"Binds\":null,\"ContainerIDFile\":\"\",\"LogConfig\":{\"Type\":\"\",\
    \"Config\":null},\"NetworkMode\":";
const AFTER_NETWORK: &str = ",\"PortBindings\":null,\"RestartPolicy\":{\"Name\":\"\",\
    \"MaximumRetryCount\":0},\"AutoRemove\":true,\"VolumeDriver\":\"\",\"VolumesFrom\":null,\
    \"ConsoleSize\":[0,0],\"CapAdd\":null,\"CapDrop\":null,\"CgroupnsMode\":\"host\",\
    \"Dns\":null,\"DnsOptions\":null,\"DnsSearch\":null,\"ExtraHosts\":null,\"GroupAdd\":null,\
    \"IpcMode\":\"\",\"Cgroup\":\"\",\"Links\":null,\"OomScoreAdj\":0,\"PidMode\":\"host\",\
    \"Privileged\":true,\"PublishAllPorts\":false,\"ReadonlyRootfs\":false,\
    \"SecurityOpt\":null,\"UTSMode\":\"\",\"UsernsMode\":\"host\",\"ShmSize\":0,\
    \"Isolation\":\"\",\"CpuShares\":0,\"Memory\":0,\"NanoCpus\":0,\"CgroupParent\":\"\",\
    \"BlkioWeight\":0,\"BlkioWeightDevice\":null,\"BlkioDeviceReadBps\":null,\
    \"BlkioDeviceWriteBps\":null,\"BlkioDeviceReadIOps\":null,\"BlkioDeviceWriteIOps\":null,\
    \"CpuPeriod\":0,\"CpuQuota\":0,\"CpuRealtimePeriod\":0,\"CpuRealtimeRuntime\":0,\
    \"CpusetCpus\":\"\",\"CpusetMems\":\"\",\"Devices\":null,\"DeviceCgroupRules\":null,\
    \"DeviceRequests\":null,\"MemoryReservation\":0,\"MemorySwap\":0,\
    \"MemorySwappiness\":null,\"OomKillDisable\":null,\"PidsLimit\":null,\"Ulimits\":null,\
    \"CpuCount\":0,\"CpuPercent\":0,\"IOMaximumIOps\":0,\"IOMaximumBandwidth\":0,\"Mounts\":";
const TAIL: &str = ",\"MaskedPaths\":null,\"ReadonlyPaths\":null}}";
const BASE_MOUNTS: &str = "[{\"Type\":\"bind\",\"Source\":\"/var/run/docker.sock\",\
    \"Target\":\"/var/run/docker.sock\"},{\"Type\":\"bind\",\"Source\":\"/sys\",\
    \"Target\":\"/host/sys\"},{\"Type\":\"bind\",\"Source\":\"/dev\",\"Target\":\"/dev\"}";

fn go_body(cmd: &str, network: &str, extra_mounts: &str) -> String {
    format!("{HEAD}{cmd}{AFTER_CMD}\"{network}\"{AFTER_NETWORK}{BASE_MOUNTS}{extra_mounts}]{TAIL}")
}

/// The create body is byte for byte the one Go sends: a nil command as `null`, an empty one as
/// `[]`, every option, and `encoding/json`'s escaping of the arguments (HTML characters, U+2028
/// and control characters as `\u` escapes, quotes and backslashes escaped).
#[test]
fn daemon_spec_body_matches_go() {
    let all = Options {
        dbus: true,
        config_dir: "/srv/sda".to_owned(),
        host_network: true,
        daemon_args: Some(
            [
                "-dry-run",
                "--device-allow",
                "/dev/<sd&>*",
                "\"q\"\\",
                "é\u{2028}\u{1}",
            ]
            .map(str::to_owned)
            .to_vec(),
        ),
        ..base()
    };
    let empty_args = Options {
        daemon_args: Some(Vec::new()),
        ..base()
    };
    let cases = [
        ("base", base(), go_body("null", "none", "")),
        (
            "all",
            all,
            go_body(
                "[\"-dry-run\",\"--device-allow\",\"/dev/\\u003csd\\u0026\\u003e*\",\
                 \"\\\"q\\\"\\\\\",\"é\\u2028\\u0001\"]",
                "host",
                ",{\"Type\":\"bind\",\"Source\":\"/run/dbus/system_bus_socket\",\
                 \"Target\":\"/var/run/dbus/system_bus_socket\"},{\"Type\":\"bind\",\
                 \"Source\":\"/srv/sda\",\"Target\":\"/etc/swarm-device-access\",\
                 \"ReadOnly\":true}",
            ),
        ),
        ("empty args", empty_args, go_body("[]", "none", "")),
    ];
    for (name, options, want) in cases {
        let spec = daemon_spec(&options, TEST_IMAGE_ID, TEST_LAUNCHER_ID);
        assert_eq!(spec.body(), want, "{name}");
    }
}
