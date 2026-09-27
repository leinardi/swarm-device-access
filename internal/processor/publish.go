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
	"context"

	"github.com/leinardi/swarm-device-access/internal/config"
)

// PassRequester asks for a reconciliation pass over every running container
// under config generation (or a newer one). It must not block on the pass.
type PassRequester func(ctx context.Context, generation uint64)

// SetPassRequester installs the function PublishAndReconcile hands each new
// generation to (the daemon's reconciliation coordinator).
func (p *Processor) SetPassRequester(request PassRequester) {
	p.mu.Lock()
	defer p.mu.Unlock()

	p.requestPass = request
}

// PublishAndReconcile publishes rt as the new config and requests a pass
// that reconciles every running container under it. It is the only way the
// config changes after startup.
//
// The publication takes the same lock as Reconcile, so no container is
// between loading the config and mutating its cgroup when the generation
// changes: a worker that computed under the old config finishes first, and
// the pass then overwrites its result. The pass runs after the lock is
// released and takes it per container, so it cannot deadlock against this
// call.
func (p *Processor) PublishAndReconcile(ctx context.Context, rt config.Runtime) uint64 {
	p.mu.Lock()
	generation := p.Publisher.Publish(rt)
	request := p.requestPass
	p.mu.Unlock()

	if request != nil {
		request(ctx, generation)
	}

	return generation
}
