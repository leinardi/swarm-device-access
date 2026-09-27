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
	"slices"
	"sync"
	"time"

	"github.com/leinardi/swarm-device-access/internal/cgroup"
)

// LifecycleState is how much is known about one run of a container.
type LifecycleState int

const (
	// LifecycleProvisional: an inspect failed; only the ID and the time of
	// the observation are known.
	LifecycleProvisional LifecycleState = iota
	// LifecycleVerified: the start time and the cgroup identity were
	// verified against the pinned process.
	LifecycleVerified
	// LifecycleTerminal: the run ended (die, destroy, or seen not running).
	LifecycleTerminal
)

func (s LifecycleState) String() string {
	switch s {
	case LifecycleProvisional:
		return "provisional"
	case LifecycleVerified:
		return "verified"
	case LifecycleTerminal:
		return "terminal"
	default:
		return "unknown"
	}
}

// maxRecordsPerContainer bounds one container's history; the oldest
// records without an identity go first.
const maxRecordsPerContainer = 8

// LifecycleRecord is one run of a container.
type LifecycleRecord struct {
	ContainerID string
	// StartedAt is Docker's State.StartedAt, empty while unknown.
	StartedAt string
	// Identity is the cgroup the run was verified in; zero while unknown.
	Identity   cgroup.Identity
	Version    int
	Privileged bool
	State      LifecycleState
	FirstSeen  time.Time
	// Observed is the last time an inspect failure refreshed the record.
	Observed time.Time
	// Revoked is set once the empty set was applied at Identity after the
	// run ended; the record is kept until the cgroup is verified gone.
	Revoked bool
}

// HasIdentity reports whether the record names a cgroup.
func (r *LifecycleRecord) HasIdentity() bool {
	return r.Identity.Path != ""
}

// started parses StartedAt; ok is false when it is unknown.
func (r *LifecycleRecord) started() (time.Time, bool) {
	if r.StartedAt == "" {
		return time.Time{}, false
	}

	parsed, err := time.Parse(time.RFC3339Nano, r.StartedAt)
	if err != nil {
		return time.Time{}, false
	}

	return parsed, true
}

// Lifecycles is the per-container history of runs. A restart under the same
// ID adds a record rather than replacing one, so cleanup that arrives late
// for an old run never reaches the new one. Records are released only once
// their cgroup is verified gone; an inspect that cannot find the container
// never removes one. The zero value is ready to use.
type Lifecycles struct {
	mu   sync.Mutex
	byID map[string][]*LifecycleRecord
	now  func() time.Time
}

// Records returns copies of the container's records, oldest first.
func (l *Lifecycles) Records(containerID string) []LifecycleRecord {
	l.mu.Lock()
	defer l.mu.Unlock()

	out := make([]LifecycleRecord, 0, len(l.byID[containerID]))
	for _, rec := range l.byID[containerID] {
		out = append(out, *rec)
	}

	return out
}

func (l *Lifecycles) clock() time.Time {
	if l.now == nil {
		return time.Now()
	}

	return l.now()
}

// all returns copies of every record.
func (l *Lifecycles) all() []LifecycleRecord {
	l.mu.Lock()
	defer l.mu.Unlock()

	var out []LifecycleRecord

	for _, recs := range l.byID {
		for _, rec := range recs {
			out = append(out, *rec)
		}
	}

	return out
}

// provisional creates or refreshes the provisional record of a container an
// inspect could not report on.
func (l *Lifecycles) provisional(containerID string) {
	l.mu.Lock()
	defer l.mu.Unlock()

	now := l.clock()

	for _, rec := range l.byID[containerID] {
		if rec.State == LifecycleProvisional {
			rec.Observed = now

			return
		}
	}

	l.appendLocked(&LifecycleRecord{
		ContainerID: containerID,
		State:       LifecycleProvisional,
		FirstSeen:   now,
		Observed:    now,
	})
}

// verify records the cgroup a run was verified in. It upgrades the record
// of the same run, else a provisional record, else adds a record.
func (l *Lifecycles) verify(verified *LifecycleRecord) {
	l.mu.Lock()
	defer l.mu.Unlock()

	recs := l.byID[verified.ContainerID]

	idx := slices.IndexFunc(recs, func(rec *LifecycleRecord) bool {
		return rec.StartedAt == verified.StartedAt
	})
	if idx < 0 {
		idx = slices.IndexFunc(recs, func(rec *LifecycleRecord) bool {
			return rec.State == LifecycleProvisional
		})
	}

	if idx < 0 {
		added := *verified
		added.State = LifecycleVerified
		added.FirstSeen = l.clock()
		l.appendLocked(&added)

		return
	}

	rec := recs[idx]
	rec.StartedAt = verified.StartedAt
	rec.Identity = verified.Identity
	rec.Version = verified.Version
	rec.Privileged = verified.Privileged
	rec.Revoked = false

	if rec.State != LifecycleTerminal {
		rec.State = LifecycleVerified
	}
}

// lookup returns the record of one run.
func (l *Lifecycles) lookup(containerID, startedAt string) (LifecycleRecord, bool) {
	l.mu.Lock()
	defer l.mu.Unlock()

	for _, rec := range l.byID[containerID] {
		if rec.StartedAt == startedAt {
			return *rec, true
		}
	}

	return LifecycleRecord{}, false
}

// terminate resolves an event that ended a run at eventTime (die,
// destroy). It marks terminal the verified record with the latest start
// time not after eventTime and returns it; with no such record, every
// provisional record becomes terminal and is returned instead.
func (l *Lifecycles) terminate(containerID string, eventTime time.Time) []LifecycleRecord {
	l.mu.Lock()
	defer l.mu.Unlock()

	var (
		match       *LifecycleRecord
		matchStart  time.Time
		provisional []*LifecycleRecord
	)

	for _, rec := range l.byID[containerID] {
		if rec.State == LifecycleProvisional {
			provisional = append(provisional, rec)

			continue
		}

		started, ok := rec.started()
		if !ok || !rec.HasIdentity() || started.After(eventTime) {
			continue
		}

		if match == nil || started.After(matchStart) {
			match, matchStart = rec, started
		}
	}

	if match != nil {
		match.State = LifecycleTerminal

		return []LifecycleRecord{*match}
	}

	out := make([]LifecycleRecord, 0, len(provisional))
	for _, rec := range provisional {
		rec.State = LifecycleTerminal
		out = append(out, *rec)
	}

	return out
}

// markTerminal marks one run ended.
func (l *Lifecycles) markTerminal(containerID, startedAt string) {
	l.update(containerID, startedAt, func(rec *LifecycleRecord) { rec.State = LifecycleTerminal })
}

// markRevoked records that the empty set was applied for an ended run.
func (l *Lifecycles) markRevoked(containerID, startedAt string) {
	l.update(containerID, startedAt, func(rec *LifecycleRecord) {
		if rec.State == LifecycleTerminal {
			rec.Revoked = true
		}
	})
}

func (l *Lifecycles) update(containerID, startedAt string, change func(*LifecycleRecord)) {
	l.mu.Lock()
	defer l.mu.Unlock()

	for _, rec := range l.byID[containerID] {
		if rec.StartedAt == startedAt {
			change(rec)
		}
	}
}

// release removes the record released is a copy of. Records are told apart
// by start time and creation time: records that never learned their start
// time all share an empty StartedAt.
func (l *Lifecycles) release(released *LifecycleRecord) {
	l.mu.Lock()
	defer l.mu.Unlock()

	containerID := released.ContainerID

	recs := slices.DeleteFunc(l.byID[containerID], func(rec *LifecycleRecord) bool {
		return rec.StartedAt == released.StartedAt && rec.FirstSeen.Equal(released.FirstSeen)
	})
	if len(recs) == 0 {
		delete(l.byID, containerID)

		return
	}

	l.byID[containerID] = recs
}

func (l *Lifecycles) appendLocked(rec *LifecycleRecord) {
	if l.byID == nil {
		l.byID = make(map[string][]*LifecycleRecord)
	}

	l.byID[rec.ContainerID] = append(l.byID[rec.ContainerID], rec)
	recs := l.byID[rec.ContainerID]

	// Over the bound, drop the oldest records that hold no identity: they
	// name no cgroup, so nothing can be left to revoke through them.
	for len(recs) > maxRecordsPerContainer {
		idx := slices.IndexFunc(recs, func(r *LifecycleRecord) bool { return !r.HasIdentity() })
		if idx < 0 {
			break
		}

		recs = slices.Delete(recs, idx, idx+1)
	}

	l.byID[rec.ContainerID] = recs
}
