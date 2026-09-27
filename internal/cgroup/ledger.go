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

import "sync"

// Ledger records, per cgroup v1 directory, the device permission bits this
// daemon itself added to devices.allow. devices.list cannot tell a runtime
// exception from one of ours, so without the ledger a later narrowing could
// neither find what to revoke nor avoid revoking the runtime's own baseline.
//
// One Ledger lives as long as the Processor that owns it and is passed to
// every cgroupv1 instance (cgroup.New builds a fresh one per call). Entries
// are keyed by Identity, so a cgroup recreated at the same path starts with
// nothing owned. The zero value is ready to use.
type Ledger struct {
	mu      sync.Mutex
	entries map[Identity]map[devBit]struct{}
}

// owned returns a copy of the bits recorded for id.
func (l *Ledger) owned(id Identity) map[devBit]struct{} {
	l.mu.Lock()
	defer l.mu.Unlock()

	out := make(map[devBit]struct{}, len(l.entries[id]))
	for bit := range l.entries[id] {
		out[bit] = struct{}{}
	}

	return out
}

func (l *Ledger) add(id Identity, bit devBit) {
	l.mu.Lock()
	defer l.mu.Unlock()

	if l.entries == nil {
		l.entries = make(map[Identity]map[devBit]struct{})
	}

	if l.entries[id] == nil {
		l.entries[id] = make(map[devBit]struct{})
	}

	l.entries[id][bit] = struct{}{}
}

func (l *Ledger) remove(id Identity, bit devBit) {
	l.mu.Lock()
	defer l.mu.Unlock()

	delete(l.entries[id], bit)

	if len(l.entries[id]) == 0 {
		delete(l.entries, id)
	}
}
