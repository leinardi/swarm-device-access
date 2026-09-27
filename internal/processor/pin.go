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

package processor

import (
	"errors"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"strconv"

	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
)

var (
	// errPinnedProcessGone reports that the process pinned for a container
	// exited before the mutation.
	errPinnedProcessGone = errors.New("pinned container process exited")
	// errLifecycleChanged reports that a second inspect no longer shows the
	// container running with the pinned pid and start time.
	errLifecycleChanged = errors.New("container changed between inspect and mutation")
	// errNotInCgroup reports that the pinned pid is not in the cgroup the
	// processor resolved for it.
	errNotInCgroup = errors.New("container process is not in the resolved cgroup")
)

// pinnedProcess is a process held by a pidfd: while it is alive its pid
// cannot be reused, so a pid read once stays meaningful.
type pinnedProcess interface {
	// alive returns nil when the pinned process has not exited.
	alive() error
	Close() error
}

// processPinner opens pidfds. It is a seam: tests cannot pin arbitrary pids.
type processPinner interface {
	pin(pid int) (pinnedProcess, error)
}

// pidfdPinner is the production processPinner (Linux 5.3+).
type pidfdPinner struct{}

type pidfd int

//nolint:ireturn // pinnedProcess hides the pidfd so tests can fake process lifetimes
func (pidfdPinner) pin(pid int) (pinnedProcess, error) {
	fd, err := unix.PidfdOpen(pid, 0)
	if err != nil {
		return nil, fmt.Errorf("pidfd_open(%d): %w", pid, err)
	}

	return pidfd(fd), nil
}

func (fd pidfd) Close() error {
	err := unix.Close(int(fd))
	if err != nil {
		return fmt.Errorf("close pidfd: %w", err)
	}

	return nil
}

func (fd pidfd) alive() error {
	err := unix.PidfdSendSignal(int(fd), 0, nil, 0)
	if err != nil {
		return fmt.Errorf("%w: %w", errPinnedProcessGone, err)
	}

	return nil
}

// readProcCgroup reads cgroup and mountinfo of pid through one descriptor on
// /proc/<pid>. The directory descriptor refers to the process it was opened
// for, so a later pid reuse cannot redirect the second read.
func readProcCgroup(procRoot string, pid int) (cgroup.ProcCgroup, error) {
	dirPath := filepath.Join(procRoot, "proc", strconv.Itoa(pid))

	dirFD, err := unix.Open(dirPath, unix.O_DIRECTORY|unix.O_RDONLY|unix.O_CLOEXEC, 0)
	if err != nil {
		return cgroup.ProcCgroup{}, fmt.Errorf("open %q: %w", dirPath, err)
	}
	defer unix.Close(dirFD)

	cgroupData, err := readAt(dirFD, "cgroup")
	if err != nil {
		return cgroup.ProcCgroup{}, err
	}

	mountinfoData, err := readAt(dirFD, "mountinfo")
	if err != nil {
		return cgroup.ProcCgroup{}, err
	}

	resolved, err := cgroup.ParseProcCgroup(cgroupData, mountinfoData)
	if err != nil {
		return cgroup.ProcCgroup{}, fmt.Errorf("resolve cgroup of pid %d: %w", pid, err)
	}

	return resolved, nil
}

func readAt(dirFD int, name string) ([]byte, error) {
	fd, err := unix.Openat(dirFD, name, unix.O_RDONLY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", name, err)
	}

	file := os.NewFile(uintptr(fd), name)
	defer file.Close()

	data, err := io.ReadAll(file)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", name, err)
	}

	return data, nil
}
