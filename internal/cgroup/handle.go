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

package cgroup

import (
	"fmt"

	"golang.org/x/sys/unix"
)

// Identity names one cgroup directory: its path and the inode of the
// directory. A cgroup removed and recreated at the same path gets a new
// inode, so state keyed by Identity never carries over to the new cgroup.
type Identity struct {
	Path  string
	Inode uint64
}

// CgroupHandle is an open cgroup directory. The directory is resolved by
// path exactly once, in OpenCgroup; every later read and write goes through
// the descriptor, so a container that exits and a cgroup recreated at the
// same path between resolution and mutation cannot receive the old
// container's rules (the descriptor still refers to the removed directory).
type CgroupHandle struct {
	fd       int
	identity Identity
}

// OpenCgroup opens the cgroup directory at path and records its identity
// from Fstat on the same descriptor. The caller must Close the handle.
func OpenCgroup(path string) (*CgroupHandle, error) {
	fd, err := unix.Open(path, unix.O_DIRECTORY|unix.O_RDONLY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, fmt.Errorf("open cgroup %q: %w", path, err)
	}

	var stat unix.Stat_t

	err = unix.Fstat(fd, &stat)
	if err != nil {
		unix.Close(fd)

		return nil, fmt.Errorf("stat cgroup %q: %w", path, err)
	}

	return &CgroupHandle{fd: fd, identity: Identity{Path: path, Inode: stat.Ino}}, nil
}

// Identity returns the path and inode the handle was opened with.
func (h *CgroupHandle) Identity() Identity {
	return h.identity
}

// Close releases the descriptor.
func (h *CgroupHandle) Close() error {
	err := unix.Close(h.fd)
	if err != nil {
		return fmt.Errorf("close cgroup %q: %w", h.identity.Path, err)
	}

	return nil
}
