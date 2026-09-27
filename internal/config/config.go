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

// Package config holds the hot-reloadable daemon configuration snapshot and
// its atomic store.
package config

import (
	"sync"
	"sync/atomic"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

// Runtime is the hot-reloadable daemon configuration snapshot.
// Written once at startup and again on each SIGHUP.
type Runtime struct {
	DryRun bool
	Policy policy.Global
}

// snapshot is one published Runtime and its generation.
type snapshot struct {
	runtime    Runtime
	generation uint64
}

// Store provides atomic read access to the current Runtime. Only the
// Publisher returned with it can replace the Runtime, so whoever holds the
// Publisher (the processor, after startup) is the one path that publishes.
type Store struct {
	current atomic.Pointer[snapshot]
}

// Publisher replaces the Runtime of the Store it was created with.
type Publisher struct {
	store *Store
	// mu orders publications, so generations are handed out in the order
	// the snapshots become visible.
	mu sync.Mutex
}

// NewStore returns a Store holding initial as generation 1, and the
// Publisher for it.
func NewStore(initial Runtime) (*Store, *Publisher) {
	store := &Store{}
	store.current.Store(&snapshot{runtime: initial, generation: 1})

	return store, &Publisher{store: store}
}

// Load returns the current Runtime.
func (s *Store) Load() Runtime {
	return s.current.Load().runtime
}

// Snapshot returns the current Runtime and its generation.
func (s *Store) Snapshot() (rt Runtime, generation uint64) {
	current := s.current.Load()

	return current.runtime, current.generation
}

// Generation returns the generation of the current Runtime. It starts at 1
// and grows by one with every publication.
func (s *Store) Generation() uint64 {
	return s.current.Load().generation
}

// Publish atomically replaces the Runtime and returns its generation.
func (p *Publisher) Publish(rt Runtime) uint64 {
	p.mu.Lock()
	defer p.mu.Unlock()

	generation := p.store.Generation() + 1
	p.store.current.Store(&snapshot{runtime: rt, generation: generation})

	return generation
}
