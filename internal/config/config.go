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
	"sync/atomic"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

// Runtime is the hot-reloadable daemon configuration snapshot.
// Written once at startup and again on each SIGHUP.
type Runtime struct {
	DryRun bool
	Policy policy.Global
}

// Store provides atomic read/write access to the current Runtime.
type Store struct {
	current atomic.Pointer[Runtime]
}

// NewStore returns an initialized Store with a zero-value Runtime.
// Call Set to populate it after parsing the initial configuration.
func NewStore() *Store {
	return &Store{}
}

// Load returns the current Runtime. Returns a zero-value Runtime if Set has
// not been called yet.
func (s *Store) Load() Runtime {
	ptr := s.current.Load()
	if ptr == nil {
		return Runtime{}
	}

	return *ptr
}

// Set atomically replaces the current Runtime.
func (s *Store) Set(rt Runtime) {
	s.current.Store(&rt)
}
