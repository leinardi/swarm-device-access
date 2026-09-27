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
	"net"
	"testing"
)

// freeAddr returns a loopback address nothing listens on.
func freeAddr(t *testing.T) string {
	t.Helper()

	ln, err := listenTCP(t, "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}

	addr := ln.Addr().String()

	err = ln.Close()
	if err != nil {
		t.Fatal(err)
	}

	return addr
}

// A busy debug address fails startup and releases the metrics address the
// first server had already bound.
func TestStartServers_SecondFailureStopsTheFirst(t *testing.T) {
	busy, err := listenTCP(t, "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(func() { _ = busy.Close() })

	effective := defaultSettings()
	effective.MetricsAddr = freeAddr(t)
	effective.DebugAddr = busy.Addr().String()

	stop, err := startServers(context.Background(), &effective)
	if err == nil {
		stop()
		t.Fatal("startServers accepted a busy debug address")
	}

	again, err := listenTCP(t, effective.MetricsAddr)
	if err != nil {
		t.Fatalf("metrics address still bound after the failed start: %v", err)
	}

	_ = again.Close()
}

func TestStartServers_NoneConfigured(t *testing.T) {
	effective := defaultSettings()

	stop, err := startServers(context.Background(), &effective)
	if err != nil {
		t.Fatal(err)
	}

	stop()
}

// listenTCP binds addr for a test.
func listenTCP(t *testing.T, addr string) (net.Listener, error) {
	t.Helper()

	var config net.ListenConfig

	return config.Listen(context.Background(), "tcp", addr) //nolint:wrapcheck // test helper
}
