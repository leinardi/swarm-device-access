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
	"bufio"
	"errors"
	"fmt"
	"io"
	"os"
	"regexp"
	"slices"
	"strings"
)

const selfMountinfo = "/proc/self/mountinfo"

var (
	errSelfNotFound = errors.New(
		"no Docker container ID in the mount table; is the launcher running in a container?",
	)
	errSelfAmbiguous = errors.New("more than one Docker container ID in the mount table")

	// containerFileRoot matches the source of the per-container files Docker
	// bind-mounts into every container, whatever its data-root
	// (/var/lib/docker, /mnt/.ix-apps/docker, ...) or hostname.
	containerFileRoot = regexp.MustCompile(
		`/containers/([0-9a-f]{64})/(?:hostname|hosts|resolv\.conf)$`,
	)

	// containerFileTargets are where those files are mounted.
	containerFileTargets = []string{
		"/etc/hostname",
		"/etc/hosts",
		"/etc/resolv.conf",
	}
)

// mountinfoFields are the fields of a /proc/<pid>/mountinfo line up to the
// mount point: mount ID, parent ID, major:minor, root, mount point.
const mountinfoFields = 5

// selfIDFromMountinfo returns the ID of the container whose mount table
// this is.
func selfIDFromMountinfo() (string, error) {
	file, err := os.Open(selfMountinfo)
	if err != nil {
		return "", fmt.Errorf("open %s: %w", selfMountinfo, err)
	}
	defer file.Close()

	return parseSelfID(file)
}

// parseSelfID finds the container ID in the root of the mounts at /etc/hostname,
// /etc/hosts and /etc/resolv.conf. No match, or two different IDs, is an
// error: the launcher must never guess which image it runs.
func parseSelfID(reader io.Reader) (string, error) {
	var found string

	scanner := bufio.NewScanner(reader)
	for scanner.Scan() {
		fields := strings.Fields(scanner.Text())
		if len(fields) < mountinfoFields || !slices.Contains(containerFileTargets, fields[4]) {
			continue
		}

		match := containerFileRoot.FindStringSubmatch(fields[3])
		if match == nil {
			continue
		}

		if found != "" && found != match[1] {
			return "", errSelfAmbiguous
		}

		found = match[1]
	}

	err := scanner.Err()
	if err != nil {
		return "", fmt.Errorf("read mount table: %w", err)
	}

	if found == "" {
		return "", errSelfNotFound
	}

	return found, nil
}
