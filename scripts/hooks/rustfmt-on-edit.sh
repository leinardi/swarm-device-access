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
# Adapted from swarm-scheduler-exporter/scripts/hooks/rustfmt-on-edit.sh at d57dcc220d29,
# originally MIT-licensed, Copyright (c) 2026 Roberto Leinardi.
#

# rustfmt-on-edit.sh -- Claude Code PostToolUse hook (configured in .claude/settings.json), the
# Rust counterpart of gofmt-on-edit.sh, which runs beside it.
# Auto-formats Rust files immediately after an Edit/Write tool call so formatting
# drift is fixed at edit time instead of failing later at pre-commit.
#
# Input: hook payload JSON on stdin; the edited path is at .tool_input.file_path.
# rustfmt finds the repository's rustfmt.toml (edition, max_width) from the file's directory.
# Always exits 0 (a formatting fix must never block the agent).

set -euo pipefail

file="$(python3 -c 'import json,sys; print(json.load(sys.stdin).get("tool_input",{}).get("file_path",""))' 2>/dev/null || true)"

case "$file" in
  *.rs)
    if [[ -f "$file" ]] && command -v rustfmt >/dev/null 2>&1; then
      rustfmt --quiet "$file" || true
    fi
    ;;
esac

exit 0
