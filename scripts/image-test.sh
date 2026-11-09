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
# Adapted from swarm-scheduler-exporter/scripts/image-test.sh at 88394aad4cf9, originally
# MIT-licensed, Copyright (c) 2026 Roberto Leinardi.
#

# image-test.sh — tests a built swarm-device-access image of one platform, in three tiers, each
# including the one before it:
#
#   smoke    the release's short test: the image is the platform's, its binary is byte for byte
#            the given release binary (sha256), and the daemon, started in dry-run with no Docker
#            socket, logs the expected stamps in its startup line and then exits non-zero because
#            it cannot reach Docker. The binary has no --version, and nothing is written to a cgroup.
#   reduced  CI's arm64 test, under QEMU: smoke, plus the binary's ELF machine is the platform's
#            and the binary is static (no interpreter, no shared library), the binary's mode and
#            owner in the image are 0755 root:root, and the image config (root, the entrypoint, no
#            exposed port, no health check, the labels) is what the Dockerfile writes.
#   full     CI's amd64 test: reduced, plus the daemon, in dry-run with the host's Docker socket and
#            the metrics listener on, reports itself live on /healthz and ready on /readyz once it
#            follows the Docker events, serves its sda_* series on /metrics, stops cleanly on
#            SIGTERM (exit code 0), and an invalid flag value fails the start with a non-zero exit
#            code. Dry-run: the daemon reads the host's containers and writes no cgroup.
#
# Usage:
#   scripts/image-test.sh --tier smoke|reduced|full --image IMAGE --platform os/arch \
#     --binary FILE [--expect-version "VERSION COMMIT DATE"] [--docker-socket PATH]
#
# IMAGE must already be in the local Docker image store, for PLATFORM. Every container it starts
# is removed on exit. --docker-socket (full tier only) is the host's Docker socket, by default
# /var/run/docker.sock. Needs docker, curl, sha256sum, tar and (reduced, full) readelf.

set -euo pipefail
# readelf and tar print in the C locale, the one the checks below read.
export LC_ALL=C

BINARY_PATH="usr/local/bin/swarm-device-access"
METRICS_PORT=9090

usage() {
	sed -n '/^# Usage:/,/^# IMAGE/p' "${BASH_SOURCE[0]}" | sed 's/^# \{0,1\}//' >&2
	exit 2
}

die() {
	echo "image-test: FAIL: $*" >&2
	exit 1
}

pass() {
	echo "image-test: ok: $*"
}

tier="" image="" platform="" binary="" expect_version="" docker_socket="/var/run/docker.sock"
while [[ $# -gt 0 ]]; do
	case "$1" in
	--tier) tier="${2:-}" ;;
	--image) image="${2:-}" ;;
	--platform) platform="${2:-}" ;;
	--binary) binary="${2:-}" ;;
	--expect-version) expect_version="${2:-}" ;;
	--docker-socket) docker_socket="${2:-}" ;;
	*) usage ;;
	esac
	shift 2 || usage
done
case "${tier}" in
smoke) level=1 ;;
reduced) level=2 ;;
full) level=3 ;;
*) usage ;;
esac
[[ -n ${image} && -n ${platform} && -n ${binary} ]] || usage
[[ -f ${binary} ]] || die "the release binary ${binary} does not exist"
want_version="" want_commit="" want_date=""
if [[ -n ${expect_version} ]]; then
	read -r want_version want_commit want_date <<<"${expect_version}"
	[[ -n ${want_date} ]] || die "--expect-version takes \"VERSION COMMIT DATE\", got '${expect_version}'"
fi
if ((level >= 3)); then
	[[ -S ${docker_socket} ]] || die "the full tier needs the host's Docker socket; ${docker_socket} is not one"
fi

work="$(mktemp -d)"
containers=()
cleanup() {
	if [[ ${#containers[@]} -gt 0 ]]; then
		docker rm -f "${containers[@]}" >/dev/null 2>&1 || true
	fi
	rm -rf "${work}"
}
trap cleanup EXIT

# Sets NAME to a unique container name, which the exit trap removes. Not called through $( ): a
# subshell would add the name to its own copy of the list.
new_container() {
	NAME="sda-image-test-$1-$$-${RANDOM}"
	containers+=("${NAME}")
}

# Checks that the startup line $1 carries the expected stamps, when they were given.
check_stamps() {
	local line="$1" pair key want
	[[ -n ${expect_version} ]] || return 0
	for pair in "version=${want_version}" "commit=${want_commit}" "date=${want_date}"; do
		key="${pair%%=*}" want="${pair#*=}"
		[[ ${line} == *"${key}=\"${want}\""* ]] ||
			die "the startup line does not report ${key} ${want}: ${line}"
	done
}

# --- The image is the platform's -------------------------------------------------------------

got_platform="$(docker image inspect -f '{{.Os}}/{{.Architecture}}' "${image}")"
[[ ${got_platform} == "${platform}" ]] || die "${image} is ${got_platform}, want ${platform}"
pass "image platform ${got_platform}"

# --- The binary in the image is the release binary --------------------------------------------

# The image's filesystem, exported from a container that never starts, so the binary's bytes,
# mode and owner are read without running anything.
new_container export
export_name="${NAME}"
docker create --platform "${platform}" --name "${export_name}" "${image}" >/dev/null
docker export -o "${work}/rootfs.tar" "${export_name}"
mkdir "${work}/rootfs"
tar -xf "${work}/rootfs.tar" -C "${work}/rootfs" "${BINARY_PATH}"
extracted="${work}/rootfs/${BINARY_PATH}"

want_sum="$(sha256sum "${binary}")"
want_sum="${want_sum%% *}"
got_sum="$(sha256sum "${extracted}")"
got_sum="${got_sum%% *}"
[[ ${got_sum} == "${want_sum}" ]] ||
	die "/${BINARY_PATH} in the image is sha256 ${got_sum}, the release binary ${binary} is ${want_sum}"
pass "/${BINARY_PATH} is the release binary (sha256 ${got_sum})"

# --- The daemon starts and reports its stamps -------------------------------------------------

# In dry-run and with no Docker socket in the container: the daemon logs its startup line, fails
# to reach Docker and exits on its own, having written nothing.
new_container smoke
smoke="${NAME}"
docker run -d --platform "${platform}" --name "${smoke}" "${image}" --dry-run=true >/dev/null
# Up to 60 s: QEMU is slow.
exited=false
for _ in $(seq 1 60); do
	if [[ "$(docker inspect -f '{{.State.Running}}' "${smoke}")" != true ]]; then
		exited=true
		break
	fi
	sleep 1
done
logs="$(docker logs "${smoke}" 2>&1)"
[[ ${exited} == true ]] || {
	printf '%s\n' "${logs}" >&2
	die "the daemon with no Docker socket was still running after 60 s"
}
line="$(grep -m1 -F 'swarm-device-access starting' <<<"${logs}")" || {
	printf '%s\n' "${logs}" >&2
	die "the daemon exited without logging its startup line"
}
check_stamps "${line}"
[[ ${line} == *"dry_run=true"* ]] || die "the startup line does not report dry_run=true: ${line}"
pass "startup line: ${line}"
grep -qF 'docker client init failed' <<<"${logs}" || {
	printf '%s\n' "${logs}" >&2
	die "the daemon did not report that it cannot reach Docker"
}
exit_code="$(docker inspect -f '{{.State.ExitCode}}' "${smoke}")"
[[ ${exit_code} != 0 ]] || die "the daemon with no Docker socket exited 0"
pass "with no Docker socket the daemon exits ${exit_code}"

if ((level >= 2)); then
	# --- The binary is the platform's, and static ------------------------------------------

	case "${platform}" in
	linux/amd64) want_machine="Advanced Micro Devices X86-64" ;;
	linux/arm64) want_machine="AArch64" ;;
	*) die "no ELF machine known for ${platform}" ;;
	esac
	header="$(readelf -h "${extracted}")"
	[[ ${header} == *"Machine:"*"${want_machine}"* ]] ||
		die "/${BINARY_PATH} is not a ${want_machine} binary"
	program_headers="$(readelf -lW "${extracted}")"
	[[ ${program_headers} != *INTERP* ]] || die "/${BINARY_PATH} has a program interpreter: it is not static"
	dynamic="$(readelf -dW "${extracted}")"
	[[ ${dynamic} != *"(NEEDED)"* ]] || die "/${BINARY_PATH} needs a shared library: it is not static"
	pass "/${BINARY_PATH} is a static ${want_machine} binary"

	# --- The binary's mode and owner ---------------------------------------------------------

	listing="$(tar --numeric-owner -tvf "${work}/rootfs.tar" "${BINARY_PATH}")"
	read -r mode owner _ <<<"${listing}"
	[[ ${mode} == -rwxr-xr-x && ${owner} == 0/0 ]] ||
		die "/${BINARY_PATH} is ${mode} ${owner}, want -rwxr-xr-x 0/0"
	pass "/${BINARY_PATH} is ${mode} ${owner}"

	# --- The image config ----------------------------------------------------------------------

	# Root: bpf(2) and the cgroup v2 writes need it, and the base defaults to a non-root user.
	user="$(docker image inspect -f '{{.Config.User}}' "${image}")"
	[[ ${user} == 0 ]] || die "the image runs as '${user}', want 0 (root)"
	entrypoint="$(docker image inspect -f '{{json .Config.Entrypoint}}' "${image}")"
	[[ ${entrypoint} == '["/usr/local/bin/swarm-device-access"]' ]] ||
		die "the entrypoint is ${entrypoint}, want [\"/usr/local/bin/swarm-device-access\"]"
	ports="$(docker image inspect -f '{{json .Config.ExposedPorts}}' "${image}")"
	[[ ${ports} == null || ${ports} == '{}' ]] || die "the image exposes ${ports}, want no port"
	healthcheck="$(docker image inspect -f '{{json .Config.Healthcheck}}' "${image}")"
	[[ ${healthcheck} == null ]] || die "the image has the health check ${healthcheck}, want none"
	if [[ -n ${expect_version} ]]; then
		for pair in "version=${want_version}" "revision=${want_commit}" "created=${want_date}"; do
			key="${pair%%=*}" want="${pair#*=}"
			got="$(docker image inspect -f "{{ index .Config.Labels \"org.opencontainers.image.${key}\" }}" "${image}")"
			[[ ${got} == "${want}" ]] || die "the label org.opencontainers.image.${key} is '${got}', want '${want}'"
		done
	fi
	pass "image config: user ${user}, entrypoint ${entrypoint}, no port, no health check, labels"
fi

if ((level >= 3)); then
	# --- Live and ready with the host's Docker ----------------------------------------------------

	# Dry-run: the daemon follows the host's Docker events and reads its containers, and writes
	# no cgroup.
	new_container daemon
	daemon="${NAME}"
	docker run -d --platform "${platform}" --name "${daemon}" -p "127.0.0.1::${METRICS_PORT}" \
		-v "${docker_socket}:/var/run/docker.sock" "${image}" \
		--dry-run=true --metrics-addr ":${METRICS_PORT}" >/dev/null
	mapping="$(docker port "${daemon}" "${METRICS_PORT}/tcp")"
	mapping="${mapping%%$'\n'*}"
	port="${mapping##*:}"

	ready=""
	for _ in $(seq 1 60); do
		ready="$(curl -sS --max-time 5 -o /dev/null -w '%{http_code}' "http://127.0.0.1:${port}/readyz" 2>/dev/null || true)"
		[[ ${ready} != 200 ]] || break
		if [[ "$(docker inspect -f '{{.State.Running}}' "${daemon}")" != true ]]; then
			docker logs "${daemon}" >&2 || true
			die "the daemon with the host's Docker socket exited"
		fi
		sleep 1
	done
	[[ ${ready} == 200 ]] || {
		docker logs "${daemon}" >&2 || true
		die "/readyz did not answer HTTP 200 within 60 s (last: ${ready:-none})"
	}
	line="$(docker logs "${daemon}" 2>&1 | grep -m1 -F 'swarm-device-access starting')" ||
		die "the daemon logged no startup line"
	check_stamps "${line}"
	pass "/readyz: HTTP 200"

	healthz="$(curl -fsS --max-time 5 "http://127.0.0.1:${port}/healthz")" ||
		die "/healthz did not answer"
	[[ ${healthz} == ok ]] || die "/healthz answered '${healthz}', want ok"
	pass "/healthz: ok"

	curl -fsS --max-time 5 -o "${work}/metrics" "http://127.0.0.1:${port}/metrics" ||
		die "/metrics did not answer"
	sample="$(grep -m1 '^sda_' "${work}/metrics")" || die "/metrics has no sda_* sample"
	pass "/metrics: ${sample}"

	# --- A stop is clean -------------------------------------------------------------------------

	docker stop -t 30 "${daemon}" >/dev/null
	exit_code="$(docker inspect -f '{{.State.ExitCode}}' "${daemon}")"
	[[ ${exit_code} == 0 ]] || die "docker stop: exit code ${exit_code}, want 0"
	pass "docker stop: exit code 0"

	# --- An invalid flag fails the start -------------------------------------------------------

	new_container invalid
	invalid="${NAME}"
	docker run --platform "${platform}" --name "${invalid}" "${image}" --log-level bogus >/dev/null 2>&1 || true
	exit_code="$(docker inspect -f '{{.State.ExitCode}}' "${invalid}")"
	[[ ${exit_code} != 0 ]] || die "an invalid --log-level exited 0"
	pass "an invalid --log-level fails the start (exit code ${exit_code})"
fi

echo "image-test: ${tier}: every check passed for ${image} (${platform})"
