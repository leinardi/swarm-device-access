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

// Package systemd watches systemd's DBus Reloading signal so the daemon can
// re-apply cgroup device rules after a daemon-reload clears them.
package systemd

import (
	"context"
	"errors"
	"fmt"
	"sync"
	"time"

	"github.com/godbus/dbus/v5"

	"github.com/leinardi/swarm-device-access/internal/logger"
)

const (
	systemdBusName     = "org.freedesktop.systemd1"
	systemdObjectPath  = dbus.ObjectPath("/org/freedesktop/systemd1")
	reloadingInterface = "org.freedesktop.systemd1.Manager"
	reloadingMember    = "Reloading"
	reloadingFullName  = reloadingInterface + "." + reloadingMember

	busDaemonName            = "org.freedesktop.DBus"
	busDaemonPath            = dbus.ObjectPath("/org/freedesktop/DBus")
	nameOwnerChangedMember   = "NameOwnerChanged"
	nameOwnerChangedFullName = busDaemonName + "." + nameOwnerChangedMember

	signalChanBuffer = 16

	// setupTimeout bounds one attempt to connect and subscribe: none of the
	// DBus calls involved take a deadline of their own.
	setupTimeout = 10 * time.Second
	// minRetry and maxRetry bound the pause between subscription attempts.
	minRetry = time.Second
	maxRetry = 5 * time.Minute
)

// errSetupTimeout reports a subscription attempt that did not finish within
// setupTimeout.
var errSetupTimeout = errors.New("connecting to the system bus timed out")

// Watcher holds a DBus connection subscribed to systemd's Reloading signal.
// systemctl daemon-reload clears cgroup BPF programs, so any container that
// had device-allow rules attached loses access. Watch invokes the callback
// after each reload completes so the caller can re-apply rules.
type Watcher struct {
	conn *dbus.Conn
	// sigCh is registered before owner is resolved, so an owner change
	// that races the lookup is queued rather than dropped.
	sigCh chan *dbus.Signal
	// owner is the unique bus name systemd held when Open resolved it.
	owner string
}

// Supervise keeps a Watcher subscribed until ctx is done, invoking onReload
// on each completed systemd reload. It never blocks its caller's other work
// on DBus: run it on its own goroutine. Each attempt to connect and
// subscribe is bounded by setupTimeout; a failed attempt, or a connection
// that is lost later (a DBus restart), is retried with backoff up to
// maxRetry.
//
// onSubscribed runs after every successful subscription, the first one
// included: a reload that completed while no watcher was subscribed (during
// startup, or while the bus was unreachable) was not seen, so the caller
// must reconcile everything once.
func Supervise(ctx context.Context, onReload, onSubscribed func()) {
	sup := supervisor{
		open:         Open,
		setupTimeout: setupTimeout,
		minRetry:     minRetry,
		maxRetry:     maxRetry,
	}
	sup.run(ctx, onReload, onSubscribed)
}

// supervisor is Supervise with its seams: tests replace open and shorten
// the timings.
type supervisor struct {
	open         func() (*Watcher, error)
	setupTimeout time.Duration
	minRetry     time.Duration
	maxRetry     time.Duration
}

func (s *supervisor) run(ctx context.Context, onReload, onSubscribed func()) {
	retry := s.minRetry
	failures := 0

	for {
		watcher, err := s.openBounded(ctx)
		if ctx.Err() != nil {
			closeWatcher(watcher)

			return
		}

		if err != nil {
			// Hosts without systemd or without the socket mounted fail
			// every attempt; say so once, then keep trying quietly.
			if failures == 0 {
				logger.L().Warn("systemd reload handling unavailable; retrying in the background",
					"err", err, "retry_in", retry)
			} else {
				logger.L().Debug("systemd reload watcher still unavailable",
					"err", err, "retry_in", retry)
			}

			failures++

			if !sleepCtx(ctx, retry) {
				return
			}

			retry = min(retry*2, s.maxRetry)

			continue
		}

		if failures > 0 {
			logger.L().Info("systemd reload handling available again")
		}

		failures = 0
		subscribedAt := time.Now()

		// Signals are buffered from here on, so none is lost while the
		// caller reconciles.
		onSubscribed()
		watcher.Watch(ctx, onReload)
		closeWatcher(watcher)

		if ctx.Err() != nil {
			return
		}

		// A subscription that held for a while starts the backoff over;
		// one that keeps dropping right away backs off like a failure, so
		// a flapping bus does not request a pass every minRetry.
		if time.Since(subscribedAt) >= s.maxRetry {
			retry = s.minRetry
		}

		logger.L().Warn("systemd reload watcher disconnected; resubscribing", "retry_in", retry)

		if !sleepCtx(ctx, retry) {
			return
		}

		retry = min(retry*2, s.maxRetry)
	}
}

// openBounded runs open, giving up after setupTimeout or when ctx is done.
// An attempt given up on keeps running in the background and closes its
// watcher if it ever succeeds, so nothing leaks a subscription.
func (s *supervisor) openBounded(ctx context.Context) (*Watcher, error) {
	type attempt struct {
		watcher *Watcher
		err     error
	}

	result := make(chan attempt, 1)

	go func() {
		watcher, err := s.open()
		result <- attempt{watcher, err}
	}()

	abandon := func() {
		go func() { closeWatcher((<-result).watcher) }()
	}

	timer := time.NewTimer(s.setupTimeout)
	defer timer.Stop()

	select {
	case got := <-result:
		return got.watcher, got.err
	case <-timer.C:
		abandon()

		return nil, errSetupTimeout
	case <-ctx.Done():
		abandon()

		return nil, fmt.Errorf("subscribe to systemd reloads: %w", ctx.Err())
	}
}

func closeWatcher(watcher *Watcher) {
	if watcher == nil {
		return
	}

	err := watcher.Close()
	if err != nil {
		logger.L().Warn("close systemd watcher", "err", err)
	}
}

// sleepCtx waits for duration and reports whether ctx is still live.
func sleepCtx(ctx context.Context, duration time.Duration) bool {
	timer := time.NewTimer(duration)
	defer timer.Stop()

	select {
	case <-ctx.Done():
		return false
	case <-timer.C:
		return true
	}
}

// Open connects to the system DBus, resolves the unique name that owns
// org.freedesktop.systemd1, and registers signal matches for systemd's
// Reloading signal (from systemd's name and object path only) and for
// changes of that name's owner. Returns an error if DBus is unavailable (no
// socket bind-mount, no systemd, daemon running off-host). Supervise
// retries it; the daemon runs without reload handling meanwhile.
func Open() (*Watcher, error) {
	conn, err := dbus.ConnectSystemBus()
	if err != nil {
		return nil, fmt.Errorf("connect system bus: %w", err)
	}

	watcher, err := subscribe(conn)
	if err != nil {
		closeErr := conn.Close()
		if closeErr != nil {
			logger.L().Warn("close dbus conn after subscription failure", "err", closeErr)
		}

		return nil, err
	}

	return watcher, nil
}

// subscribe adds the matches and registers the signal channel before it
// resolves systemd's owner: godbus drops signals no channel is registered
// for, so a NameOwnerChanged between the lookup and the registration would
// otherwise be lost and leave the watcher trusting a dead name.
func subscribe(conn *dbus.Conn) (*Watcher, error) {
	err := conn.AddMatchSignal(
		dbus.WithMatchSender(systemdBusName),
		dbus.WithMatchObjectPath(systemdObjectPath),
		dbus.WithMatchInterface(reloadingInterface),
		dbus.WithMatchMember(reloadingMember),
	)
	if err != nil {
		return nil, fmt.Errorf("add match signal %s: %w", reloadingFullName, err)
	}

	err = conn.AddMatchSignal(
		dbus.WithMatchSender(busDaemonName),
		dbus.WithMatchObjectPath(busDaemonPath),
		dbus.WithMatchInterface(busDaemonName),
		dbus.WithMatchMember(nameOwnerChangedMember),
		dbus.WithMatchArg(0, systemdBusName),
	)
	if err != nil {
		return nil, fmt.Errorf("add match signal %s: %w", nameOwnerChangedFullName, err)
	}

	sigCh := make(chan *dbus.Signal, signalChanBuffer)
	conn.Signal(sigCh)

	var owner string

	err = conn.BusObject().Call(busDaemonName+".GetNameOwner", 0, systemdBusName).Store(&owner)
	if err != nil {
		conn.RemoveSignal(sigCh)

		return nil, fmt.Errorf("resolve the owner of %s: %w", systemdBusName, err)
	}

	return &Watcher{conn: conn, sigCh: sigCh, owner: owner}, nil
}

// Close releases the DBus connection.
func (w *Watcher) Close() error {
	if w == nil || w.conn == nil {
		return nil
	}

	w.conn.RemoveSignal(w.sigCh)

	err := w.conn.Close()
	if err != nil {
		return fmt.Errorf("close dbus conn: %w", err)
	}

	return nil
}

// Watch blocks until ctx is canceled, invoking onReload on each completed
// systemd reload (see watch).
func (w *Watcher) Watch(ctx context.Context, onReload func()) {
	watch(ctx, w.sigCh, w.owner, onReload)
}

// watch consumes signals until ctx is canceled or sigCh closes. The
// Reloading signal fires twice per reload: once with active=true when
// reload starts and once with active=false when it finishes. Only the
// completion edge counts (re-applying mid-reload races the cgroup wipe),
// and only from systemd: the signal must come from owner, the unique name
// of org.freedesktop.systemd1, and from systemd's object path. Any other
// client on the bus can emit a signal with the same name, so the match
// rules alone are not trusted. owner follows NameOwnerChanged from the bus
// daemon, so a systemd re-exec does not silence the watcher.
//
// onReload runs on its own goroutine and is coalesced: a reload that
// completes while it runs is absorbed into one more run after it
// finishes. watch returns only after the last run has finished.
func watch(ctx context.Context, sigCh <-chan *dbus.Signal, owner string, onReload func()) {
	runs := &coalescer{run: onReload}

	defer runs.wait()

	logger.L().Debug("systemd reload watcher started", "systemd", owner)

	for {
		select {
		case <-ctx.Done():
			return

		case sig, ok := <-sigCh:
			if !ok {
				logger.L().Debug("systemd signal channel closed")

				return
			}

			if newOwner, changed := ownerChange(sig); changed {
				logger.L().Debug("systemd bus name changed owner", "old", owner, "new", newOwner)
				owner = newOwner

				continue
			}

			if !fromSystemd(sig, owner) || !isReloadCompleted(sig) {
				continue
			}

			logger.L().Info("systemd reload completed; re-applying device rules")
			runs.trigger()
		}
	}
}

// fromSystemd reports whether sig was sent by owner from systemd's object
// path. An empty owner (systemd not on the bus) matches nothing.
func fromSystemd(sig *dbus.Signal, owner string) bool {
	return sig != nil && owner != "" && sig.Sender == owner && sig.Path == systemdObjectPath
}

// ownerChange returns the new owner of org.freedesktop.systemd1 when sig is
// the bus daemon's NameOwnerChanged for it.
func ownerChange(sig *dbus.Signal) (string, bool) {
	if sig == nil || sig.Name != nameOwnerChangedFullName ||
		sig.Sender != busDaemonName || sig.Path != busDaemonPath || len(sig.Body) != 3 {
		return "", false
	}

	name, nameOK := sig.Body[0].(string)
	newOwner, ownerOK := sig.Body[2].(string)

	if !nameOK || !ownerOK || name != systemdBusName {
		return "", false
	}

	return newOwner, true
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

// coalescer runs a function on its own goroutine, one run at a time:
// triggers that arrive during a run collapse into one run after it.
type coalescer struct {
	run func()

	mu      sync.Mutex
	running bool
	pending bool
	done    sync.WaitGroup
}

func (c *coalescer) trigger() {
	c.mu.Lock()
	defer c.mu.Unlock()

	if c.running {
		c.pending = true

		return
	}

	c.running = true

	c.done.Go(c.loop)
}

func (c *coalescer) loop() {
	for {
		c.run()

		c.mu.Lock()

		if !c.pending {
			c.running = false
			c.mu.Unlock()

			return
		}

		c.pending = false
		c.mu.Unlock()
	}
}

// wait blocks until no run is in progress.
func (c *coalescer) wait() {
	c.done.Wait()
}
