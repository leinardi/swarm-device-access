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

package wirecapture

import (
	"context"
	"io"
	"maps"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	"github.com/moby/moby/client"
)

const (
	// TestImage is the image every capture container runs, the integration suite's busybox,
	// pinned by digest: the launcher capture runs it in a privileged container.
	TestImage = "docker.io/library/busybox:1.36" +
		"@sha256:73aaf090f3d85aa34ee199857f03fa3a95c8ede2ffd4cc2cdb5b94e566b11662"

	// LabelCapture marks every container a capture creates, so a leftover can be found.
	LabelCapture = "swarm-device-access-wirecapture"

	pullTimeout    = 2 * time.Minute
	callTimeout    = 30 * time.Second
	pollInterval   = 50 * time.Millisecond
	quietPollLimit = 30 * time.Second
)

// Docker returns a client of DockerSocket for setting up a capture; its own requests do not go
// through the proxy. It pulls TestImage when it is missing.
func Docker(t *testing.T) *client.Client {
	t.Helper()

	cli, err := client.New(client.WithHost("unix://" + DockerSocket))
	if err != nil {
		t.Fatalf("docker client: %v", err)
	}

	t.Cleanup(func() { _ = cli.Close() })

	ctx, cancel := context.WithTimeout(t.Context(), pullTimeout)
	defer cancel()

	_, err = cli.ImageInspect(ctx, TestImage)
	if err == nil {
		return cli
	}

	reader, err := cli.ImagePull(ctx, TestImage, client.ImagePullOptions{})
	if err != nil {
		t.Fatalf("pull %s: %v", TestImage, err)
	}

	defer func() { _ = reader.Close() }()

	_, err = io.Copy(io.Discard, reader)
	if err != nil {
		t.Fatalf("pull %s: %v", TestImage, err)
	}

	return cli
}

// APIVersion is the API version moby client v0.6.0 negotiates with the daemon at DockerSocket,
// the one in every versioned path the binaries send, which names the fixture directory: the lower
// of the daemon's and the client's newest.
func APIVersion(t *testing.T, cli *client.Client) string {
	t.Helper()

	ctx, cancel := context.WithTimeout(t.Context(), callTimeout)
	defer cancel()

	_, err := cli.Ping(ctx, client.PingOptions{NegotiateAPIVersion: true})
	if err != nil {
		t.Fatalf("ping docker: %v", err)
	}

	return cli.ClientVersion()
}

// Create creates a container from config and hostConfig, labeled LabelCapture, and removes it
// when the test ends.
func Create(
	t *testing.T,
	cli *client.Client,
	name string,
	config *container.Config,
	hostConfig *container.HostConfig,
) string {
	t.Helper()

	labeled := *config
	labeled.Labels = map[string]string{LabelCapture: "1"}

	maps.Copy(labeled.Labels, config.Labels)

	ctx, cancel := context.WithTimeout(t.Context(), callTimeout)
	defer cancel()

	created, err := cli.ContainerCreate(ctx, client.ContainerCreateOptions{
		Name:       name,
		Config:     &labeled,
		HostConfig: hostConfig,
	})
	if err != nil {
		t.Fatalf("create %s: %v", name, err)
	}

	t.Cleanup(func() {
		removeCtx, cancelRemove := context.WithTimeout(context.WithoutCancel(ctx), callTimeout)
		defer cancelRemove()

		_, _ = cli.ContainerRemove(
			removeCtx,
			created.ID,
			client.ContainerRemoveOptions{Force: true},
		)
	})

	return created.ID
}

// StartContainer starts containerID.
func StartContainer(t *testing.T, cli *client.Client, containerID string) {
	t.Helper()

	ctx, cancel := context.WithTimeout(t.Context(), callTimeout)
	defer cancel()

	_, err := cli.ContainerStart(ctx, containerID, client.ContainerStartOptions{})
	if err != nil {
		t.Fatalf("start %s: %v", containerID, err)
	}
}

// IDs is a set of container IDs, safe for concurrent use, whose Has narrows a proxy.
type IDs struct {
	mu  sync.Mutex
	ids map[string]bool
}

// Add puts containerID in the set.
func (s *IDs) Add(containerID string) {
	s.mu.Lock()
	defer s.mu.Unlock()

	if s.ids == nil {
		s.ids = make(map[string]bool)
	}

	s.ids[containerID] = true
}

// Has reports whether containerID is in the set.
func (s *IDs) Has(containerID string) bool {
	s.mu.Lock()
	defer s.mu.Unlock()

	return s.ids[containerID]
}

// AwaitQuiet waits until no request has reached the proxy for quiet, so a capture ends after
// the requests its last step caused.
func (p *Proxy) AwaitQuiet(t *testing.T, quiet time.Duration) {
	t.Helper()

	deadline := time.Now().Add(quietPollLimit)
	seen := p.Requests()
	since := time.Now()

	for time.Since(since) < quiet {
		if time.Now().After(deadline) {
			t.Fatalf("requests kept arriving for %s", quietPollLimit)
		}

		time.Sleep(pollInterval)

		if now := p.Requests(); now != seen {
			seen = now
			since = time.Now()
		}
	}
}

// Fixture is the path of a fixture file under tests/testdata/engine_wire/api-<version>, from the
// repository root above the working directory.
func Fixture(t *testing.T, version, name string) string {
	t.Helper()

	dir, err := os.Getwd()
	if err != nil {
		t.Fatalf("working directory: %v", err)
	}

	for {
		_, statErr := os.Stat(filepath.Join(dir, "go.mod"))
		if statErr == nil {
			return filepath.Join(dir, "tests", "testdata", "engine_wire", "api-"+version, name)
		}

		parent := filepath.Dir(dir)
		if parent == dir {
			t.Fatal("no go.mod above the working directory")
		}

		dir = parent
	}
}

// Eventually polls condition until it holds, failing the test with what after timeout.
func Eventually(t *testing.T, timeout time.Duration, what string, condition func() bool) {
	t.Helper()

	deadline := time.Now().Add(timeout)

	for !condition() {
		if time.Now().After(deadline) {
			t.Fatalf("timed out after %s waiting for %s", timeout, what)
		}

		time.Sleep(pollInterval)
	}
}
