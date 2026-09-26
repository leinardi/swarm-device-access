//go:build integration

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

package integration

import (
	"bufio"
	"context"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"maps"
	"net"
	"os"
	"os/exec"
	"strings"
	"sync"
	"testing"
	"time"

	"github.com/moby/moby/api/types/container"
	dockerclient "github.com/moby/moby/client"
)

const (
	// envBinary overrides the daemon binary path. Defaults to defaultBinary.
	envBinary = "SDA_TEST_BINARY"
	// defaultBinary is the path produced by `make go-build`.
	defaultBinary = "../../dist/swarm-device-access"

	// envEnforce opts into the real-enforcement test (enforce_test.go) and
	// makes missing prerequisites fail instead of skip.
	envEnforce = "SDA_IT_ENFORCE"

	// envSuiteTimeout overrides defaultSuiteTimeout. It must stay below the
	// `go test -timeout` of the Make target (INTEGRATION_TIMEOUT) so that
	// cleanups still run before Go's hard timeout panics the binary.
	envSuiteTimeout     = "SDA_IT_SUITE_TIMEOUT"
	defaultSuiteTimeout = 4 * time.Minute

	// labelEnvID tags every container the suite creates with the run's ID so
	// leftovers of a killed run can be found and removed. It deliberately sits
	// outside the swarm-device-access. prefix: the daemon warns about unknown
	// keys under that prefix.
	labelEnvID = "swarm-device-access-it.envid"

	// detectTimeout is how long to wait for an expected daemon log record,
	// the startup subscription included.
	detectTimeout = 15 * time.Second
	// cleanupTimeout bounds each teardown call. Teardown runs after the test
	// context is canceled, so it uses its own context.
	cleanupTimeout = 10 * time.Second
	// imagePullTimeout bounds the one-off pull of testImage.
	imagePullTimeout = 45 * time.Second

	// Messages of the daemon's log records the tests wait for.
	msgReady         = "subscribed to docker events"
	msgProcessed     = "container processed"
	msgSkippedPolicy = "container skipped by policy"
	msgInvalidLabels = "container skipped: invalid policy labels"

	testImage = "docker.io/library/busybox:1.36"
)

var (
	// suiteDeadline ends every test context, so blocked tests fail fast and
	// m.Run returns before the go test -timeout panics.
	suiteDeadline time.Time //nolint:gochecknoglobals // set once by TestMain, read by every test
	// envID identifies this run in labelEnvID.
	envID string //nolint:gochecknoglobals // set once by TestMain, read by every test

	pullTestImageOnce sync.Once //nolint:gochecknoglobals // one pull per run
	errPullTestImage  error     //nolint:gochecknoglobals // result of pullTestImageOnce

	errSuiteDeadline = errors.New("integration suite deadline exceeded")
)

func TestMain(m *testing.M) {
	os.Exit(runSuite(m))
}

func runSuite(m *testing.M) int {
	timeout := defaultSuiteTimeout

	raw := os.Getenv(envSuiteTimeout)
	if raw != "" {
		parsed, err := time.ParseDuration(raw)
		if err != nil || parsed <= 0 {
			fmt.Fprintf(
				os.Stderr,
				"invalid %s=%q: want a positive duration\n",
				envSuiteTimeout,
				raw,
			)

			return 2
		}

		timeout = parsed
	}

	suiteDeadline = time.Now().Add(timeout)

	idBytes := make([]byte, 6)
	_, _ = rand.Read(idBytes)
	envID = hex.EncodeToString(idBytes)

	fmt.Fprintf(os.Stderr,
		"integration run %s (deadline %s); if it is killed, remove leftovers with:\n"+
			"  docker ps -aq --filter label=%s=%s | xargs -r docker rm -f\n",
		envID, timeout, labelEnvID, envID)

	return m.Run()
}

// testCtx returns a context canceled by whichever ends first: the test or the
// suite deadline. Every wait in a test must use it.
func testCtx(t *testing.T) context.Context {
	t.Helper()

	// Checked up front so that a test starting after the deadline fails
	// instead of reaching a helper that would report the dead context as a
	// missing prerequisite and skip.
	if !time.Now().Before(suiteDeadline) {
		t.Fatalf("%v (%s=%s)", errSuiteDeadline, envSuiteTimeout, os.Getenv(envSuiteTimeout))
	}

	ctx, cancel := context.WithDeadlineCause(t.Context(), suiteDeadline, errSuiteDeadline)
	t.Cleanup(cancel)

	return ctx
}

// ---- daemon process ----

// logRecord is one JSON log line of the daemon.
type logRecord map[string]any

func (r logRecord) str(key string) string {
	val, _ := r[key].(string)

	return val
}

// num returns a numeric attribute; encoding/json decodes numbers as float64.
func (r logRecord) num(key string) (int, bool) {
	val, ok := r[key].(float64)

	return int(val), ok
}

// daemonProc is a running daemon whose JSON log is collected line by line.
type daemonProc struct {
	mu      sync.Mutex
	records []logRecord
	// changed is closed and replaced every time a record is appended.
	changed chan struct{}
	// exited is closed when the daemon's stdout reaches EOF.
	exited chan struct{}
}

// matcher selects log records.
type matcher func(logRecord) bool

// forContainer matches records with the given message about containerID.
func forContainer(msg, containerID string) matcher {
	return func(rec logRecord) bool {
		return rec.str("msg") == msg && rec.str("id") == containerID
	}
}

// count returns how many records match so far.
func (d *daemonProc) count(match matcher) int {
	return len(d.matching(match))
}

func (d *daemonProc) matching(match matcher) []logRecord {
	d.mu.Lock()
	defer d.mu.Unlock()

	var out []logRecord

	for _, rec := range d.records {
		if match(rec) {
			out = append(out, rec)
		}
	}

	return out
}

// waitN blocks until at least n records match and returns them. It fails the
// test when timeout or ctx ends first, or when the daemon exits.
func (d *daemonProc) waitN(
	ctx context.Context,
	t *testing.T,
	what string,
	match matcher,
	n int,
) []logRecord {
	t.Helper()

	waitCtx, cancel := context.WithTimeout(ctx, detectTimeout)
	defer cancel()

	for {
		d.mu.Lock()
		changed := d.changed
		d.mu.Unlock()

		found := d.matching(match)
		if len(found) >= n {
			return found
		}

		select {
		case <-changed:
		case <-d.exited:
			found = d.matching(match)
			if len(found) >= n {
				return found
			}

			t.Fatalf("daemon exited before logging %s (want %d, got %d)", what, n, len(found))
		case <-waitCtx.Done():
			t.Fatalf(
				"waiting for %s (want %d, got %d): %v",
				what,
				n,
				len(found),
				context.Cause(waitCtx),
			)
		}
	}
}

// wait blocks until one record matches and returns it.
func (d *daemonProc) wait(ctx context.Context, t *testing.T, what string, match matcher) logRecord {
	t.Helper()

	return d.waitN(ctx, t, what, match, 1)[0]
}

func (d *daemonProc) append(rec logRecord) {
	d.mu.Lock()
	defer d.mu.Unlock()

	d.records = append(d.records, rec)
	close(d.changed)
	d.changed = make(chan struct{})
}

// collect parses the daemon's output and passes every line to logf. Lines
// that are not JSON (a panic, say) are only logged. It takes t.Log rather than
// t, so the lines are attributed to collect instead of the goroutine's runtime
// frame, which is what a t.Helper() call here would produce.
func (d *daemonProc) collect(logf func(args ...any), r io.Reader) {
	defer close(d.exited)

	scanner := bufio.NewScanner(r)
	for scanner.Scan() {
		line := scanner.Text()
		logf("[daemon]", line)

		var rec logRecord

		err := json.Unmarshal([]byte(line), &rec)
		if err != nil {
			continue
		}

		d.append(rec)
	}
}

// launchDaemon starts the daemon binary with JSON debug logging plus flags,
// waits until it subscribes to Docker events and returns its log collector.
// The process is killed when the test ends.
func launchDaemon(ctx context.Context, t *testing.T, flags ...string) *daemonProc {
	t.Helper()

	binary := findBinary(ctx, t)

	args := append([]string{"-log-format=json", "-log-level=debug"}, flags...)
	cmd := exec.CommandContext(ctx, binary, args...)

	stdout, err := cmd.StdoutPipe()
	if err != nil {
		t.Fatalf("stdout pipe: %v", err)
	}

	cmd.Stderr = cmd.Stdout

	err = cmd.Start()
	if err != nil {
		t.Fatalf("start daemon: %v", err)
	}

	proc := newDaemonProc()

	go proc.collect(t.Log, stdout)

	t.Cleanup(func() {
		_ = cmd.Process.Kill()

		<-proc.exited

		_ = cmd.Wait()
	})

	proc.waitReady(ctx, t)

	return proc
}

func newDaemonProc() *daemonProc {
	return &daemonProc{changed: make(chan struct{}), exited: make(chan struct{})}
}

// waitReady blocks until the daemon has subscribed to Docker events.
func (d *daemonProc) waitReady(ctx context.Context, t *testing.T) {
	t.Helper()

	d.waitN(ctx, t, "daemon subscription to docker events", withMsg(msgReady), 1)
}

// withMsg matches records with the given message.
func withMsg(msg string) matcher {
	return func(rec logRecord) bool { return rec.str("msg") == msg }
}

// requireProcessed waits for the daemon to process containerID and checks the
// exact counts of its `container processed` record.
func requireProcessed(
	ctx context.Context,
	t *testing.T,
	proc *daemonProc,
	containerID string,
	granted, skipped int,
) {
	t.Helper()

	rec := proc.wait(ctx, t, "container processed for "+shortID(containerID),
		forContainer(msgProcessed, containerID))
	checkCounts(t, rec, granted, skipped)
}

func checkCounts(t *testing.T, rec logRecord, granted, skipped int) {
	t.Helper()

	for key, want := range map[string]int{"devices_granted": granted, "skipped": skipped, "errors": 0} {
		got, ok := rec.num(key)
		if !ok || got != want {
			t.Errorf("container processed: %s = %v, want %d (record %v)", key, rec[key], want, rec)
		}
	}
}

// requireSkipped waits for a skip record about containerID and checks that the
// daemon did not process the container anyway. A skip and a processed record
// are alternative outcomes of one pass, so once the skip is logged no processed
// record for that pass can follow.
func requireSkipped(
	ctx context.Context,
	t *testing.T,
	proc *daemonProc,
	containerID, skipMsg string,
) {
	t.Helper()

	proc.wait(ctx, t, fmt.Sprintf("%q for %s", skipMsg, shortID(containerID)),
		forContainer(skipMsg, containerID))

	processed := proc.matching(forContainer(msgProcessed, containerID))
	if len(processed) > 0 {
		t.Errorf("container %s was skipped but also processed: %v", shortID(containerID), processed)
	}
}

// enforceRequested reports whether the run opted into the real-enforcement
// test. Opting in also makes every missing prerequisite a failure: a run that
// asked for enforcement must not go green by skipping it.
func enforceRequested() bool {
	return os.Getenv(envEnforce) == "1"
}

// requireOrSkip stops the test when a prerequisite is missing (err != nil). It
// fails when strict is set and skips otherwise.
func requireOrSkip(t *testing.T, strict bool, err error) {
	t.Helper()

	if err == nil {
		return
	}

	if strict {
		t.Fatalf("missing prerequisite (%s=1 makes this a failure): %v", envEnforce, err)
	}

	t.Skipf("missing prerequisite: %v", err)
}

func findBinary(ctx context.Context, t *testing.T) string {
	t.Helper()

	path := os.Getenv(envBinary)
	if path == "" {
		path = defaultBinary
	}

	_, err := os.Stat(path)
	if err != nil {
		requireOrSkip(t, enforceRequested(), fmt.Errorf(
			"daemon binary not found at %q (set %s or run make go-build): %w",
			path,
			envBinary,
			err,
		))
	}

	// Probe execability: a cross-compiled Linux binary on macOS returns
	// "exec format error" which would crash the test later. Skip instead.
	err = exec.CommandContext(ctx, path, "-help").Run()
	if err != nil {
		if strings.Contains(err.Error(), "exec format error") ||
			strings.Contains(err.Error(), "cannot execute") {
			requireOrSkip(t, enforceRequested(), fmt.Errorf(
				"daemon binary %q is not executable on this platform (cross-compiled?): %w",
				path,
				err,
			))
		}
		// Non-zero exit is fine: -help exits 0, but any other error means it ran.
	}

	return path
}

// freeLocalAddr returns a loopback address with a port that was free a moment
// ago. Another process can still take it before the daemon binds it; the
// window is small enough for a test.
func freeLocalAddr(ctx context.Context, t *testing.T) string {
	t.Helper()

	var listenCfg net.ListenConfig

	listener, err := listenCfg.Listen(ctx, "tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatalf("probe free port: %v", err)
	}

	addr := listener.Addr().String()
	_ = listener.Close()

	return addr
}

// ---- Docker ----

func requireDocker(t *testing.T) *dockerclient.Client {
	t.Helper()

	cli, err := dockerclient.New(dockerclient.FromEnv)
	if err != nil {
		requireOrSkip(t, enforceRequested(), fmt.Errorf("docker client init: %w", err))
	}

	ctx, cancel := context.WithTimeout(testCtx(t), 5*time.Second)
	defer cancel()

	_, err = cli.Ping(ctx, dockerclient.PingOptions{})
	if err != nil {
		requireOrSkip(t, enforceRequested(), fmt.Errorf("docker daemon not reachable: %w", err))
	}

	t.Cleanup(func() { _ = cli.Close() })

	return cli
}

func ensureTestImage(t *testing.T, cli *dockerclient.Client) {
	t.Helper()

	pullTestImageOnce.Do(func() {
		ctx, cancel := context.WithTimeout(testCtx(t), imagePullTimeout)
		defer cancel()

		reader, err := cli.ImagePull(ctx, testImage, dockerclient.ImagePullOptions{})
		if err != nil {
			errPullTestImage = err

			return
		}
		defer reader.Close()

		_, err = io.Copy(io.Discard, reader)
		if err != nil {
			errPullTestImage = err
		}
	})

	if errPullTestImage != nil {
		t.Fatalf("pull test image %s: %v", testImage, errPullTestImage)
	}
}

// startTestContainer creates and starts a busybox container carrying labels,
// labelEnvID and binds. It is removed when the test ends.
func startTestContainer(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	labels map[string]string,
	binds []string,
) string {
	t.Helper()

	return startContainer(ctx, t, cli,
		&container.Config{Image: testImage, Cmd: []string{"sleep", "300"}, Labels: labels},
		&container.HostConfig{Binds: binds})
}

// startContainer creates and starts a container from cfg and hostCfg, adding
// labelEnvID to its labels. It is removed when the test ends.
func startContainer(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	cfg *container.Config,
	hostCfg *container.HostConfig,
) string {
	t.Helper()

	allLabels := map[string]string{labelEnvID: envID}
	maps.Copy(allLabels, cfg.Labels)

	withLabels := *cfg
	withLabels.Labels = allLabels

	resp, err := cli.ContainerCreate(ctx, dockerclient.ContainerCreateOptions{
		Config:     &withLabels,
		HostConfig: hostCfg,
	})
	if err != nil {
		t.Fatalf("create container from %s: %v", cfg.Image, err)
	}

	t.Cleanup(func() { removeContainer(ctx, t, cli, resp.ID) })

	_, err = cli.ContainerStart(ctx, resp.ID, dockerclient.ContainerStartOptions{})
	if err != nil {
		t.Fatalf("start container from %s: %v", cfg.Image, err)
	}

	t.Logf("started container %s from %s", shortID(resp.ID), cfg.Image)

	return resp.ID
}

// removeContainer force-removes containerID. It runs as a cleanup, after the
// test context is canceled, so it drops that cancellation and relies on
// cleanupTimeout instead.
func removeContainer(
	ctx context.Context,
	t *testing.T,
	cli *dockerclient.Client,
	containerID string,
) {
	t.Helper()

	removeCtx, cancel := context.WithTimeout(context.WithoutCancel(ctx), cleanupTimeout)
	defer cancel()

	_, err := cli.ContainerRemove(
		removeCtx,
		containerID,
		dockerclient.ContainerRemoveOptions{Force: true},
	)
	if err != nil {
		t.Logf("remove container %s: %v", shortID(containerID), err)
	}
}

func shortID(id string) string {
	const shortLen = 12

	if len(id) > shortLen {
		return id[:shortLen]
	}

	return id
}
