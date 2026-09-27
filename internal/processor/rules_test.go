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
	"io/fs"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"testing"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/api/types/mount"
	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/config"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

// fakeDevFS scripts what each name under /dev resolves to, as openat2
// beneath /dev would. Every registered name also exists as a file (or a
// directory) under dir, so directory mounts can be enumerated.
type fakeDevFS struct {
	t       *testing.T
	dir     string
	nodes   map[string]fakeNode
	errs    map[string]error
	links   map[string]string
	uevents map[deviceID]string
	// walkErrs injects an error for a name (relative to dir) into the walk.
	walkErrs map[string]error
}

type fakeNode struct {
	kind nodeKind
	id   deviceID
	path string // the resolved /dev path
}

func (fakeNode) Close() error                        { return nil }
func (n fakeNode) stat() (nodeKind, deviceID, error) { return n.kind, n.id, nil }
func (n fakeNode) resolved() string                  { return n.path }

func newFakeDevFS(t *testing.T) *fakeDevFS {
	t.Helper()

	return &fakeDevFS{
		t:        t,
		dir:      t.TempDir(),
		nodes:    make(map[string]fakeNode),
		errs:     make(map[string]error),
		links:    make(map[string]string),
		uevents:  make(map[deviceID]string),
		walkErrs: make(map[string]error),
	}
}

func (*fakeDevFS) Close() error   { return nil }
func (f *fakeDevFS) root() string { return f.dir }

//nolint:ireturn // the devFS seam returns the interface
func (f *fakeDevFS) openBeneath(rel string) (devNode, error) {
	// A component that fails the lookup fails every name below it.
	parts := strings.Split(rel, "/")
	for idx := range parts {
		if err, ok := f.errs[strings.Join(parts[:idx+1], "/")]; ok {
			return nil, err
		}
	}

	node, ok := f.nodes[rel]
	if !ok {
		return nil, fmt.Errorf("openat2 %s: %w", rel, unix.ENOENT)
	}

	return node, nil
}

func (f *fakeDevFS) readLink(rel string) (string, error) {
	target, ok := f.links[rel]
	if !ok {
		return "", unix.EINVAL
	}

	return target, nil
}

func (f *fakeDevFS) readUevent(id deviceID) ([]byte, error) {
	data, ok := f.uevents[id]
	if !ok {
		return nil, fmt.Errorf("read uevent %s: %w", id, unix.ENOENT)
	}

	return []byte(data), nil
}

func (f *fakeDevFS) walk(base string, visit fs.WalkDirFunc) error {
	//nolint:wrapcheck // WalkDir only returns what visit returned
	return filepath.WalkDir(base, func(path string, entry fs.DirEntry, err error) error {
		rel, relErr := filepath.Rel(f.dir, path)
		if injected, ok := f.walkErrs[rel]; ok && relErr == nil && err == nil {
			return visit(path, entry, injected)
		}

		return visit(path, entry, err)
	})
}

// touch creates the name under dir so a walk enumerates it.
func (f *fakeDevFS) touch(rel string) {
	f.t.Helper()

	path := filepath.Join(f.dir, rel)

	err := os.MkdirAll(filepath.Dir(path), 0o755)
	if err == nil {
		err = os.WriteFile(path, nil, 0o600)
	}

	if err != nil {
		f.t.Fatal(err)
	}
}

// device registers a device node at rel with its sysfs DEVNAME.
func (f *fakeDevFS) device(rel string, id deviceID, devname string) {
	f.touch(rel)
	f.nodes[rel] = fakeNode{kind: kindOf(id), id: id, path: devRoot + "/" + rel}
	f.uevents[id] = "MAJOR=" + strconv.FormatInt(id.major, 10) + "\nDEVNAME=" + devname + "\n"
}

// alias registers a symlink at rel the kernel follows (relative, inside
// /dev) to the node registered at target.
func (f *fakeDevFS) alias(rel, target string) {
	f.touch(rel)
	f.nodes[rel] = f.nodes[target]
}

// absLink registers an absolute symlink at rel: openat2 beneath /dev
// refuses it with EXDEV, and readLink returns target.
func (f *fakeDevFS) absLink(rel, target string) {
	f.touch(rel)
	f.errs[rel] = fmt.Errorf("openat2 %s: %w", rel, unix.EXDEV)
	f.links[rel] = target
}

// directory registers rel as a directory.
func (f *fakeDevFS) directory(rel string) {
	f.t.Helper()

	err := os.MkdirAll(filepath.Join(f.dir, rel), 0o755)
	if err != nil {
		f.t.Fatal(err)
	}

	f.nodes[rel] = fakeNode{kind: nodeDir, path: devRoot + "/" + rel}
}

func kindOf(id deviceID) nodeKind {
	if id.typ == "b" {
		return nodeBlock
	}

	return nodeChar
}

var (
	devSda  = deviceID{typ: "b", major: 8, minor: 0}
	devCard = deviceID{typ: "c", major: 226, minor: 0}
	devRend = deviceID{typ: "c", major: 226, minor: 128}
	devNull = deviceID{typ: "c", major: 1, minor: 3}
)

func collectFrom(dev devFS, gpol policy.Global, sources ...string) containerRules {
	mounts := make([]container.MountPoint, 0, len(sources))
	for _, source := range sources {
		mounts = append(
			mounts,
			container.MountPoint{Source: source, Destination: source, Type: mount.TypeBind},
		)
	}

	return collectContainerRules(dev, "abc", 1, mounts, gpol, policy.Container{}, nil)
}

func ruleSet(rules []cgroup.DeviceRule) map[string]struct{} {
	set := make(map[string]struct{}, len(rules))
	for _, rule := range rules {
		set[rule.Type+" "+strconv.FormatInt(*rule.Major, 10)+":"+strconv.FormatInt(*rule.Minor, 10)] = struct{}{}
	}

	return set
}

func warnLines(logOutput string) []string {
	var lines []string

	for line := range strings.SplitSeq(logOutput, "\n") {
		if strings.Contains(line, "level=WARN") {
			lines = append(lines, line)
		}
	}

	return lines
}

// wantRules fails unless the collection has no errors and grants exactly
// the listed devices.
func wantRules(t *testing.T, got containerRules, want ...deviceID) {
	t.Helper()

	if len(got.deviceErrs) != 0 {
		t.Fatalf("unexpected errors: %v", got.deviceErrs)
	}

	set := ruleSet(got.granted)
	if len(set) != len(want) || len(got.granted) != len(want) {
		t.Fatalf("granted %v, want %v", set, want)
	}

	for _, id := range want {
		if _, ok := set[id.typ+" "+strconv.FormatInt(id.major, 10)+":"+strconv.FormatInt(id.minor, 10)]; !ok {
			t.Fatalf("granted %v, want %v", set, want)
		}
	}

	for _, rule := range got.granted {
		if !rule.Allow || rule.Access != "rwm" {
			t.Errorf("rule has unexpected allow/access: %+v", rule)
		}
	}
}

// wantUnresolved fails unless the collection reports an unresolved identity
// naming every fragment; computeDesired then empties the whole set (see
// TestProcessor_UnresolvedCandidateEmptiesTheSet).
func wantUnresolved(t *testing.T, got containerRules, fragments ...string) {
	t.Helper()

	wantIncomplete(t, got, errUnresolved, fragments...)
}

func wantIncomplete(t *testing.T, got containerRules, sentinel error, fragments ...string) {
	t.Helper()

	if len(got.deviceErrs) == 0 {
		t.Fatalf("want a %v error, got none", sentinel)
	}

	joined := errors.Join(got.deviceErrs...)
	if !errors.Is(joined, sentinel) {
		t.Errorf("err = %v, want %v", joined, sentinel)
	}

	for _, fragment := range fragments {
		if !strings.Contains(joined.Error(), fragment) {
			t.Errorf("err = %v, want it to contain %q", joined, fragment)
		}
	}
}

func TestEvaluateIdentity(t *testing.T) {
	t.Parallel()

	const (
		alias     = "/dev/disk/by-id/usb-x"
		resolved  = "/dev/sda"
		canonical = "/dev/sda"
	)

	for _, tc := range []struct {
		name string
		gpol policy.Global
		cpol policy.Container
		want decision
	}{
		{"no policy", policy.Global{}, policy.Container{}, decision{authorized: true}},
		{
			"alias denied",
			policy.Global{DeviceDeny: []string{"/dev/disk/by-id/*"}},
			policy.Container{},
			decision{deniedBy: alias},
		},
		{
			"canonical denied by the container",
			policy.Global{},
			policy.Container{DeviceDeny: []string{"/dev/sd*"}},
			decision{deniedBy: resolved},
		},
		{
			"alias allowed but not the device",
			policy.Global{DeviceAllow: []string{"/dev/disk/by-id/*"}},
			policy.Container{},
			decision{},
		},
		{
			"device allowed, alias not listed",
			policy.Global{DeviceAllow: []string{"/dev/sda"}},
			policy.Container{},
			decision{authorized: true},
		},
		{
			"container allow list narrows",
			policy.Global{DeviceAllow: []string{"/dev/sd*"}},
			policy.Container{DeviceAllow: []string{"/dev/sdb"}},
			decision{},
		},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got := evaluateIdentity(alias, resolved, canonical, tc.gpol, tc.cpol)
			if got != tc.want {
				t.Errorf("decision = %+v, want %+v", got, tc.want)
			}
		})
	}

	// Authorized needs both the canonical and the resolved name.
	got := evaluateIdentity("/dev/x", "/dev/dri/card0", "/dev/sda",
		policy.Global{DeviceAllow: []string{"/dev/dri/*"}}, policy.Container{})
	if got.authorized {
		t.Error("authorized on the resolved name alone")
	}
}

func TestCanonicalName(t *testing.T) {
	t.Parallel()

	for _, tc := range []struct {
		name, uevent, want string
	}{
		{"plain", "MAJOR=8\nDEVNAME=sda\nDEVTYPE=disk\n", "/dev/sda"},
		{"nested", "DEVNAME=dri/card0\n", "/dev/dri/card0"},
		{"missing", "MAJOR=8\n", ""},
		{"two lines", "DEVNAME=sda\nDEVNAME=sdb\n", ""},
		{"empty", "DEVNAME=\n", ""},
		{"absolute", "DEVNAME=/dev/sda\n", ""},
		{"dot-dot", "DEVNAME=../sda\n", ""},
		{"inner dot-dot", "DEVNAME=dri/../../sda\n", ""},
		{"unclean", "DEVNAME=dri//card0\n", ""},
		{"trailing slash", "DEVNAME=sda/\n", ""},
	} {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			dev := newFakeDevFS(t)
			dev.uevents[devSda] = tc.uevent

			got, err := canonicalName(dev, devSda)
			if tc.want == "" {
				if !errors.Is(err, errBadDevname) {
					t.Errorf("canonicalName = %q, %v; want errBadDevname", got, err)
				}

				return
			}

			if err != nil || got != tc.want {
				t.Errorf("canonicalName = %q, %v; want %q", got, err, tc.want)
			}
		})
	}
}

// A node planted under /dev/shm by anyone who can write there is judged by
// its sysfs identity, not by where it was planted.
func TestCollect_PlantedNodeUnderShmJudgedAsSda(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.directory("shm")
	dev.device("shm/evil", devSda, "sda")

	allowShm := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/shm/*"}}
	wantRules(t, collectFrom(dev, allowShm, "/dev/shm"))

	denySda := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/sda"}}
	wantRules(t, collectFrom(dev, denySda, "/dev/shm"))
}

func TestCollect_SymlinkOutsideDevRejected(t *testing.T) {
	dev := newFakeDevFS(t)
	dev.directory("dri")
	dev.absLink("dri/x", "/tmp/x")
	dev.absLink("dri/up", "/dev/../tmp/x")

	buf := captureLogger(t)

	got := collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/dri")
	wantRules(t, got)

	if got.skipped != 2 {
		t.Errorf("skipped = %d, want 2", got.skipped)
	}

	if strings.Count(buf.String(), "device path resolves outside /dev") != 2 ||
		!strings.Contains(buf.String(), "reason=outside_dev") {
		t.Errorf("want one outside_dev WARN per link, got:\n%s", buf.String())
	}
}

func TestCollect_RelativeSymlinkPolicyOnAllThreeNames(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.device("sda", devSda, "sda")
	dev.directory("disk/by-id")
	dev.alias("disk/by-id/usb-x", "sda") // ../../sda

	const source = "/dev/disk/by-id/usb-x"

	wantRules(t, collectFrom(dev, policy.Global{Mode: policy.ModeAll}, source), devSda)

	for _, deny := range []string{"/dev/disk/by-id/*", "/dev/sda"} {
		gpol := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{deny}}
		wantRules(t, collectFrom(dev, gpol, source))
	}

	aliasOnly := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/disk/by-id/*"}}
	wantRules(t, collectFrom(dev, aliasOnly, source))

	canonical := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/sda"}}
	wantRules(t, collectFrom(dev, canonical, source), devSda)
}

// A deny on one name holds against the device under every other name, even
// one that is walked separately and allowed.
func TestCollect_AliasDenySuppressesDeviceAcrossMounts(t *testing.T) {
	dev := newFakeDevFS(t)
	dev.device("sda", devSda, "sda")
	dev.directory("disk/by-id")
	dev.alias("disk/by-id/usb-x", "sda")

	gpol := policy.Global{
		Mode:        policy.ModeAll,
		DeviceAllow: []string{"/dev/sda"},
		DeviceDeny:  []string{"/dev/disk/by-id/*"},
	}

	buf := captureLogger(t)

	for _, order := range [][]string{
		{"/dev/sda", "/dev/disk/by-id"},
		{"/dev/disk/by-id", "/dev/sda"},
	} {
		got := collectFrom(dev, gpol, order...)
		wantRules(t, got)

		if got.skipped != 2 {
			t.Errorf("%v: skipped = %d, want both candidates", order, got.skipped)
		}
	}

	if !strings.Contains(buf.String(), "device denied by policy under one of its names") ||
		!strings.Contains(buf.String(), "denied_by=/dev/disk/by-id/usb-x") {
		t.Errorf("want an INFO naming the denying alias, got:\n%s", buf.String())
	}
}

func TestCollect_ByPathAliasGrantedByCanonical(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.directory("dri")
	dev.device("dri/card0", devCard, "dri/card0")
	dev.device("dri/renderD128", devRend, "dri/renderD128")
	dev.directory("dri/by-path")
	dev.alias("dri/by-path/pci-0000:00:02.0-card", "dri/card0")

	gpol := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/dri/*"}}

	got := collectFrom(dev, gpol, "/dev/dri")
	wantRules(t, got, devCard, devRend)

	if got.skipped != 0 {
		t.Errorf("skipped = %d, want 0", got.skipped)
	}
}

// openat2 refuses an absolute symlink even into /dev; the retry follows it
// by hand so a planted /dev/evil -> /dev/sda is still judged under both
// names.
func TestCollect_AbsoluteSymlinkUnderDeniedAliasVetoed(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.device("sda", devSda, "sda")
	dev.directory("evil")
	dev.absLink("evil/link", "/dev/sda")

	gpol := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/evil/*"}}
	wantRules(t, collectFrom(dev, gpol, "/dev/sda", "/dev/evil"))

	wantRules(t, collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/evil"), devSda)
}

// A directory mounted through an absolute symlink into /dev is walked, and
// each entry is still followed through the link and judged under its alias.
func TestCollect_DirectoryBehindAbsoluteSymlink(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.directory("dri")
	dev.device("dri/card0", devCard, "dri/card0")
	dev.absLink("gpu", "/dev/dri")

	allow := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/dri/*"}}
	wantRules(t, collectFrom(dev, allow, "/dev/gpu"), devCard)

	deny := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/gpu/*"}}
	wantRules(t, collectFrom(dev, deny, "/dev/gpu"))
}

func TestCollect_AbsoluteSymlinkLoopUnresolved(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.absLink("a", "/dev/b")
	dev.absLink("b", "/dev/a")

	wantUnresolved(t, collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/a"),
		"/dev/a", "too many levels")
}

func TestCollect_DotDotSourceRejected(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.device("sda", devSda, "sda")

	for _, source := range []string{"/dev/../etc/passwd", "/dev//sda", "/dev/sda/"} {
		got := collectFrom(dev, policy.Global{Mode: policy.ModeAll}, source)
		wantRules(t, got)

		if got.skipped != 1 {
			t.Errorf("%s: skipped = %d, want 1", source, got.skipped)
		}
	}
}

func TestCollect_UeventProblemsEmptyTheSet(t *testing.T) {
	t.Parallel()

	for name, uevent := range map[string]string{
		"missing":         "",
		"no DEVNAME":      "MAJOR=8\n",
		"two DEVNAME":     "DEVNAME=sda\nDEVNAME=sdb\n",
		"DEVNAME=../sda":  "DEVNAME=../sda\n",
		"absolute":        "DEVNAME=/dev/sda\n",
		"empty DEVNAME":   "DEVNAME=\n",
		"unclean DEVNAME": "DEVNAME=./sda\n",
	} {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			dev := newFakeDevFS(t)
			dev.device("null", devNull, "null")
			dev.device("sda", devSda, "sda")

			if uevent == "" {
				delete(dev.uevents, devSda)
			} else {
				dev.uevents[devSda] = uevent
			}

			got := collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/null", "/dev/sda")
			wantUnresolved(t, got, "/dev/sda", "b 8:0")
		})
	}
}

// Without a canonical name a device cannot be authorized; when its other
// names already exclude it the set is still known.
func TestCollect_UeventMissingButExcludedByPolicy(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.directory("pts")
	dev.device("pts/0", deviceID{typ: "c", major: 136, minor: 0}, "")
	delete(dev.uevents, deviceID{typ: "c", major: 136, minor: 0})
	dev.device("null", devNull, "null")

	notAllowed := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/null"}}
	wantRules(t, collectFrom(dev, notAllowed, "/dev/null", "/dev/pts"), devNull)

	denied := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/pts/*"}}
	wantRules(t, collectFrom(dev, denied, "/dev/null", "/dev/pts"), devNull)

	wantUnresolved(
		t,
		collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/pts"),
		"/dev/pts/0",
	)
}

// The walk only enumerates names: an entry swapped after enumeration is
// judged by what its descriptor is, not by the name it had.
func TestCollect_EntryReplacedAfterEnumerationFdIdentityWins(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.device("sda", devSda, "sda")
	dev.directory("dri")
	dev.touch("dri/card0")
	// By the time it is opened, dri/card0 is sda.
	dev.nodes["dri/card0"] = fakeNode{kind: nodeBlock, id: devSda, path: "/dev/dri/card0"}

	gpol := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/dri/*"}}
	wantRules(t, collectFrom(dev, gpol, "/dev/dri"))

	all := policy.Global{Mode: policy.ModeAll}
	got := collectFrom(dev, all, "/dev/dri")
	wantRules(t, got, devSda)
}

func TestCollect_OtherOpenErrorUnresolved(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.directory("dri")
	dev.touch("dri/card0")
	dev.errs["dri/card0"] = fmt.Errorf("openat2 dri/card0: %w", unix.EACCES)

	wantUnresolved(
		t,
		collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/dri"),
		"/dev/dri/card0",
	)
}

func TestCollect_MissingSourceUnresolved(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)

	wantUnresolved(
		t,
		collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/ttyUSB0"),
		"/dev/ttyUSB0",
	)
}

func TestCollect_SingleFileNonDevice(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.touch("regular")
	dev.nodes["regular"] = fakeNode{kind: nodeOther, path: "/dev/regular"}

	got := collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/regular")
	if len(got.granted) != 0 || len(got.deviceErrs) != 1 ||
		!errors.Is(got.deviceErrs[0], errNotDevice) {
		t.Fatalf("want one errNotDevice, got rules=%v errs=%v", got.granted, got.deviceErrs)
	}
}

// Dangling names and non-device entries of a directory mount are skipped,
// never errors; only a dangling name an explicit allow glob matches WARNs.
func TestCollect_DirectorySkips(t *testing.T) {
	dev := newFakeDevFS(t)
	dev.directory("d")
	dev.device("null", devNull, "null")
	dev.alias("d/null", "null")
	dev.touch("d/nullish") // dangling: not registered
	dev.touch("d/regular")
	dev.nodes["d/regular"] = fakeNode{kind: nodeOther, path: "/dev/d/regular"}
	dev.directory("d/sub")
	dev.touch("d/linktodir")
	dev.nodes["d/linktodir"] = fakeNode{kind: nodeDir, path: "/dev/d/sub"}

	buf := captureLogger(t)

	gpol := policy.Global{
		Mode:        policy.ModeAll,
		DeviceAllow: []string{"/dev/null", "/dev/d/nullish"},
	}

	got := collectFrom(dev, gpol, "/dev/d")
	wantRules(t, got, devNull)

	if got.skipped != 3 {
		t.Errorf("skipped = %d, want nullish, regular and linktodir", got.skipped)
	}

	warns := warnLines(buf.String())
	if len(warns) != 1 ||
		!strings.Contains(warns[0], "device symlink matches allow policy but cannot be resolved") ||
		!strings.Contains(warns[0], "path=/dev/d/nullish") {
		t.Errorf("want one WARN for nullish, got %v", warns)
	}

	for _, debug := range []string{"non-device entry skipped", "symlink to directory skipped"} {
		if !strings.Contains(buf.String(), debug) {
			t.Errorf("want DEBUG %q, got:\n%s", debug, buf.String())
		}
	}
}

// Every way a directory mount's names can go unseen leaves the set unknown:
// an alias that was not enumerated might deny a device that was.
func TestCollect_IncompleteEnumeration(t *testing.T) {
	t.Parallel()

	for name, tc := range map[string]struct {
		setup func(dev *fakeDevFS)
		want  error
	}{
		"root cannot be opened": {func(dev *fakeDevFS) {
			dev.directory("dri")
			dev.device("dri/card0", devCard, "dri/card0")
			dev.errs["dri"] = fmt.Errorf("openat2 dri: %w", unix.EACCES)
		}, errUnresolved},
		"root vanished before the walk": {func(dev *fakeDevFS) {
			dev.nodes["dri"] = fakeNode{kind: nodeDir, path: "/dev/dri"}
		}, errIncompleteWalk},
		"directory read error": {func(dev *fakeDevFS) {
			dev.directory("dri")
			dev.directory("dri/by-path")
			dev.walkErrs["dri/by-path"] = unix.EACCES
		}, errIncompleteWalk},
		"entry error": {func(dev *fakeDevFS) {
			dev.directory("dri")
			dev.touch("dri/renderD128")
			dev.walkErrs["dri/renderD128"] = unix.EIO
		}, errIncompleteWalk},
	} {
		t.Run(name, func(t *testing.T) {
			t.Parallel()

			dev := newFakeDevFS(t)
			dev.device("null", devNull, "null")
			tc.setup(dev)

			got := collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/null", "/dev/dri")
			wantIncomplete(t, got, tc.want, "/dev/dri")

			proc := &Processor{devfs: func() (devFS, error) { return dev, nil }}
			info := &container.InspectResponse{
				State: &container.State{Running: true, Pid: 1},
				Mounts: []container.MountPoint{
					{Source: "/dev/null", Destination: "/dev/null"},
					{Source: "/dev/dri", Destination: "/dev/dri"},
				},
			}

			desired := proc.computeDesired(
				"abc",
				info,
				config.Runtime{Policy: policy.Global{Mode: policy.ModeAll}},
			)
			if len(desired.rules) != 0 || !errors.Is(desired.incomplete, tc.want) {
				t.Errorf("desired = %+v, want an empty set and a %v error", desired, tc.want)
			}
		})
	}
}

func TestCollect_EntryCapOverflowIsAnError(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.directory("big")

	for idx := range maxMountEntries + 1 {
		dev.device(
			"big/n"+strconv.Itoa(idx),
			deviceID{typ: "c", major: 250, minor: int64(idx)},
			"n",
		)
	}

	got := collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/big")
	wantIncomplete(t, got, errMountTooLarge, "/dev/big", "narrow the bind mount")

	// At the cap exactly, the mount is enumerated in full.
	delete(dev.nodes, "big/n0")

	err := os.Remove(filepath.Join(dev.dir, "big", "n0"))
	if err != nil {
		t.Fatal(err)
	}

	got = collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/big")
	if len(got.deviceErrs) != 0 || len(got.granted) != maxMountEntries {
		t.Errorf("at the cap: granted %d, errs %v; want %d and none",
			len(got.granted), got.deviceErrs, maxMountEntries)
	}
}

func TestCollect_DirectoryNoChildrenMatchWarns(t *testing.T) {
	dev := newFakeDevFS(t)
	dev.directory("dri")
	dev.device("dri/card0", devCard, "dri/card0")
	dev.device("dri/renderD128", devRend, "dri/renderD128")

	buf := captureLogger(t)

	gpol := policy.Global{Mode: policy.ModeAll, DeviceAllow: []string{"/dev/dri/nonexistent"}}
	wantRules(t, collectFrom(dev, gpol, "/dev/dri"))

	logOutput := buf.String()
	if !strings.Contains(logOutput, "mount excluded: no children matched") ||
		!strings.Contains(logOutput, "card0") || !strings.Contains(logOutput, "renderD128") {
		t.Errorf("want a WARN naming the children, got:\n%s", logOutput)
	}
}

func TestProcessor_DevFSOpenFailureEmptiesTheSet(t *testing.T) {
	t.Parallel()

	proc := &Processor{devfs: func() (devFS, error) { return nil, unix.ENOSYS }}
	info := &container.InspectResponse{
		State:  &container.State{Running: true, Pid: 1},
		Mounts: []container.MountPoint{{Source: "/dev/null", Destination: "/dev/null"}},
	}

	desired := proc.computeDesired(
		"abc",
		info,
		config.Runtime{Policy: policy.Global{Mode: policy.ModeAll}},
	)
	if len(desired.rules) != 0 || desired.incomplete == nil ||
		!strings.Contains(desired.incomplete.Error(), "retryable") {
		t.Errorf("desired = %+v, want an empty set and a retryable error", desired)
	}
}

func TestProcessor_UnresolvedCandidateEmptiesTheSet(t *testing.T) {
	t.Parallel()

	dev := newFakeDevFS(t)
	dev.device("null", devNull, "null")
	dev.device("sda", devSda, "sda")
	delete(dev.uevents, devSda)

	proc := &Processor{devfs: func() (devFS, error) { return dev, nil }}
	info := &container.InspectResponse{
		State: &container.State{Running: true, Pid: 1},
		Mounts: []container.MountPoint{
			{Source: "/dev/null", Destination: "/dev/null"},
			{Source: "/dev/sda", Destination: "/dev/sda"},
		},
	}

	desired := proc.computeDesired(
		"abc",
		info,
		config.Runtime{Policy: policy.Global{Mode: policy.ModeAll}},
	)
	if len(desired.rules) != 0 || desired.incomplete == nil {
		t.Fatalf("desired = %+v, want an empty set and an error", desired)
	}

	msg := desired.incomplete.Error()
	for _, fragment := range []string{"incomplete_device_set, retryable", "/dev/sda", "b 8:0"} {
		if !strings.Contains(msg, fragment) {
			t.Errorf("err = %q, want %q", msg, fragment)
		}
	}
}

func TestProbeOpenat2(t *testing.T) {
	t.Parallel()

	err := ProbeOpenat2()
	if err != nil && !errors.Is(err, errOpenat2Unsupported) {
		t.Errorf("ProbeOpenat2 = %v, want nil or errOpenat2Unsupported", err)
	}
}

// realDev opens the host's /dev for tests that need a real device node.
func realDev(t *testing.T) *realDevFS {
	t.Helper()

	err := ProbeOpenat2()
	if err != nil {
		t.Skipf("openat2 unavailable: %v", err)
	}

	dev, err := openDevFS(devRoot, "/sys")
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(func() { _ = dev.Close() })

	return dev
}

func TestCollect_RealDevNull(t *testing.T) {
	t.Parallel()

	dev := realDev(t)

	wantRules(t, collectFrom(dev, policy.Global{Mode: policy.ModeAll}, "/dev/null"), devNull)

	denied := policy.Global{Mode: policy.ModeAll, DeviceDeny: []string{"/dev/null"}}

	got := collectFrom(dev, denied, "/dev/null")
	wantRules(t, got)

	if got.skipped != 1 {
		t.Errorf("skipped = %d, want 1", got.skipped)
	}
}

// TestRealDevFS_Openat2Containment runs the real openat2 against regular
// files: RESOLVE_BENEATH refuses an absolute symlink and a relative one that
// climbs out, and readLink returns the link itself.
func TestRealDevFS_Openat2Containment(t *testing.T) {
	t.Parallel()

	err := ProbeOpenat2()
	if err != nil {
		t.Skipf("openat2 unavailable: %v", err)
	}

	outside := filepath.Join(t.TempDir(), "secret")
	root := t.TempDir()

	for _, step := range []error{
		os.WriteFile(outside, nil, 0o600),
		os.WriteFile(filepath.Join(root, "inside"), nil, 0o600),
		os.Symlink(outside, filepath.Join(root, "abs")),
		os.Symlink("../"+filepath.Base(filepath.Dir(outside))+"/secret", filepath.Join(root, "climb")),
		os.Symlink("inside", filepath.Join(root, "rel")),
	} {
		if step != nil {
			t.Fatal(step)
		}
	}

	dev, err := openDevFS(root, "/sys")
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(func() { _ = dev.Close() })

	for _, name := range []string{"abs", "climb"} {
		node, openErr := dev.openBeneath(name)
		if !errors.Is(openErr, unix.EXDEV) {
			if node != nil {
				_ = node.Close()
			}

			t.Errorf("openBeneath(%s) = %v, want EXDEV", name, openErr)
		}
	}

	target, err := dev.readLink("abs")
	if err != nil || target != outside {
		t.Errorf("readLink(abs) = %q, %v; want %q", target, err, outside)
	}

	node, err := dev.openBeneath("rel")
	if err != nil {
		t.Fatal(err)
	}

	defer func() { _ = node.Close() }()

	kind, _, err := node.stat()
	if err != nil || kind != nodeOther {
		t.Errorf("stat = %v, %v; want a regular file", kind, err)
	}

	if node.resolved() != devRoot+"/inside" {
		t.Errorf("resolved = %q, want /dev/inside", node.resolved())
	}
}
