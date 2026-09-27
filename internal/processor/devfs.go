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
	"os"
	"path/filepath"
	"strconv"
	"strings"

	"golang.org/x/sys/unix"
)

// devRoot is where the daemon sees the host's /dev (bind-mounted in).
const devRoot = "/dev"

// errOpenat2Unsupported reports a kernel without openat2 RESOLVE_BENEATH.
var errOpenat2Unsupported = errors.New(
	"openat2 with RESOLVE_BENEATH is not supported; Linux 5.6 or newer is required",
)

// nodeKind is the file type of a resolved /dev entry.
type nodeKind int

const (
	nodeOther nodeKind = iota
	nodeChar
	nodeBlock
	nodeDir
)

// deviceID is a device number and its type ("c" or "b").
type deviceID struct {
	typ   string
	major int64
	minor int64
}

func (d deviceID) String() string {
	return fmt.Sprintf("%s %d:%d", d.typ, d.major, d.minor)
}

// sysfsClass is the /sys/dev directory for the device's type.
func (d deviceID) sysfsClass() string {
	if d.typ == "b" {
		return "block"
	}

	return "char"
}

// devNode is a /dev entry opened by descriptor: its identity comes from the
// descriptor, never from a second lookup of the path.
type devNode interface {
	// stat returns the node's type and, for a device, its number.
	stat() (nodeKind, deviceID, error)
	// resolved is the /dev path the descriptor refers to; informational
	// and for policy only.
	resolved() string
	Close() error
}

// devFS resolves names under /dev without leaving it. It is the seam that
// lets unit tests script identities: an unprivileged test cannot mknod.
type devFS interface {
	// root is the directory that stands for /dev, for enumerating names.
	root() string
	// openBeneath opens the entry at rel (relative to /dev), following
	// symlinks but refusing any step outside /dev (unix.EXDEV).
	openBeneath(rel string) (devNode, error)
	// readLink returns the target of the symlink at rel itself.
	readLink(rel string) (string, error)
	// readUevent returns the sysfs uevent of a device.
	readUevent(id deviceID) ([]byte, error)
	Close() error
}

// realDevFS is the production devFS: openat2 beneath a /dev descriptor
// opened once per pass, and sysfs under sysfsRoot.
type realDevFS struct {
	dir       string
	fd        int
	sysfsRoot string
}

// openDevFS opens dir (the /dev directory) for one pass.
func openDevFS(dir, sysfsRoot string) (*realDevFS, error) {
	fd, err := unix.Open(dir, unix.O_PATH|unix.O_DIRECTORY|unix.O_CLOEXEC, 0)
	if err != nil {
		return nil, fmt.Errorf("open %s: %w", dir, err)
	}

	return &realDevFS{dir: dir, fd: fd, sysfsRoot: sysfsRoot}, nil
}

// sysfsRootFor returns where sysfs is read from: <hostRoot>/sys, the
// documented mount, when it has device entries, otherwise /sys (a daemon run
// straight on the host). /sys/dev lists the kernel's devices whatever the
// mount namespace, so both give the same answer.
func sysfsRootFor(hostRoot string) string {
	mounted := filepath.Join(hostRoot, "sys")

	_, err := os.Stat(filepath.Join(mounted, "dev"))
	if err == nil {
		return mounted
	}

	return "/sys"
}

// ProbeOpenat2 fails when the kernel cannot resolve paths beneath a
// directory (openat2 RESOLVE_BENEATH, Linux 5.6). Without it no device path
// can be contained to /dev, so the daemon refuses to start.
func ProbeOpenat2() error {
	probeFd, err := unix.Openat2(unix.AT_FDCWD, ".", &unix.OpenHow{
		Flags:   unix.O_PATH | unix.O_CLOEXEC,
		Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS,
	})
	if errors.Is(err, unix.ENOSYS) || errors.Is(err, unix.EINVAL) {
		return fmt.Errorf("%w: %w", errOpenat2Unsupported, err)
	}

	if err != nil {
		return fmt.Errorf("probe openat2: %w", err)
	}

	unix.Close(probeFd)

	return nil
}

func (r *realDevFS) Close() error {
	err := unix.Close(r.fd)
	if err != nil {
		return fmt.Errorf("close %s: %w", r.dir, err)
	}

	return nil
}

func (r *realDevFS) root() string { return r.dir }

//nolint:ireturn // devNode hides the descriptor so tests can script identities
func (r *realDevFS) openBeneath(rel string) (devNode, error) {
	fd, err := r.openat2(rel, 0)
	if err != nil {
		return nil, err
	}

	return &realDevNode{fd: fd, dir: r.dir}, nil
}

func (r *realDevFS) readLink(rel string) (string, error) {
	linkFd, err := r.openat2(rel, unix.O_NOFOLLOW)
	if err != nil {
		return "", err
	}
	defer unix.Close(linkFd)

	buf := make([]byte, unix.PathMax)

	n, err := unix.Readlinkat(linkFd, "", buf)
	if err != nil {
		return "", fmt.Errorf("readlink %s: %w", rel, err)
	}

	return string(buf[:n]), nil
}

func (r *realDevFS) readUevent(id deviceID) ([]byte, error) {
	path := filepath.Join(r.sysfsRoot, "dev", id.sysfsClass(),
		strconv.FormatInt(id.major, 10)+":"+strconv.FormatInt(id.minor, 10), "uevent")

	data, err := os.ReadFile(path)
	if err != nil {
		return nil, fmt.Errorf("read %s: %w", path, err)
	}

	return data, nil
}

// openat2 opens rel beneath the /dev descriptor with O_PATH, which never
// opens the device driver. The kernel follows symlinks but refuses, as one
// atomic lookup, any step that leaves /dev: an absolute target, ".." past
// the root, or a magic link.
func (r *realDevFS) openat2(rel string, flags uint64) (int, error) {
	nodeFd, err := unix.Openat2(r.fd, rel, &unix.OpenHow{
		Flags:   unix.O_PATH | unix.O_CLOEXEC | flags,
		Resolve: unix.RESOLVE_BENEATH | unix.RESOLVE_NO_MAGICLINKS,
	})
	if err != nil {
		return -1, fmt.Errorf("openat2 %s: %w", rel, err)
	}

	return nodeFd, nil
}

type realDevNode struct {
	fd  int
	dir string
}

func (n *realDevNode) Close() error {
	err := unix.Close(n.fd)
	if err != nil {
		return fmt.Errorf("close device node: %w", err)
	}

	return nil
}

func (n *realDevNode) stat() (nodeKind, deviceID, error) {
	var info unix.Stat_t

	err := unix.Fstat(n.fd, &info)
	if err != nil {
		return nodeOther, deviceID{}, fmt.Errorf("fstat: %w", err)
	}

	device := deviceID{major: int64(unix.Major(info.Rdev)), minor: int64(unix.Minor(info.Rdev))}

	switch info.Mode & unix.S_IFMT {
	case unix.S_IFCHR:
		device.typ = "c"

		return nodeChar, device, nil
	case unix.S_IFBLK:
		device.typ = "b"

		return nodeBlock, device, nil
	case unix.S_IFDIR:
		return nodeDir, deviceID{}, nil
	default:
		return nodeOther, deviceID{}, nil
	}
}

func (n *realDevNode) resolved() string {
	target, err := os.Readlink("/proc/self/fd/" + strconv.Itoa(n.fd))
	if err != nil {
		return ""
	}

	rel, err := filepath.Rel(n.dir, target)
	if err != nil || rel == ".." || strings.HasPrefix(rel, "../") {
		return ""
	}

	return filepath.Join(devRoot, rel)
}
