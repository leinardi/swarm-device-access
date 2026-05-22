//go:build linux

/*
 * Copyright 2026 Roberto Leinardi
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     http://www.apache.org/licenses/LICENSE-2.0
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
	"testing"
	"time"
)

func TestNextBackoff_Doubles(t *testing.T) {
	got := nextBackoff(1 * time.Second)
	if got != 2*time.Second {
		t.Errorf("nextBackoff(1s) = %v, want 2s", got)
	}
}

func TestNextBackoff_Caps(t *testing.T) {
	got := nextBackoff(maxBackoff)
	if got != maxBackoff {
		t.Errorf("nextBackoff(maxBackoff) = %v, want %v (capped)", got, maxBackoff)
	}

	got = nextBackoff(maxBackoff - 1*time.Second)
	if got != maxBackoff {
		t.Errorf("nextBackoff(maxBackoff-1s) = %v, want %v (capped)", got, maxBackoff)
	}
}

func TestPtr_ReturnsAddressOfValue(t *testing.T) {
	v := int64(42)
	p := ptr(v)

	if p == nil {
		t.Fatal("ptr returned nil")
	}

	if *p != 42 {
		t.Errorf("*ptr = %d, want 42", *p)
	}
}

func TestSleepCtx_RespectsCancellation(t *testing.T) {
	ctx, cancel := context.WithCancel(context.Background())
	cancel()

	start := time.Now()
	sleepCtx(ctx, 5*time.Second)

	elapsed := time.Since(start)
	if elapsed > 100*time.Millisecond {
		t.Errorf("sleepCtx took %v with cancelled ctx; want immediate return", elapsed)
	}
}

func TestSleepCtx_WaitsForDuration(t *testing.T) {
	ctx := context.Background()
	target := 50 * time.Millisecond

	start := time.Now()
	sleepCtx(ctx, target)

	elapsed := time.Since(start)
	if elapsed < target {
		t.Errorf("sleepCtx returned in %v; want at least %v", elapsed, target)
	}
}
