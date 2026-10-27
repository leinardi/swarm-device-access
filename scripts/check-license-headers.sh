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
# License header checker.
#
# swarm-device-access is Apache-2.0, and every source file says so in its own header. This fails on
# any tracked source file whose first lines do not carry the Apache License 2.0 notice with Roberto
# Leinardi's copyright, so a header copied from another project is caught as surely as a missing
# one. The files preserved from NVIDIA's code keep NVIDIA's own Apache-2.0 header (AGENTS.md: never
# relicense or reformat that block); they are named in NVIDIA_FILES, and only they may carry it.
#
# Governed: Go, Rust and shell sources, Dockerfiles, the Makefile and the repo-local .mk fragments.
# Rust means every .rs file of every Cargo package: src, tests, build scripts, the meta and testenv
# members. Exempt: the make-common files the bootstrap fetches (upstream, under make-common's MIT
# license: the snippets named in the Makefile's MK_COMMON_FILES, and the bootstrap script itself),
# and Rust files under a testdata/ directory, which are fixtures. YAML, JSON, TOML and Markdown are
# never governed.
#
# Rust files have two more rules, which leave the Go, shell and make checks exactly as they were:
#
#   - Code reused from swarm-scheduler-exporter or swarm-rootless-docker (MIT, same author) carries
#     this project's Apache-2.0 header followed by a provenance comment, "// Adapted from
#     <repo>/<path> at <commit>, originally MIT-licensed, Copyright (c) <year> Roberto Leinardi",
#     which may wrap over several // lines. The comment is accepted and never required, but one
#     that is present must be complete, so a reused file's origin cannot be recorded half-way.
#   - The MIT permission text itself must not appear in the header: the reused file is relicensed,
#     and pasting the old block alongside the new one would leave two licences in one file.
#
# The Rust detection is adapted from swarm-scheduler-exporter's scripts/check-license-headers.sh at
# d57dcc220d29, originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi.

set -euo pipefail

usage() {
	cat <<'USAGE'
Usage: scripts/check-license-headers.sh

Checks every tracked source file for the swarm-device-access Apache-2.0 header and lists the
files that lack it. Run from anywhere inside the repository.
USAGE
}

if [[ $# -gt 0 ]]; then
	usage >&2
	exit 2
fi

cd "$(git rev-parse --show-toplevel)"

# How many leading lines the header must sit in: a shebang, a build constraint or a Dockerfile
# parser directive, and a blank line may precede it.
HEADER_WINDOW=25

REQUIRED_LINES=(
	'Licensed under the Apache License, Version 2.0 (the "License");'
	"you may not use this file except in compliance with the License."
	"WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied."
)
# The year is the file's first commit; a range (2025-2026) is allowed too.
COPYRIGHT_RE='Copyright [0-9]{4}(-[0-9]{4})? Roberto Leinardi\.'
NVIDIA_COPYRIGHT_RE='Copyright \(c\) [0-9]{4}, NVIDIA CORPORATION\.'

# The files preserved from NVIDIA's code, which keep NVIDIA's copyright line: the Go originals and
# their Rust ports.
NVIDIA_FILES=(
	"internal/cgroup/api.go"
	"internal/cgroup/ebpf.go"
	"internal/cgroup/v1.go"
	"internal/cgroup/v2.go"
	"src/cgroup/api.rs"
	"src/cgroup/ebpf.rs"
	"src/cgroup/v1.rs"
	"src/cgroup/v2.rs"
)

# The provenance comment of a Rust file reused from another project, once its // lines are joined.
PROVENANCE_RE='^Adapted from [A-Za-z0-9._-]+/[^ ]+ at [0-9a-f]{7,40}, originally MIT-licensed, Copyright \(c\) [0-9]{4}(-[0-9]{4})? Roberto Leinardi$'
# Text only the MIT licence carries.
MIT_MARKER='Permission is hereby granted, free of charge'

# Exact paths that are not this repository's own source.
EXEMPT_PATHS=(
	"scripts/bootstrap-mk-common.sh"
)

# The shared make fragments, read from the Makefile so the list cannot drift from what the
# bootstrap actually fetches.
mk_common_files() {
	sed -n 's/^MK_COMMON_FILES[[:space:]]*:=[[:space:]]*//p' Makefile
}

is_governed() {
	local path="$1" base
	base="${path##*/}"

	case "${path}" in
	testdata/*.rs | */testdata/*.rs)
		return 1
		;;
	*.go | *.rs | *.sh | .mk/*.mk)
		return 0
		;;
	esac

	case "${base}" in
	Makefile | Dockerfile | *.Dockerfile)
		return 0
		;;
	esac

	return 1
}

is_exempt() {
	local path="$1" entry

	for entry in "${EXEMPT_PATHS[@]}"; do
		if [[ ${path} == "${entry}" ]]; then
			return 0
		fi
	done

	for entry in ${COMMON_FILES}; do
		if [[ ${path} == ".mk/${entry}" ]]; then
			return 0
		fi
	done

	return 1
}

is_nvidia() {
	local path="$1" entry

	for entry in "${NVIDIA_FILES[@]}"; do
		if [[ ${path} == "${entry}" ]]; then
			return 0
		fi
	done

	return 1
}

has_header() {
	local path="$1" head line copyright_re
	head="$(head -n "${HEADER_WINDOW}" -- "${path}")"

	for line in "${REQUIRED_LINES[@]}"; do
		if [[ ${head} != *"${line}"* ]]; then
			return 1
		fi
	done

	copyright_re="${COPYRIGHT_RE}"
	if is_nvidia "${path}"; then
		copyright_re="${NVIDIA_COPYRIGHT_RE}"
	fi

	grep -Eq -- "${copyright_re}" <<<"${head}" || return 1

	if [[ ${path} == *.rs ]]; then
		rust_header_ok "${head}" || return 1
	fi
}

# rust_header_ok applies the two Rust-only rules to a file's header window.
rust_header_ok() {
	local head="$1" provenance

	if [[ ${head} == *"${MIT_MARKER}"* ]]; then
		return 1
	fi

	# The comment runs from its "// Adapted from" line to the line that ends in the holder's name,
	# or to the first line that is not a plain // comment.
	provenance="$(awk '
		/^\/\/ Adapted from / { open = 1 }
		open {
			if ($0 !~ /^\/\/( |$)/) { exit }
			text = $0
			sub(/^\/\/ ?/, "", text)
			joined = (joined == "" ? text : joined " " text)
			if (text ~ /Roberto Leinardi$/) { exit }
		}
		END { print joined }
	' <<<"${head}")"

	if [[ -z ${provenance} ]]; then
		return 0
	fi

	grep -Eq -- "${PROVENANCE_RE}" <<<"${provenance}"
}

COMMON_FILES="$(mk_common_files)"
if [[ -z ${COMMON_FILES} ]]; then
	echo "The Makefile names no MK_COMMON_FILES; the make-common exemption would be empty." >&2
	exit 1
fi

missing=()
checked=0

while IFS= read -r -d '' path; do
	if ! is_governed "${path}" || is_exempt "${path}"; then
		continue
	fi

	# A tracked file deleted in the working tree is not this check's concern.
	if [[ ! -f ${path} ]]; then
		continue
	fi

	checked=$((checked + 1))

	if ! has_header "${path}"; then
		missing+=("${path}")
	fi
done < <(git ls-files -z)

if [[ ${#missing[@]} -gt 0 ]]; then
	echo "These files lack the swarm-device-access Apache-2.0 header:" >&2
	printf '  %s\n' "${missing[@]}" >&2
	echo "Copy the header from any Go or shell file (IntelliJ: the Apache-2.0 copyright profile)." >&2
	echo "A reused Rust file adds a complete provenance comment below it, never the MIT block." >&2
	exit 1
fi

echo "License headers: ${checked} files checked, all carry the Apache-2.0 header."
