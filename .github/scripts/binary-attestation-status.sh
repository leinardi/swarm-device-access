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
# Adapted from swarm-scheduler-exporter/.github/scripts/binary-attestation-status.sh
# at d57dcc220d29, originally MIT-licensed, Copyright (c) 2025 Roberto Leinardi.
#

# Usage: binary-attestation-status.sh <repo> <signer-workflow> <source-ref> <source-digest> <file>...
#
# Prints, one per line, the files that still need a build-provenance attestation, and exits 0.
# A release recovery rebuilds the binaries byte-identically, so a binary an earlier run of the
# release workflow already attested keeps that attestation, and only the rest are attested again.
#
# Fail-closed and per file. For each file, the attestations of its sha256 are looked up with
# `gh api repos/<repo>/attestations/sha256:<digest>`:
#   - absent: exactly the "no attestations" answer below. The file is printed.
#   - present: a success whose body has a non-empty .attestations list. The file must then pass
#     `gh attestation verify` for <signer-workflow>, <source-ref> (the branch the release runs
#     from, e.g. refs/heads/master) and <source-digest>;
#     if it does not, it is attested by something this workflow did not produce, which needs a
#     human, and the script exits non-zero.
#   - anything else (another HTTP status such as 401, 403, 429 or 5xx, a network error, a body
#     that does not parse) exits non-zero with the captured error, naming the file.
#
# The "no attestations" answer, observed with gh 2.74.0 on 2026-09-27 for the sha256 of empty
# input (sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855) on a public
# repository:
#   exit status 1
#   stdout {"message":"Not Found","documentation_url":"https://docs.github.com/rest/repos/attestations#list-attestations","status":"404"}
#   stderr gh: Not Found (HTTP 404)
# Only that combination counts as absent, and so does a success with an empty .attestations list.

set -euo pipefail

usage() {
	echo "usage: $0 <repo> <signer-workflow> <source-ref> <source-digest> <file>..." >&2
	exit 2
}

if [ "$#" -lt 5 ]; then
	usage
fi

repo=$1
signer_workflow=$2
source_ref=$3
source_digest=$4
shift 4

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT

# fail names the file the script could not classify, prints why, and exits non-zero.
fail() {
	local file=$1
	shift
	echo "binary-attestation-status: $file: $*" >&2
	exit 1
}

# is_absent reports whether the failed lookup ($1 exit status, $2 stdout, $3 stderr) is the
# observed "no attestations" answer.
is_absent() {
	local status=$1 stdout=$2 stderr=$3
	[ "$status" -eq 1 ] &&
		grep -qxF 'gh: Not Found (HTTP 404)' "$stderr" &&
		jq -e '.status == "404" and .message == "Not Found"' "$stdout" >/dev/null 2>&1
}

for file in "$@"; do
	if [ ! -f "$file" ]; then
		fail "$file" "not a file"
	fi

	digest=$(sha256sum "$file" | cut -d' ' -f1)
	stdout="$scratch/stdout"
	stderr="$scratch/stderr"

	status=0
	gh api "repos/$repo/attestations/sha256:$digest" >"$stdout" 2>"$stderr" || status=$?

	if [ "$status" -ne 0 ]; then
		if is_absent "$status" "$stdout" "$stderr"; then
			echo "$file"
			continue
		fi

		fail "$file" "cannot tell whether sha256:$digest is attested: gh api exited $status: $(cat "$stderr") $(cat "$stdout")"
	fi

	if ! count=$(jq -e '.attestations | if type == "array" then length else error("no attestations list") end' "$stdout" 2>"$stderr"); then
		fail "$file" "cannot parse the attestations of sha256:$digest: $(cat "$stderr") $(head -c 512 "$stdout")"
	fi

	if [ "$count" -eq 0 ]; then
		echo "$file"
		continue
	fi

	if ! gh attestation verify "$file" \
		--repo "$repo" \
		--signer-workflow "$signer_workflow" \
		--source-ref "$source_ref" \
		--source-digest "$source_digest" >"$stdout" 2>"$stderr"; then
		fail "$file" "sha256:$digest is attested, but not by $signer_workflow for $source_digest on $source_ref: it needs a human - docs/release.md. $(cat "$stderr")"
	fi
done
