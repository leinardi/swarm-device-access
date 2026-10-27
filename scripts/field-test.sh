#!/usr/bin/env bash
#
# Copyright 2026 Roberto Leinardi.
#
# Licensed under the Apache License, Version 2.0 (the "License");
# you may not use this file except in compliance with the License.
# You may obtain a copy of the License at
#
#     https://www.apache.org/licenses/LICENSE-2.0
#
# Unless required by applicable law or agreed to in writing, software
# distributed under the License is distributed on an "AS IS" BASIS,
# WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
# See the License for the specific language governing permissions and
# limitations under the License.
#
# Field test: hands one Swarm node from the deployed 1.x (Go) daemon to the Rust daemon and back,
# with real device access checked from inside Swarm tasks at every step. docs/testing.md
# ("Field test") is the procedure; this script is its root part, run on the node through
# systemd-run so an ssh drop does not end it halfway.
#
# The deployed Go service is only ever kept off this node, with a placement constraint on its node
# ID, never changed otherwise, so other nodes keep running it. Both daemons under test run as
# services of their own, pinned to this node, in launch mode with the deployed service's mounts and
# arguments (in double-dash spelling for Rust). The cleanup trap removes every service the script
# created and lifts the constraint, whatever step failed.
#
# Environment:
#   SDA_FIELD_RUST_IMAGE  the Rust image, already loaded on this node (required)
#   SDA_FIELD_GO_SERVICE  the deployed Go service (default swarm-device-access)
#   SDA_FIELD_MOUNT       the /dev directory the test tasks bind-mount (default /dev/dri)
#   SDA_FIELD_DEVICE      the device under it they open (default the first renderD* or bus device)
#   SDA_FIELD_TEST_IMAGE  the tasks' image (default docker.io/library/busybox:1.36)

set -euo pipefail

readonly GO_SERVICE="${SDA_FIELD_GO_SERVICE:-swarm-device-access}"
readonly MOUNT="${SDA_FIELD_MOUNT:-/dev/dri}"
readonly TEST_IMAGE="${SDA_FIELD_TEST_IMAGE:-docker.io/library/busybox:1.36}"
readonly PREFIX="sda-field"
readonly RUST_DAEMON_SERVICE="${PREFIX}-rust"
readonly GO_DAEMON_SERVICE="${PREFIX}-go"
readonly DAEMON_CONTAINER="swarm-device-access"
# How long a change may take to show inside a task: a daemon start, a reconcile, a reload.
readonly SETTLE_SECONDS=60
# Where the daemons' logs are kept: beside this script, under /dev/shm in the procedure.
LOG_DIR=$(dirname "$(readlink -f "$0")")
readonly LOG_DIR

failures=0
constraint=""
created_services=()

log() { printf '%s %s\n' "$(date -u +%H:%M:%S)" "$*"; }
pass() { log "PASS $*"; }
fail() {
  log "FAIL $*"
  failures=$((failures + 1))
}
die() {
  log "ABORT $*"
  exit 1
}

require() {
  [[ ${EUID} -eq 0 ]] || die "run as root (through sudo systemd-run, see docs/testing.md)"
  [[ -n ${SDA_FIELD_RUST_IMAGE:-} ]] || die "SDA_FIELD_RUST_IMAGE is not set"
  command -v jq >/dev/null || die "jq is required"
  docker image inspect "${SDA_FIELD_RUST_IMAGE}" >/dev/null || die "load ${SDA_FIELD_RUST_IMAGE} first"
  [[ $(docker info --format '{{.Swarm.LocalNodeState}}') == active ]] || die "this node is not in a swarm"
  docker service inspect "${GO_SERVICE}" >/dev/null || die "no service ${GO_SERVICE}"
  [[ -d ${MOUNT} ]] || die "${MOUNT} is not a directory"
}

pick_device() {
  if [[ -n ${SDA_FIELD_DEVICE:-} ]]; then
    printf '%s\n' "${SDA_FIELD_DEVICE}"
    return
  fi
  local candidate
  for candidate in "${MOUNT}"/renderD* "${MOUNT}"/* "${MOUNT}"/*/*; do
    if [[ -c ${candidate} ]]; then
      printf '%s\n' "${candidate}"
      return
    fi
  done
  die "no character device under ${MOUNT}; set SDA_FIELD_DEVICE"
}

# The task container of a service on this node.
task_container() {
  docker ps -q --filter "label=com.docker.swarm.service.name=$1" | head -n 1
}

# Prints the task's access to the device: allowed when it opens it for reading, denied only when
# the open fails with the device cgroup's EPERM, and error with the output on anything else (a
# failed exec, a missing path), which must never pass for a revocation.
access_of() {
  local output
  if output=$(docker exec "$1" sh -c 'exec 3< "$1"' sh "${DEVICE}" 2>&1); then
    printf 'allowed\n'
  elif [[ ${output} == *"Operation not permitted"* ]]; then
    printf 'denied\n'
  else
    printf 'error: %s\n' "${output//$'\n'/ }"
  fi
}

# Waits until the task's access is the expected one (allowed or denied). The task's container
# must stay the one it was when the step began: a replaced task would be judged on a fresh grant.
expect_access() {
  local name=$1 want=$2 step=$3 container deadline access=""
  container=$(task_container "${name}")
  [[ -n ${container} ]] || {
    fail "${step}: no task container for ${name}"
    return
  }
  deadline=$((SECONDS + SETTLE_SECONDS))
  while ((SECONDS < deadline)); do
    if [[ $(task_container "${name}") != "${container}" ]]; then
      fail "${step}: the task container of ${name} changed from ${container}"
      return
    fi
    access=$(access_of "${container}")
    if [[ ${access} == "${want}" ]]; then
      pass "${step}: ${name} access to ${DEVICE} is ${want}"
      return
    fi
    sleep 2
  done
  fail "${step}: ${name} access to ${DEVICE} is ${access}, not ${want}, after ${SETTLE_SECONDS}s"
}

# A cursor for expect_logged: the daemon's log lines after this instant.
log_cursor() { date -u +%Y-%m-%dT%H:%M:%S.%NZ; }

# Under pipefail a grep -q at the end of a pipe would fail it: it exits on the first match and the
# writers before it die of SIGPIPE, so the last grep reads everything instead.
#
# Waits for a daemon log line after the cursor that carries both the message (an extended regular
# expression, so a step can accept any of several lines) and the container ID, in the text and the
# JSON format alike.
expect_logged() {
  local cursor=$1 message=$2 container=$3 step=$4 deadline=$((SECONDS + SETTLE_SECONDS))
  while ((SECONDS < deadline)); do
    if docker logs --since "${cursor}" "${DAEMON_CONTAINER}" 2>&1 |
      grep -E "${message}" | grep -F "${container}" >/dev/null; then
      pass "${step}: the daemon logged ${message} for ${container}"
      return
    fi
    sleep 2
  done
  fail "${step}: no ${message} for ${container} within ${SETTLE_SECONDS}s"
}

wait_for() {
  local what=$1 deadline=$((SECONDS + SETTLE_SECONDS))
  shift
  until "$@"; do
    ((SECONDS < deadline)) || die "timed out waiting for ${what}"
    sleep 2
  done
}

daemon_running() { [[ -n $(docker ps -q --filter "name=^${DAEMON_CONTAINER}$") ]]; }
daemon_gone() { [[ -z $(docker ps -aq --filter "name=^${DAEMON_CONTAINER}$") ]]; }

# The daemon container's resident set, current and peak, in kB.
record_rss() {
  local pid
  pid=$(docker inspect --format '{{.State.Pid}}' "${DAEMON_CONTAINER}")
  log "RSS $1: $(awk '/^VmRSS|^VmHWM/ {printf "%s %s kB  ", $1, $2}' "/proc/${pid}/status")"
}

daemon_logs() { docker logs "${DAEMON_CONTAINER}" 2>&1; }

# Every service call is detached: the CLI's wait for a global service to converge can hang for
# good (a constraint that leaves this node out never converges in its progress monitor), so each
# step waits for the state it needs instead.
create_service() {
  local name=$1
  shift
  docker service create --quiet --detach --name "${name}" \
    --constraint "node.id==${NODE_ID}" "$@" >/dev/null
  created_services+=("${name}")
}

remove_service() {
  docker service rm "$1" >/dev/null
  local kept=() service
  for service in "${created_services[@]}"; do
    [[ ${service} == "$1" ]] || kept+=("${service}")
  done
  created_services=("${kept[@]}")
}

task_running() { [[ -n $(task_container "$1") ]]; }

start_task() {
  create_service "$1" --mount "type=bind,source=${MOUNT},target=${MOUNT}" \
    --container-label swarm-device-access.enable=true "${TEST_IMAGE}" sleep 86400
  wait_for "the $1 task" task_running "$1"
}

# Starts a daemon service from an image with the deployed service's mounts and launch arguments.
start_daemon() {
  local name=$1 image=$2
  shift 2
  wait_for "the previous daemon to be removed" daemon_gone
  create_service "${name}" --mode global --no-resolve-image "${MOUNT_ARGS[@]}" "${image}" "$@"
  wait_for "the ${name} daemon" daemon_running
  sleep 5
}

# Keeps each daemon's log beside the results, for the record and for a failed check.
saved_logs=0
stop_daemon() {
  saved_logs=$((saved_logs + 1))
  daemon_logs >"${LOG_DIR}/${saved_logs}-$1.log" || true
  remove_service "$1"
  wait_for "the ${1} daemon to stop" daemon_gone
}

cleanup() {
  local service
  for service in "${created_services[@]}"; do
    docker service rm "${service}" >/dev/null 2>&1 || log "could not remove ${service}"
  done
  if [[ -n ${constraint} ]]; then
    docker service update --quiet --detach --constraint-rm "${constraint}" "${GO_SERVICE}" \
      >/dev/null || log "could not lift ${constraint} from ${GO_SERVICE}: lift it by hand"
  fi
}

main() {
  require
  DEVICE=$(pick_device)
  NODE_ID=$(docker info --format '{{.Swarm.NodeID}}')
  local spec go_image
  spec=$(docker service inspect "${GO_SERVICE}")
  go_image=$(jq -r '.[0].Spec.TaskTemplate.ContainerSpec.Image' <<<"${spec}")
  mapfile -t GO_ARGS < <(jq -r '.[0].Spec.TaskTemplate.ContainerSpec.Args // [] | .[]' <<<"${spec}")
  mapfile -t MOUNT_ARGS < <(jq -r '.[0].Spec.TaskTemplate.ContainerSpec.Mounts // [] | .[] |
    "--mount", "type=\(.Type),source=\(.Source),target=\(.Target),readonly=\(.ReadOnly // false)"' \
    <<<"${spec}")
  # Rust spells every long flag with two dashes (README, Migrating from 1.x). Its daemons under test
  # log at debug, appended last so it overrides the deployed level as the last flag does: the
  # revocation after die and destroy is a debug line.
  local rust_args=() arg
  for arg in "${GO_ARGS[@]}"; do
    if [[ ${arg} =~ ^-[a-z] ]]; then rust_args+=("-${arg}"); else rust_args+=("${arg}"); fi
  done
  [[ " ${GO_ARGS[*]} " == *" -- "* ]] || {
    GO_ARGS+=(--)
    rust_args+=(--)
  }
  rust_args+=(--log-level=debug)
  log "node ${NODE_ID}, kernel $(uname -r), device ${DEVICE} under ${MOUNT}"
  log "Go service ${GO_SERVICE}: ${go_image} ${GO_ARGS[*]}"
  log "Rust image ${SDA_FIELD_RUST_IMAGE}: ${rust_args[*]}"
  trap cleanup EXIT
  trap 'exit 130' INT TERM

  # 1. The deployed Go daemon grants a task.
  daemon_running || die "the Go daemon is not running on this node"
  record_rss "Go daemon (deployed)"
  start_task "${PREFIX}-task-go"
  expect_access "${PREFIX}-task-go" allowed "Go grants after start"

  # 2. Go stops on this node; Rust takes over with the same policy and keeps the grant.
  constraint="node.id!=${NODE_ID}"
  docker service update --quiet --detach --constraint-add "${constraint}" "${GO_SERVICE}" >/dev/null
  wait_for "the Go daemon to stop" daemon_gone
  start_daemon "${RUST_DAEMON_SERVICE}" "${SDA_FIELD_RUST_IMAGE}" "${rust_args[@]}"
  record_rss "Rust daemon (startup)"
  expect_access "${PREFIX}-task-go" allowed "Go-granted task under Rust"

  # 3. A new task is granted after start, after unpause and after a systemd daemon-reload.
  start_task "${PREFIX}-task-rust"
  expect_access "${PREFIX}-task-rust" allowed "Rust grants after start"
  local container cursor
  container=$(docker inspect --format '{{.Id}}' "$(task_container "${PREFIX}-task-rust")")
  cursor=$(log_cursor)
  docker pause "${container}" >/dev/null
  docker unpause "${container}" >/dev/null
  expect_logged "${cursor}" "container processed" "${container}" "Rust reconciles on unpause"
  expect_access "${PREFIX}-task-rust" allowed "Rust grants after unpause"
  cursor=$(log_cursor)
  systemctl daemon-reload
  # The watcher's own line proves the pass came from the reload (it needs --dbus in the launch
  # arguments), not from an event still being handled; it names no container.
  expect_logged "${cursor}" "systemd reload completed; re-applying device rules" "" \
    "Rust's systemd reload watcher sees the reload"
  expect_logged "${cursor}" "container processed" "${container}" \
    "Rust reconciles after systemctl daemon-reload"
  expect_access "${PREFIX}-task-rust" allowed "Rust grants after systemctl daemon-reload"

  # 4. Die and destroy release the task's rules.
  cursor=$(log_cursor)
  remove_service "${PREFIX}-task-rust"
  # The grants are revoked while the task's cgroup still opens, and the record is released once it
  # is gone, which a removed service's task usually is by the time its die and destroy arrive.
  expect_logged "${cursor}" "device grants revoked|container cgroup gone; released" "${container}" \
    "Rust revokes or releases on die and destroy"
  if docker logs --since "${cursor}" "${DAEMON_CONTAINER}" 2>&1 |
    grep -F "could not clean up after container" | grep -F "${container}" >/dev/null; then
    fail "die and destroy of ${container} logged a cleanup failure"
  fi
  record_rss "Rust daemon (after the tasks)"

  # 5. Rust under a narrower policy revokes the Go-granted task, then grants it again.
  stop_daemon "${RUST_DAEMON_SERVICE}"
  start_daemon "${RUST_DAEMON_SERVICE}" "${SDA_FIELD_RUST_IMAGE}" "${rust_args[@]}" \
    "--device-deny=${DEVICE}"
  expect_access "${PREFIX}-task-go" denied "Rust revokes the Go grant under --device-deny"
  stop_daemon "${RUST_DAEMON_SERVICE}"
  start_daemon "${RUST_DAEMON_SERVICE}" "${SDA_FIELD_RUST_IMAGE}" "${rust_args[@]}"
  expect_access "${PREFIX}-task-go" allowed "Rust grants again with the original policy"
  record_rss "Rust daemon (end)"
  stop_daemon "${RUST_DAEMON_SERVICE}"

  # 6. The restored Go daemon keeps the Rust grant with the same policy, revokes it under a
  # narrower one, and the deployed Go service, back on the node, grants it again.
  start_daemon "${GO_DAEMON_SERVICE}" "${go_image}" "${GO_ARGS[@]}"
  expect_access "${PREFIX}-task-go" allowed "Rust-granted task under the restored Go daemon"
  stop_daemon "${GO_DAEMON_SERVICE}"
  start_daemon "${GO_DAEMON_SERVICE}" "${go_image}" "${GO_ARGS[@]}" "-device-deny=${DEVICE}"
  expect_access "${PREFIX}-task-go" denied "restored Go revokes the Rust grant under -device-deny"
  stop_daemon "${GO_DAEMON_SERVICE}"
  docker service update --quiet --detach --constraint-rm "${constraint}" "${GO_SERVICE}" >/dev/null
  constraint=""
  wait_for "the deployed Go daemon to return" daemon_running
  expect_access "${PREFIX}-task-go" allowed "deployed Go grants again"
  record_rss "Go daemon (restored)"

  if ((failures > 0)); then
    log "${failures} check(s) failed"
    exit 1
  fi
  log "all checks passed"
}

main "$@"
