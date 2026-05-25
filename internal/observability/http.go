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
	"net/http"
	"net/http/pprof"
	"sync/atomic"
	"time"

	"github.com/prometheus/client_golang/prometheus/promhttp"

	"github.com/leinardi/swarm-device-access/internal/logger"
)

const shutdownTimeout = 5 * time.Second

// ready tracks whether the daemon is subscribed to the Docker event stream.
var ready atomic.Bool

// SetReady updates the daemon readiness state reflected by the /readyz endpoint.
func SetReady(val bool) { ready.Store(val) }

// StartMetricsServer starts an HTTP server on addr exposing:
//
//	/metrics  — Prometheus text format (default registry)
//	/healthz  — 200 OK always (liveness)
//	/readyz   — 200 OK once subscribed to Docker events (readiness)
func StartMetricsServer(ctx context.Context, addr string) {
	log := logger.L()
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

	srv := &http.Server{
		Addr:              addr,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		log.Info("metrics server listening", "addr", addr)

		err := srv.ListenAndServe()
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("metrics server error", "err", err)
		}
	}()

	go func() {
		<-ctx.Done()

		shutCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), shutdownTimeout)
		defer cancel()

		err := srv.Shutdown(shutCtx)
		if err != nil {
			log.Warn("metrics server shutdown error", "err", err)
		}
	}()
}

// StartDebugServer starts an HTTP server on addr exposing pprof endpoints.
func StartDebugServer(ctx context.Context, addr string) {
	log := logger.L()
	mux := http.NewServeMux()
	mux.HandleFunc("/debug/pprof/", pprof.Index)
	mux.HandleFunc("/debug/pprof/cmdline", pprof.Cmdline)
	mux.HandleFunc("/debug/pprof/profile", pprof.Profile)
	mux.HandleFunc("/debug/pprof/symbol", pprof.Symbol)
	mux.HandleFunc("/debug/pprof/trace", pprof.Trace)

	srv := &http.Server{
		Addr:              addr,
		Handler:           mux,
		ReadHeaderTimeout: 5 * time.Second,
	}

	go func() {
		log.Info("debug server listening", "addr", addr)

		err := srv.ListenAndServe()
		if err != nil && !errors.Is(err, http.ErrServerClosed) {
			log.Error("debug server error", "err", err)
		}
	}()

	go func() {
		<-ctx.Done()

		shutCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), shutdownTimeout)
		defer cancel()

		err := srv.Shutdown(shutCtx)
		if err != nil {
			log.Warn("debug server shutdown error", "err", err)
		}
	}()
}
