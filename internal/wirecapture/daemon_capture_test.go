//go:build wirecapture

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

package wirecapture_test

import (
	"bufio"
	"context"
	"encoding/json"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"sync"
	"syscall"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"

	"github.com/leinardi/swarm-device-access/internal/wirecapture"
)

const (
	// binaryEnv names the Go daemon binary the capture runs.
	binaryEnv = "SDA_WIRE_BINARY"

	stepTimeout = 30 * time.Second
	settle      = time.Second
	stopTimeout = 10 * time.Second

	dryRunMessage      = "dry-run: would set device rules"
	subscribedMessage  = "subscribed to docker events"
	resubscribeMessage = "docker event stream re-subscribed; reconciling running containers"
)

// daemonLog collects the JSON log records of a running daemon.
type daemonLog struct {
	mu      sync.Mutex
	records []map[string]any
}

func (l *daemonLog) collect(scanner *bufio.Scanner) {
	for scanner.Scan() {
		var record map[string]any
		if json.Unmarshal(scanner.Bytes(), &record) != nil {
			continue
		}

		l.mu.Lock()
		l.records = append(l.records, record)
		l.mu.Unlock()
	}
}

// count is how many records have msg, and the id containerID when it is not empty.
func (l *daemonLog) count(msg, containerID string) int {
	l.mu.Lock()
	defer l.mu.Unlock()

	matched := 0

	for _, record := range l.records {
		if record["msg"] == msg && (containerID == "" || record["id"] == containerID) {
			matched++
		}
	}

	return matched
}

// TestCaptureDaemonWire runs the Go daemon in dry-run against the proxy and drives it through
// every Engine call it makes: the startup ping, subscription and enumeration with an inspect,
// a start and an unpause event with their inspects, a cut of the event stream with the
// resubscription since the last event and the re-enumeration, then a die and a destroy event.
// The container list and the events are narrowed to the two containers it creates.
func TestCaptureDaemonWire(t *testing.T) {
	binary := os.Getenv(binaryEnv)
	if binary == "" {
		t.Fatalf("set %s to the Go daemon binary (make wire-capture does)", binaryEnv)
	}

	cli := wirecapture.Docker(t)
	version := wirecapture.APIVersion(t, cli)

	var ids wirecapture.IDs

	first := startDeviceContainer(t, cli, &ids, "sda-wire-first")

	proxy, err := wirecapture.Start(
		t.Context(),
		filepath.Join(t.TempDir(), "docker.sock"),
		wirecapture.Options{Keep: ids.Has},
	)
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(proxy.Close)

	logs, stop := runDaemon(t, binary, proxy.Socket)

	await(t, logs, "the startup pass over the first container", dryRunMessage, first, 1)

	second := startDeviceContainer(t, cli, &ids, "sda-wire-second")
	await(t, logs, "the start event of the second container", dryRunMessage, second, 1)

	ctx, cancel := context.WithTimeout(t.Context(), stepTimeout)
	defer cancel()

	_, err = cli.ContainerPause(ctx, second, client.ContainerPauseOptions{})
	if err != nil {
		t.Fatalf("pause: %v", err)
	}

	_, err = cli.ContainerUnpause(ctx, second, client.ContainerUnpauseOptions{})
	if err != nil {
		t.Fatalf("unpause: %v", err)
	}

	await(t, logs, "the unpause event of the second container", dryRunMessage, second, 2)

	if cut := proxy.Cut("/events"); cut != 1 {
		t.Fatalf("cut %d event streams, want 1", cut)
	}

	await(t, logs, "the resubscription", resubscribeMessage, "", 1)
	await(t, logs, "the re-enumeration", dryRunMessage, first, 2)

	stopSeconds := 1

	_, err = cli.ContainerStop(ctx, second, client.ContainerStopOptions{Timeout: &stopSeconds})
	if err != nil {
		t.Fatalf("stop: %v", err)
	}

	_, err = cli.ContainerRemove(ctx, second, client.ContainerRemoveOptions{})
	if err != nil {
		t.Fatalf("remove: %v", err)
	}

	// Neither event causes a request: wait until the open stream has delivered both.
	for _, action := range []string{"die", "destroy"} {
		wirecapture.Eventually(t, stepTimeout, "the "+action+" event", func() bool {
			return delivered(proxy, action, second)
		})
	}

	proxy.AwaitQuiet(t, settle)
	stop()

	err = proxy.Write(wirecapture.Fixture(t, version, "daemon.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
}

// TestCaptureDaemonPingFallback runs the Go daemon's startup against a proxy that refuses HEAD
// /_ping with 405, as an Engine without HEAD support would, so moby's ping falls back to GET
// /_ping: the negotiation's and the subscription's. The list and the events are narrowed to no
// container.
func TestCaptureDaemonPingFallback(t *testing.T) {
	binary := os.Getenv(binaryEnv)
	if binary == "" {
		t.Fatalf("set %s to the Go daemon binary (make wire-capture does)", binaryEnv)
	}

	cli := wirecapture.Docker(t)
	version := wirecapture.APIVersion(t, cli)

	var none wirecapture.IDs

	proxy, err := wirecapture.Start(
		t.Context(),
		filepath.Join(t.TempDir(), "docker.sock"),
		wirecapture.Options{Keep: none.Has, RefuseHeadPing: true},
	)
	if err != nil {
		t.Fatal(err)
	}

	t.Cleanup(proxy.Close)

	logs, stop := runDaemon(t, binary, proxy.Socket)

	await(t, logs, "the subscription", subscribedMessage, "", 1)
	proxy.AwaitQuiet(t, settle)
	stop()

	err = proxy.Write(wirecapture.Fixture(t, version, "daemon-ping-fallback.jsonl"))
	if err != nil {
		t.Fatal(err)
	}
}

// delivered reports whether an event stream the proxy recorded has delivered action for
// containerID.
func delivered(proxy *wirecapture.Proxy, action, containerID string) bool {
	for _, body := range proxy.Bodies("/events") {
		for line := range strings.Lines(body) {
			var message map[string]any
			if json.Unmarshal([]byte(line), &message) != nil {
				continue
			}

			actor, _ := message["Actor"].(map[string]any)
			if message["Action"] == action && actor["ID"] == containerID {
				return true
			}
		}
	}

	return false
}

// startDeviceContainer starts a busybox container that opts in and bind-mounts /dev/null, so the
// daemon inspects it and collects a rule, and adds it to ids.
func startDeviceContainer(
	t *testing.T,
	cli *client.Client,
	ids *wirecapture.IDs,
	name string,
) string {
	t.Helper()

	containerID := wirecapture.Create(t, cli, name,
		&container.Config{
			Image:  wirecapture.TestImage,
			Cmd:    []string{"sleep", "300"},
			Labels: map[string]string{"swarm-device-access.enable": "true"},
		},
		&container.HostConfig{Binds: []string{"/dev/null:/dev/null"}},
	)
	ids.Add(containerID)
	wirecapture.StartContainer(t, cli, containerID)

	return containerID
}

// runDaemon starts the daemon in dry-run on socket and returns its log and a stop that sends
// SIGTERM and waits for it to exit.
func runDaemon(t *testing.T, binary, socket string) (logs *daemonLog, stop func()) {
	t.Helper()

	cmd := exec.CommandContext(t.Context(), binary,
		"-dry-run", "-log-format=json", "-log-level=debug", "-docker-socket="+socket)

	output, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatal(err)
	}

	cmd.Stderr = cmd.Stdout

	err = cmd.Start()
	if err != nil {
		t.Fatalf("start %s: %v", binary, err)
	}

	logs = &daemonLog{}
	collected := make(chan struct{})

	go func() {
		defer close(collected)

		logs.collect(bufio.NewScanner(output))
	}()

	var once sync.Once

	stop = func() {
		once.Do(func() {
			_ = cmd.Process.Signal(syscall.SIGTERM)

			done := make(chan error, 1)

			go func() {
				<-collected

				done <- cmd.Wait()
			}()

			select {
			case err := <-done:
				if err != nil {
					t.Errorf("daemon exit: %v", err)
				}
			case <-time.After(stopTimeout):
				_ = cmd.Process.Kill()

				t.Error("daemon did not stop on SIGTERM")
			}
		})
	}

	t.Cleanup(stop)

	return logs, stop
}

func await(t *testing.T, logs *daemonLog, what, msg, containerID string, want int) {
	t.Helper()

	wirecapture.Eventually(t, stepTimeout, what, func() bool {
		return logs.count(msg, containerID) >= want
	})
}
