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

// Package systemd watches systemd's DBus Reloading signal so the daemon can
// re-apply cgroup device rules after a daemon-reload clears them.
package systemd

import (
	"context"
	"fmt"

	"github.com/godbus/dbus/v5"
	"github.com/leinardi/device-mapping-manager/internal/logger"
)

const (
	reloadingInterface = "org.freedesktop.systemd1.Manager"
	reloadingMember    = "Reloading"
	reloadingFullName  = reloadingInterface + "." + reloadingMember

	signalChanBuffer = 16
)

// Watcher holds a DBus connection subscribed to systemd's Reloading signal.
// systemctl daemon-reload clears cgroup BPF programs, so any container that
// had device-allow rules attached loses access. Watch invokes the callback
// after each reload completes so the caller can re-apply rules.
type Watcher struct {
	conn *dbus.Conn
}

// Open connects to the system DBus and registers a signal match for
// org.freedesktop.systemd1.Manager.Reloading. Returns an error if DBus is
// unavailable (no socket bind-mount, no systemd, daemon running off-host).
// Callers should treat this as non-fatal and continue without reload handling.
func Open() (*Watcher, error) {
	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		return nil, fmt.Errorf("connect system bus: %w", err)
	}

	matchErr := conn.AddMatchSignal(
		dbus.WithMatchInterface(reloadingInterface),
		dbus.WithMatchMember(reloadingMember),
	)
	if matchErr != nil {
		closeErr := conn.Close()
		if closeErr != nil {
			logger.L().Warn("close dbus conn after AddMatchSignal failure", "err", closeErr)
		}

		return nil, fmt.Errorf("add match signal %s: %w", reloadingFullName, matchErr)
	}

	return &Watcher{conn: conn}, nil
}

// Close releases the DBus connection.
func (w *Watcher) Close() error {
	if w == nil || w.conn == nil {
		return nil
	}

	err := w.conn.Close()
	if err != nil {
		return fmt.Errorf("close dbus conn: %w", err)
	}

	return nil
}

// Watch blocks until ctx is canceled, invoking onReload on each completed
// systemd reload. The Reloading signal fires twice per reload: once with
// active=true when reload starts and once with active=false when it finishes.
// Only the completion edge triggers onReload — re-applying mid-reload races
// the cgroup wipe.
func (w *Watcher) Watch(ctx context.Context, onReload func()) {
	log := logger.L()
	sigCh := make(chan *dbus.Signal, signalChanBuffer)

	w.conn.Signal(sigCh)
	defer w.conn.RemoveSignal(sigCh)

	log.Debug("systemd reload watcher started")

	for {
		select {
		case <-ctx.Done():
			return

		case sig, ok := <-sigCh:
			if !ok {
				log.Warn("systemd signal channel closed; reload watcher exiting")

				return
			}

			if !isReloadCompleted(sig) {
				continue
			}

			log.Info("systemd reload completed; re-applying device rules")
			onReload()
		}
	}
}

// isReloadCompleted reports whether sig is a Reloading completion edge
// (signal name matches and body[0] == false). Returns false for the start
// edge, other signals, or malformed payloads.
func isReloadCompleted(sig *dbus.Signal) bool {
	if sig == nil || sig.Name != reloadingFullName || len(sig.Body) == 0 {
		return false
	}

	active, ok := sig.Body[0].(bool)
	if !ok {
		return false
	}

	return !active
}
