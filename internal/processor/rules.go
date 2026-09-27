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
	"bufio"
	"bytes"
	"errors"
	"fmt"
	"io/fs"
	"path/filepath"
	"slices"
	"strings"

	"golang.org/x/sys/unix"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
	"github.com/leinardi/swarm-device-access/internal/logger"
	"github.com/leinardi/swarm-device-access/internal/policy"
)

const (
	// maxMountEntries bounds the names enumerated under one directory
	// mount. A walk that cannot see every name cannot know that no alias
	// denies a device it did see, so overflow is an error, not a cutoff.
	maxMountEntries = 4096
	maxLogChildren  = 32
	// maxLinkHops bounds the absolute symlinks followed by hand (see
	// evaluateCandidate), like the kernel's own symlink limit.
	maxLinkHops = 40

	deviceAccessAll = "rwm"
)

var (
	// errNotDevice is returned (wrapped) for a mount source that exists but
	// is neither a character nor a block device.
	errNotDevice = errors.New("neither character nor block device")
	// errUnresolved marks a candidate whose device identity could not be
	// established; the container's whole device set is then unknown.
	errUnresolved = errors.New("device identity could not be established")
	// errBadDevname reports a sysfs uevent without exactly one valid DEVNAME.
	errBadDevname = errors.New("uevent has no single valid DEVNAME")
	// errTooManyLinks reports a chain of absolute symlinks over maxLinkHops.
	errTooManyLinks = errors.New("too many levels of symbolic links")
	// errIncompleteWalk marks a directory mount whose names could not all
	// be enumerated; like an unresolved candidate, it leaves the set unknown.
	errIncompleteWalk = errors.New("device mount enumeration incomplete")
	// errMountTooLarge reports a directory mount over maxMountEntries.
	errMountTooLarge = errors.New("mount too large; narrow the bind mount")
)

// Candidate skip reasons, also the sda_device_candidates_skipped_total
// reason label.
const (
	skipOutsideDev = "outside_dev"
	skipDangling   = "dangling"
	skipNotDevice  = "not_device"
)

// decision is the policy outcome for one candidate.
type decision struct {
	// deniedBy is the name that matched a deny glob; empty when none did.
	deniedBy string
	// authorized is set when the canonical and the resolved name both pass
	// the allow lists.
	authorized bool
}

// candidate is one name found under a /dev mount (the mount source itself,
// or an entry of a directory mount) and what it resolved to.
type candidate struct {
	alias     string // the name as found
	resolved  string // the /dev path its descriptor refers to
	canonical string // /dev/ + the device's sysfs DEVNAME
	id        deviceID
	kind      nodeKind
	skip      string // non-empty: not a device candidate, see skip reasons
	decision  decision
}

// MountResult is the outcome of collecting the candidates of one mount
// source. Skipped counts entries that name no device (dangling or outside
// /dev symlinks, non-device entries); Errs holds failures that leave the
// container's device set unknown.
type MountResult struct {
	Candidates []candidate
	Skipped    int
	Errs       []error
	// Outcomes counts the skipped entries per skip reason.
	Outcomes map[string]int
}

// IsMountSource reports whether path is /dev or a path under /dev/.
func IsMountSource(path string) bool {
	return path == "/dev" || strings.HasPrefix(path, "/dev/")
}

// underDev is the lexical gate on a name: already clean (no "..", "//" or
// trailing "/") and /dev or below.
func underDev(path string) bool {
	return filepath.Clean(path) == path && IsMountSource(path)
}

// relToDev returns path relative to /dev ("." for /dev itself).
func relToDev(path string) string {
	if path == devRoot {
		return "."
	}

	return strings.TrimPrefix(path, devRoot+"/")
}

// CollectMountRules evaluates the mount source and, for a directory, every
// entry below it. Each name goes through evaluateCandidate on descriptors;
// the walk only enumerates names.
func CollectMountRules(
	dev devFS,
	mountPath string,
	gpol policy.Global,
	cpol policy.Container,
) MountResult {
	result := MountResult{Outcomes: make(map[string]int)}

	source := evaluateCandidate(dev, mountPath, gpol, cpol)

	switch {
	case source.err != nil:
		result.Errs = append(result.Errs, source.err)

		return result
	case source.skip == skipDangling:
		// Docker mounted it, so it existed; a device that went away
		// leaves the set unknown until it is back or the container ends.
		result.Errs = append(result.Errs,
			fmt.Errorf("%w: mount source %s: %w", errUnresolved, mountPath, unix.ENOENT))

		return result
	case source.skip != "":
		result.addSkip(source.skip)

		return result
	case source.kind == nodeDir:
		return walkMount(dev, mountPath, source.resolved, gpol, cpol, result)
	case source.kind == nodeOther:
		// A single file mounted from /dev must be a device.
		result.Errs = append(result.Errs, fmt.Errorf("device %q is %w", mountPath, errNotDevice))

		return result
	default:
		result.Candidates = append(result.Candidates, source.candidate)

		return result
	}
}

func (r *MountResult) addSkip(reason string) {
	r.Skipped++
	r.Outcomes[reason]++
}

// walkMount enumerates a directory mount. dir is the resolved directory; the
// candidates are named after the mount source (their alias).
func walkMount(
	dev devFS,
	source, dir string,
	gpol policy.Global,
	cpol policy.Container,
	result MountResult,
) MountResult {
	state := &mountWalkState{
		source: source,
		base:   filepath.Join(dev.root(), relToDev(dir)),
		dev:    dev,
		gpol:   gpol,
		cpol:   cpol,
		result: result,
	}

	walkErr := dev.walk(state.base, state.visitEntry)
	if walkErr != nil {
		state.result.Errs = append(state.result.Errs,
			fmt.Errorf("%w: %s: %w", errIncompleteWalk, source, walkErr))
	}

	granted := 0

	for idx := range state.result.Candidates {
		verdict := state.result.Candidates[idx].decision
		if verdict.deniedBy == "" && verdict.authorized {
			granted++
		}
	}

	if granted == 0 && state.policyExcluded > 0 && len(state.result.Errs) == 0 {
		logger.L().Warn("mount excluded: no children matched allow/deny policy",
			"path", source,
			"children_seen", state.childrenSeen,
			// Concat, not append: append could write the container's
			// globs into the spare capacity of the stored config slice.
			"allow_globs", slices.Concat(gpol.DeviceAllow, cpol.DeviceAllow),
			"deny_globs", slices.Concat(gpol.DeviceDeny, cpol.DeviceDeny),
		)
	}

	return state.result
}

type mountWalkState struct {
	source string
	base   string
	dev    devFS
	gpol   policy.Global
	cpol   policy.Container
	result MountResult
	// policyExcluded counts device candidates the policy did not grant.
	policyExcluded int
	childrenSeen   []string
	// entries counts every name enumerated, directories included.
	entries int
}

func (s *mountWalkState) visitEntry(walkedPath string, entry fs.DirEntry, entryErr error) error {
	alias := s.source + strings.TrimPrefix(walkedPath, s.base)

	// The root failing to open, a directory that cannot be read, any walk
	// error: some name was not seen, so the set is unknown.
	if entryErr != nil {
		s.result.Errs = append(s.result.Errs,
			fmt.Errorf("%w: %s: %w", errIncompleteWalk, alias, entryErr))

		return nil
	}

	if walkedPath == s.base {
		return nil
	}

	s.entries++
	if s.entries > maxMountEntries {
		s.result.Errs = append(s.result.Errs, fmt.Errorf("%w: %s: %w: more than %d entries",
			errIncompleteWalk, s.source, errMountTooLarge, maxMountEntries))

		return filepath.SkipAll
	}

	if entry.IsDir() {
		return nil
	}

	s.trackChild(walkedPath)

	found := evaluateCandidate(s.dev, alias, s.gpol, s.cpol)

	switch {
	case found.err != nil:
		s.result.Errs = append(s.result.Errs, found.err)
	case found.skip != "":
		if found.skip == skipDangling {
			skipUnresolvable(alias, s.gpol, s.cpol)
		}

		s.result.addSkip(found.skip)
	case found.kind == nodeDir:
		logger.L().Debug("symlink to directory skipped", "path", alias, "target", found.resolved)
		s.result.addSkip(skipNotDevice)
	case found.kind == nodeOther:
		logger.L().Debug("non-device entry skipped", "path", alias)
		s.result.addSkip(skipNotDevice)
	default:
		if found.decision.deniedBy != "" || !found.decision.authorized {
			s.policyExcluded++
		}

		s.result.Candidates = append(s.result.Candidates, found.candidate)
	}

	return nil
}

func (s *mountWalkState) trackChild(path string) {
	if len(s.childrenSeen) < maxLogChildren {
		s.childrenSeen = append(s.childrenSeen, filepath.Base(path))
	}
}

// evaluated is a candidate, or why it is not one.
type evaluated struct {
	candidate

	// err is set when the name exists but its identity could not be
	// established.
	err error
}

// evaluateCandidate establishes what alias names, on descriptors rather
// than re-walked paths, so an entry swapped between enumeration and use
// cannot lend its name to another device:
//
//  1. alias must be clean and under /dev;
//  2. it is opened beneath /dev (openat2 RESOLVE_BENEATH): symlinks are
//     followed but nothing outside /dev is reached. A planted absolute link
//     into /dev (-> /dev/sda) is followed by hand, bounded; any other escape
//     is skipped as outside_dev, a dangling name as dangling;
//  3. fstat on the descriptor gives the type and major:minor;
//  4. sysfs gives the canonical name (DEVNAME);
//  5. policy is evaluated on the alias, the resolved and the canonical name
//     (evaluateIdentity).
//
// A name that exists but whose identity cannot be established returns err:
// the caller must then treat the container's whole device set as unknown.
func evaluateCandidate(
	dev devFS,
	alias string,
	gpol policy.Global,
	cpol policy.Container,
) evaluated {
	found := evaluated{candidate: candidate{alias: alias}}

	if !underDev(alias) {
		logger.L().
			Warn("device path resolves outside /dev", "path", alias, "reason", skipOutsideDev)
		found.skip = skipOutsideDev

		return found
	}

	node, skip, err := openCandidate(dev, alias)
	if err != nil || skip != "" {
		found.skip = skip
		found.err = err

		return found
	}

	defer func() {
		closeErr := node.Close()
		if closeErr != nil {
			logger.L().Warn("close device node", "path", alias, "err", closeErr)
		}
	}()

	found.kind, found.id, err = node.stat()
	if err != nil {
		found.err = unresolvedErr(alias, "", nil, err)

		return found
	}

	found.resolved = node.resolved()
	if found.resolved == "" {
		found.resolved = alias
	}

	if found.kind != nodeChar && found.kind != nodeBlock {
		return found
	}

	found.canonical, err = canonicalName(dev, found.id)
	if err == nil {
		found.decision = evaluateIdentity(alias, found.resolved, found.canonical, gpol, cpol)

		return found
	}

	// Without a canonical name the device can never be authorized, so the
	// candidate only matters if its other names would grant it: then the
	// set is unknown. Excluded by policy under those names, it is simply
	// not granted.
	found.decision = evaluateIdentity(alias, found.resolved, found.resolved, gpol, cpol)
	if found.decision.deniedBy == "" && found.decision.authorized {
		found.err = unresolvedErr(alias, found.resolved, &found.id, err)
	}

	found.decision.authorized = false

	return found
}

// openCandidate opens alias beneath /dev, following a planted absolute
// symlink into /dev by hand (up to maxLinkHops). skip is set for a name
// that names nothing under /dev.
//
//nolint:ireturn // devNode is the devFS seam
func openCandidate(dev devFS, alias string) (devNode, string, error) {
	rel := relToDev(alias)

	for range maxLinkHops {
		node, err := dev.openBeneath(rel)

		switch {
		case err == nil:
			return node, "", nil
		case errors.Is(err, unix.ENOENT):
			return nil, skipDangling, nil
		case !errors.Is(err, unix.EXDEV):
			return nil, "", unresolvedErr(alias, "", nil, err)
		}

		next, target, err := escapeHop(dev, rel)
		if err != nil {
			return nil, "", unresolvedErr(alias, "", nil, err)
		}

		if next == "" {
			logger.L().Warn("device path resolves outside /dev",
				"path", alias, "target", target, "reason", skipOutsideDev)

			return nil, skipOutsideDev, nil
		}

		rel = next
	}

	return nil, "", unresolvedErr(alias, "", nil, errTooManyLinks)
}

// escapeHop handles a lookup of rel that left /dev (EXDEV). The kernel
// refuses an absolute symlink even when it points back into /dev, so the
// first component that escapes is found and its link read: an absolute
// target inside /dev replaces that prefix and next is the path to retry.
// next is empty when the escape really leaves /dev; target is then what
// it pointed at.
func escapeHop(dev devFS, rel string) (next, target string, err error) {
	parts := strings.Split(rel, "/")

	for idx := range parts {
		prefix := strings.Join(parts[:idx+1], "/")

		node, openErr := dev.openBeneath(prefix)
		if openErr == nil {
			closeErr := node.Close()
			if closeErr != nil {
				logger.L().Warn("close device node", "path", prefix, "err", closeErr)
			}

			continue
		}

		if !errors.Is(openErr, unix.EXDEV) {
			return "", "", openErr
		}

		target, err = dev.readLink(prefix)
		if err != nil {
			return "", "", err
		}

		cleaned := filepath.Clean(target)
		if !filepath.IsAbs(target) || !underDev(cleaned) {
			return "", target, nil
		}

		return filepath.Join(append([]string{relToDev(cleaned)}, parts[idx+1:]...)...), target, nil
	}

	// Every prefix opens on its own: the whole name no longer escapes.
	return rel, "", nil
}

// canonicalName reads the device's DEVNAME from sysfs. The uevent must hold
// exactly one DEVNAME line whose value is a clean relative path without
// "..".
func canonicalName(dev devFS, id deviceID) (string, error) {
	data, err := dev.readUevent(id)
	if err != nil {
		return "", err
	}

	var names []string

	scanner := bufio.NewScanner(bytes.NewReader(data))
	for scanner.Scan() {
		if value, ok := strings.CutPrefix(scanner.Text(), "DEVNAME="); ok {
			names = append(names, value)
		}
	}

	if len(names) != 1 {
		return "", fmt.Errorf("%w: %d DEVNAME lines", errBadDevname, len(names))
	}

	name := names[0]
	if name == "" || filepath.IsAbs(name) || filepath.Clean(name) != name ||
		name == ".." || strings.HasPrefix(name, "../") {
		return "", fmt.Errorf("%w: %q", errBadDevname, name)
	}

	return devRoot + "/" + name, nil
}

// evaluateIdentity applies policy to one candidate. Deny globs are checked
// on every name the device was found by: any match is a deny vote. The
// allow lists must pass for the canonical and the resolved name; an alias
// missing them is not a vote either way (a by-path or by-id link is not
// what authorizes a device).
func evaluateIdentity(
	alias, resolved, canonical string,
	gpol policy.Global,
	cpol policy.Container,
) decision {
	for _, name := range []string{alias, resolved, canonical} {
		if gpol.Denied(cpol, name) {
			return decision{deniedBy: name}
		}
	}

	return decision{authorized: gpol.Authorized(cpol, canonical) && gpol.Authorized(cpol, resolved)}
}

// unresolvedErr names a candidate whose identity could not be established.
func unresolvedErr(alias, resolved string, id *deviceID, cause error) error {
	switch {
	case id != nil:
		return fmt.Errorf("%w: %s (resolved %s, %s): %w", errUnresolved, alias, resolved, id, cause)
	default:
		return fmt.Errorf("%w: %s: %w", errUnresolved, alias, cause)
	}
}

// skipUnresolvable logs a dangling name: a WARN when an explicit allow glob
// names it, since the operator expected a device there.
func skipUnresolvable(alias string, gpol policy.Global, cpol policy.Container) {
	if gpol.ExplicitlyAllowed(cpol, alias) {
		logger.L().Warn("device symlink matches allow policy but cannot be resolved", "path", alias)

		return
	}

	logger.L().Debug("unresolvable symlink skipped", "path", alias)
}

// deviceVotes aggregates every candidate that resolved to one device.
type deviceVotes struct {
	id         deviceID
	candidates int
	authorized bool
	deniedBy   string
}

// aggregation collects candidates per device across all mounts of a
// container, so one name's deny vote holds against every other name of the
// same device.
type aggregation struct {
	order []deviceID
	votes map[deviceID]*deviceVotes
}

func (a *aggregation) add(found *candidate) {
	if a.votes == nil {
		a.votes = make(map[deviceID]*deviceVotes)
	}

	votes, ok := a.votes[found.id]
	if !ok {
		votes = &deviceVotes{id: found.id}
		a.votes[found.id] = votes
		a.order = append(a.order, found.id)
	}

	votes.candidates++

	if found.decision.deniedBy != "" && votes.deniedBy == "" {
		votes.deniedBy = found.decision.deniedBy
	}

	if found.decision.authorized {
		votes.authorized = true
	}
}

// emit returns the rules for every device that is authorized and has no
// deny vote, in the order devices were first seen, and how many candidates
// were not granted.
func (a *aggregation) emit(containerID string) (rules []cgroup.DeviceRule, skipped int) {
	for _, device := range a.order {
		votes := a.votes[device]

		switch {
		case votes.deniedBy != "" && votes.authorized:
			logger.L().Info("device denied by policy under one of its names",
				"id", containerID, "device", device.String(), "denied_by", votes.deniedBy)

			skipped += votes.candidates
		case votes.deniedBy != "":
			logger.L().Debug("device excluded by policy",
				"id", containerID, "device", device.String(), "denied_by", votes.deniedBy)

			skipped += votes.candidates
		case !votes.authorized:
			logger.L().Debug("device not in the allow list",
				"id", containerID, "device", device.String())

			skipped += votes.candidates
		default:
			major, minor := device.major, device.minor
			rules = append(rules, cgroup.DeviceRule{
				Allow:  true,
				Access: deviceAccessAll,
				Type:   device.typ,
				Major:  &major,
				Minor:  &minor,
			})
		}
	}

	return rules, skipped
}
