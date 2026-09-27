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
	"errors"
	"fmt"
	"net"
	"net/http"
	"net/http/pprof"
	"sync"
	"sync/atomic"
	"time"

	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/leinardi/swarm-device-access/internal/logger"
)

const (
	shutdownTimeout   = 5 * time.Second
	readHeaderTimeout = 5 * time.Second
	readTimeout       = 10 * time.Second
	idleTimeout       = 60 * time.Second
	// metricsWriteTimeout bounds a scrape. The debug server has none: a
	// CPU profile or trace streams for as long as the caller asks.
	metricsWriteTimeout = 30 * time.Second
)

// ready tracks whether the daemon is subscribed to the Docker event stream.
var ready atomic.Bool

// SetReady updates the daemon readiness state reflected by the /readyz endpoint.
func SetReady(val bool) { ready.Store(val) }

// StartMetricsServer binds addr and serves, until ctx is done or stop is
// called:
//
//	/metrics  — Prometheus text format (default registry)
//	/healthz  — 200 OK always (liveness)
//	/readyz   — 200 OK once subscribed to Docker events (readiness)
//
// The address is bound before it returns, so a busy or invalid address is
// an error rather than a log line from a goroutine. stop shuts the server
// down and waits for it; it is safe to call more than once.
func StartMetricsServer(ctx context.Context, addr string) (stop func(), err error) {
	listener, err := listen(ctx, addr)
	if err != nil {
		return nil, fmt.Errorf("metrics server: %w", err)
	}

	return serve(ctx, "metrics", listener, newMetricsServer()), nil
}

// StartDebugServer binds addr and serves the pprof endpoints until ctx is
// done or stop is called (see StartMetricsServer). The server has no write
// timeout, because profiles and traces stream for as long as the caller
// asks, so it must stay on a loopback address.
func StartDebugServer(ctx context.Context, addr string) (stop func(), err error) {
	listener, err := listen(ctx, addr)
	if err != nil {
		return nil, fmt.Errorf("debug server: %w", err)
	}

	return serve(ctx, "debug", listener, newDebugServer()), nil
}

// listen binds addr now, so the caller learns about a busy address.
func listen(ctx context.Context, addr string) (net.Listener, error) {
	var config net.ListenConfig

	listener, err := config.Listen(ctx, "tcp", addr)
	if err != nil {
		return nil, fmt.Errorf("listen on %s: %w", addr, err)
	}

	return listener, nil
}

func newMetricsServer() *http.Server {
	mux := http.NewServeMux()
	mux.Handle("/metrics", promhttp.Handler())
	mux.HandleFunc("/healthz", func(resp http.ResponseWriter, _ *http.Request) {
		resp.WriteHeader(http.StatusOK)
		_, _ = resp.Write([]byte("ok"))
	})
	mux.HandleFunc("/readyz", func(resp http.ResponseWriter, _ *http.Request) {
		if ready.Load() {
			resp.WriteHeader(http.StatusOK)
			_, _ = resp.Write([]byte("ok"))
		} else {
			resp.WriteHeader(http.StatusServiceUnavailable)
			_, _ = resp.Write([]byte("not ready"))
		}
	})

	return &http.Server{
		Handler:           mux,
		ReadHeaderTimeout: readHeaderTimeout,
		ReadTimeout:       readTimeout,
		WriteTimeout:      metricsWriteTimeout,
		IdleTimeout:       idleTimeout,
	}
}

func newDebugServer() *http.Server {
	mux := http.NewServeMux()
	mux.HandleFunc("/debug/pprof/", pprof.Index)
	mux.HandleFunc("/debug/pprof/cmdline", pprof.Cmdline)
	mux.HandleFunc("/debug/pprof/profile", pprof.Profile)
	mux.HandleFunc("/debug/pprof/symbol", pprof.Symbol)
	mux.HandleFunc("/debug/pprof/trace", pprof.Trace)

	return &http.Server{
		Handler:           mux,
		ReadHeaderTimeout: readHeaderTimeout,
		ReadTimeout:       readTimeout,
		IdleTimeout:       idleTimeout,
	}
}

// serve runs srv on ln until ctx is done or the returned stop is called.
func serve(ctx context.Context, name string, listener net.Listener, srv *http.Server) func() {
	log := logger.L()
	served := make(chan struct{})

	log.Info(name+" server listening", "addr", listener.Addr().String())

	go func() {
		defer close(served)

		err := srv.Serve(listener)
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error(name+" server error", "err", err)
		}
	}()

	stop := sync.OnceFunc(func() {
		shutCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), shutdownTimeout)
		defer cancel()

		err := srv.Shutdown(shutCtx)
		if err != nil {
			log.Warn(name+" server shutdown error", "err", err)
		}

		<-served
	})

	go func() {
		select {
		case <-ctx.Done():
			stop()
		case <-served:
		}
	}()

	return stop
}
