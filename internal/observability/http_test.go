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

package observability

import (
	"context"
	"io"
	"net"
	"net/http"
	"strings"
	"testing"
	"time"
)

// serveOnLoopback runs srv on a fresh loopback port and returns its base URL
// and stop func.
func serveOnLoopback(t *testing.T, name string, srv *http.Server) (baseURL string, stop func()) {
	t.Helper()

	ln, err := listenTCP(t, "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}

	stop = serve(context.Background(), name, ln, srv)
	t.Cleanup(stop)

	return "http://" + ln.Addr().String(), stop
}

func get(t *testing.T, url string, timeout time.Duration) (status int, body string) {
	t.Helper()

	ctx, cancel := context.WithTimeout(context.Background(), timeout)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodGet, url, http.NoBody)
	if err != nil {
		t.Fatal(err)
	}

	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatalf("GET %s: %v", url, err)
	}
	defer resp.Body.Close()

	data, err := io.ReadAll(resp.Body)
	if err != nil {
		t.Fatalf("read %s: %v", url, err)
	}

	return resp.StatusCode, string(data)
}

func TestStartServers_BusyAddressIsAnError(t *testing.T) {
	t.Parallel()

	busy, err := listenTCP(t, "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(func() { _ = busy.Close() })

	for name, start := range map[string]func(context.Context, string) (func(), error){
		"metrics": StartMetricsServer,
		"debug":   StartDebugServer,
	} {
		stop, err := start(context.Background(), busy.Addr().String())
		if err == nil {
			stop()
			t.Errorf("%s: started on a busy address", name)
		}
	}
}

// Not parallel: readiness is package state.
func TestMetricsServer_Endpoints(t *testing.T) {
	baseURL, _ := serveOnLoopback(t, "metrics", newMetricsServer())

	if status, body := get(
		t,
		baseURL+"/healthz",
		5*time.Second,
	); status != http.StatusOK ||
		body != "ok" {
		t.Errorf("/healthz = %d %q, want 200 ok", status, body)
	}

	SetReady(false)

	if status, _ := get(
		t,
		baseURL+"/readyz",
		5*time.Second,
	); status != http.StatusServiceUnavailable {
		t.Errorf("/readyz before ready = %d, want 503", status)
	}

	SetReady(true)
	t.Cleanup(func() { SetReady(false) })

	if status, _ := get(t, baseURL+"/readyz", 5*time.Second); status != http.StatusOK {
		t.Errorf("/readyz when ready = %d, want 200", status)
	}

	if status, body := get(t, baseURL+"/metrics", 5*time.Second); status != http.StatusOK ||
		!strings.Contains(body, "go_goroutines") {
		t.Errorf("/metrics = %d without go_goroutines", status)
	}
}

func TestServe_StopReturnsAndClosesTheListener(t *testing.T) {
	t.Parallel()

	baseURL, stop := serveOnLoopback(t, "metrics", newMetricsServer())

	done := make(chan struct{})

	go func() {
		defer close(done)

		stop()
		stop()
	}()

	select {
	case <-done:
	case <-time.After(2 * shutdownTimeout):
		t.Fatal("stop did not return")
	}

	req, err := http.NewRequestWithContext(
		context.Background(),
		http.MethodGet,
		baseURL+"/healthz",
		http.NoBody,
	)
	if err != nil {
		t.Fatal(err)
	}

	resp, err := http.DefaultClient.Do(req)
	if err == nil {
		_ = resp.Body.Close()

		t.Error("server still answers after stop")
	}
}

func TestServe_ContextCancelStops(t *testing.T) {
	t.Parallel()

	ln, err := listenTCP(t, "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}

	ctx, cancel := context.WithCancel(context.Background())
	stop := serve(ctx, "metrics", ln, newMetricsServer())

	cancel()

	done := make(chan struct{})

	go func() {
		defer close(done)

		stop()
	}()

	select {
	case <-done:
	case <-time.After(2 * shutdownTimeout):
		t.Fatal("stop after cancel did not return")
	}
}

// The debug server has no write timeout, so a CPU profile streams to the
// end instead of being cut off or refused.
func TestDebugServer_ProfileCompletes(t *testing.T) {
	t.Parallel()

	baseURL, _ := serveOnLoopback(t, "debug", newDebugServer())

	status, body := get(t, baseURL+"/debug/pprof/profile?seconds=1", 10*time.Second)
	if status != http.StatusOK || body == "" {
		t.Errorf("profile = %d with %d bytes, want 200 and a profile", status, len(body))
	}
}

// listenTCP binds addr for a test.
func listenTCP(t *testing.T, addr string) (net.Listener, error) {
	t.Helper()

	var config net.ListenConfig

	return config.Listen(context.Background(), "tcp", addr) //nolint:wrapcheck // test helper
}
