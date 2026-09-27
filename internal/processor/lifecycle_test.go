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

package processor

import (
	"os"
	"path/filepath"
	"strconv"
	"testing"
	"time"

	"github.com/leinardi/swarm-device-access/internal/policy"
)

const restartedAt = "2026-09-27T10:05:00.000000000Z"

func mustTime(t *testing.T, value string) time.Time {
	t.Helper()

	parsed, err := time.Parse(time.RFC3339Nano, value)
	if err != nil {
		t.Fatal(err)
	}

	return parsed
}

// TestLifecycle_InspectFailureThenSuccessUpgradesRecord: an inspect
// failure leaves a provisional record; the next successful reconcile
// upgrades that same record to verified with the run's cgroup.
func TestLifecycle_InspectFailureThenSuccessUpgradesRecord(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.insp.then(inspectReply{err: errDaemonUnavail})

	_ = env.reconcile()

	recs := env.proc.lifecycleStore().Records("abc")
	if len(recs) != 1 || recs[0].State != LifecycleProvisional || recs[0].HasIdentity() {
		t.Fatalf("records = %+v, want one provisional record without a cgroup", recs)
	}

	env.insp.then(runningDevNull(reconcileStarted))

	err := env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	recs = env.proc.lifecycleStore().Records("abc")
	if len(recs) != 1 || recs[0].State != LifecycleVerified ||
		recs[0].StartedAt != reconcileStarted || recs[0].Identity != env.fake.identity {
		t.Fatalf("records = %+v, want the provisional record upgraded to verified", recs)
	}
}

// TestTerminate_StaleDieAfterRestartLeavesNewRun: the container restarted
// under the same ID into a new cgroup; a die event for the old run,
// delivered late, revokes the old run's cgroup only.
func TestTerminate_StaleDieAfterRestartLeavesNewRun(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)

	oldIdentity := env.fake.identity

	// The restart: a new cgroup directory and start time.
	newDir := filepath.Join(env.proc.HostRoot, "sys", "fs", "cgroup", "docker", "restarted")

	err := os.MkdirAll(newDir, 0o755)
	if err != nil {
		t.Fatal(err)
	}

	err = os.WriteFile(
		filepath.Join(newDir, "cgroup.procs"),
		[]byte(strconv.Itoa(reconcilePid)+"\n"),
		0o600,
	)
	if err != nil {
		t.Fatal(err)
	}

	err = os.WriteFile(
		filepath.Join(env.proc.ProcRoot, "proc", strconv.Itoa(reconcilePid), "cgroup"),
		[]byte("0::/docker/restarted\n"),
		0o600,
	)
	if err != nil {
		t.Fatal(err)
	}

	env.insp.then(runningDevNull(restartedAt))

	err = env.reconcile()
	if err != nil {
		t.Fatal(err)
	}

	newIdentity := env.fake.identity

	err = env.proc.Terminate("abc", mustTime(t, restartedAt).Add(-time.Minute))
	if err != nil {
		t.Fatal(err)
	}

	if env.fake.calls != 3 || len(env.fake.rules[2]) != 0 || env.fake.identity != oldIdentity {
		t.Fatalf("calls = %d, last identity = %+v; want one empty set on the old run's cgroup %+v",
			env.fake.calls, env.fake.identity, oldIdentity)
	}

	newRun, ok := env.proc.lifecycleStore().lookup("abc", restartedAt)
	if !ok || newRun.State != LifecycleVerified || newRun.Identity != newIdentity {
		t.Errorf("new run = %+v, want it verified and untouched", newRun)
	}
}

// TestTerminate_DieRevokesAndRetainsUntilGone: a die event while the
// cgroup still exists revokes there and keeps the record; the sweep
// releases it only once the cgroup is gone.
func TestTerminate_DieRevokesAndRetainsUntilGone(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)

	err := env.proc.Terminate("abc", mustTime(t, reconcileStarted).Add(time.Minute))
	if err != nil {
		t.Fatal(err)
	}

	if env.fake.calls != 2 || len(env.fake.rules[1]) != 0 {
		t.Fatalf("rules = %v, want the empty set applied", env.fake.rules)
	}

	recs := verifiedRecords(env.proc)
	if len(recs) != 1 || recs[0].State != LifecycleTerminal || !recs[0].Revoked {
		t.Fatalf("records = %+v, want the run kept as terminal and revoked", recs)
	}

	if released := env.proc.Sweep(); released != 0 || len(verifiedRecords(env.proc)) != 1 {
		t.Fatalf("sweep released %d with the cgroup present, want 0", released)
	}

	err = os.RemoveAll(testCgroupDir(env.proc.HostRoot))
	if err != nil {
		t.Fatal(err)
	}

	if released := env.proc.Sweep(); released != 1 || len(verifiedRecords(env.proc)) != 0 {
		t.Fatalf("sweep released %d after the cgroup was removed, want 1", released)
	}

	if env.fake.calls != 2 {
		t.Errorf("sweep mutated: calls = %d, want 2", env.fake.calls)
	}
}

// TestTerminate_ProvisionalOnly: without a verified run the provisional
// record becomes terminal, nothing is mutated, and the sweep drops it.
func TestTerminate_ProvisionalOnly(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.insp.then(inspectReply{err: errDaemonUnavail})

	_ = env.reconcile()

	err := env.proc.Terminate("abc", time.Now())
	if err != nil {
		t.Fatal(err)
	}

	recs := env.proc.lifecycleStore().Records("abc")
	if len(recs) != 1 || recs[0].State != LifecycleTerminal || env.fake.calls != 0 {
		t.Fatalf("records = %+v, calls = %d; want the provisional record terminal and no mutation",
			recs, env.fake.calls)
	}

	if released := env.proc.Sweep(); released != 1 ||
		len(env.proc.lifecycleStore().Records("abc")) != 0 {
		t.Fatalf("sweep released %d, want the terminal provisional record dropped", released)
	}
}

// TestSweep_DropsDeadEntries: the sweep releases runs whose cgroup is gone
// or recreated and stale provisional records, and keeps live runs.
func TestSweep_DropsDeadEntries(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	env.grantOnce(t)

	store := env.proc.lifecycleStore()
	now := time.Now()
	store.now = func() time.Time { return now }

	store.provisional("gone")

	if released := env.proc.Sweep(); released != 0 {
		t.Fatalf("sweep released %d live or fresh records, want 0", released)
	}

	store.now = func() time.Time { return now.Add(provisionalTTL + time.Second) }

	if released := env.proc.Sweep(); released != 1 || len(store.Records("gone")) != 0 {
		t.Fatalf("sweep released %d, want the stale provisional record dropped", released)
	}

	// Recreated: same path, new inode.
	dir := testCgroupDir(env.proc.HostRoot)

	err := os.Mkdir(dir+".new", 0o755)
	if err != nil {
		t.Fatal(err)
	}

	err = os.RemoveAll(dir)
	if err != nil {
		t.Fatal(err)
	}

	err = os.Rename(dir+".new", dir)
	if err != nil {
		t.Fatal(err)
	}

	if released := env.proc.Sweep(); released != 1 || len(verifiedRecords(env.proc)) != 0 {
		t.Fatalf("sweep released %d, want the run with a recreated cgroup released", released)
	}

	if env.fake.calls != 1 {
		t.Errorf("sweep mutated: calls = %d, want only the grant", env.fake.calls)
	}
}

// TestSweep_ReleasesOnlyTheEndedRecord: records without a start time all
// share an empty StartedAt; dropping an ended one must keep a fresh one.
func TestSweep_ReleasesOnlyTheEndedRecord(t *testing.T) {
	env := newReconcileEnv(t, policy.ModeAll, false)
	store := env.proc.lifecycleStore()

	now := time.Now()
	store.now = func() time.Time { return now }
	store.provisional("abc")

	_ = store.terminate("abc", now)

	store.now = func() time.Time { return now.Add(time.Second) }
	store.provisional("abc")

	if released := env.proc.Sweep(); released != 1 {
		t.Fatalf("sweep released %d, want 1", released)
	}

	recs := store.Records("abc")
	if len(recs) != 1 || recs[0].State != LifecycleProvisional {
		t.Fatalf("records = %+v, want only the fresh provisional record", recs)
	}
}
