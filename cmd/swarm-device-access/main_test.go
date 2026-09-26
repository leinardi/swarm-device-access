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

package main

import (
	"context"
	"fmt"
	"testing"
	"time"

	"github.com/moby/moby/client"
)

// hungInfo blocks Info until its context is done, like a wedged dockerd.
type hungInfo struct{}

func (hungInfo) Info(ctx context.Context, _ client.InfoOptions) (client.SystemInfoResult, error) {
	<-ctx.Done()

	return client.SystemInfoResult{}, fmt.Errorf("hung info: %w", ctx.Err())
}

func TestDetectSwarmManager_HungInfoIsBounded(t *testing.T) {
	const timeout = 50 * time.Millisecond

	done := make(chan bool, 1)

	go func() { done <- detectSwarmManager(context.Background(), hungInfo{}, timeout) }()

	select {
	case manager := <-done:
		if manager {
			t.Error("a timed-out Info must degrade to worker, got manager")
		}
	case <-time.After(20 * timeout):
		t.Fatal("detectSwarmManager did not return: Info is not bounded")
	}
}
