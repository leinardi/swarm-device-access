//go:build goldencapture

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
	"bytes"
	"io"
	"net/http"
	"net/http/httptest"
	"strings"
	"testing"
	"time"

	"github.com/leinardi/swarm-device-access/internal/goldencapture"
)

// recorderSequence is the scripted sequence of Recorder calls the exposition golden is taken
// after: every method, every label value the daemon uses, a counter left at zero by an Add(0),
// apply durations on and between the histogram's bucket bounds, and gauges set more than once.
func recorderSequence(recorder *Recorder) {
	for _, action := range []string{"start", "start", "unpause", "die", "destroy", "destroy"} {
		recorder.RecordEvent(action)
	}

	recorder.RecordRuleApplied(true)
	recorder.RecordRuleApplied(true)
	recorder.RecordRuleApplied(true)
	recorder.RecordRuleApplied(false)
	recorder.IncReloadReapply()
	recorder.IncDockerReconnect()
	recorder.IncDockerReconnect()

	for _, duration := range []time.Duration{
		time.Millisecond, 5 * time.Millisecond, 7 * time.Millisecond, 250 * time.Millisecond,
		time.Second, 3 * time.Second, 12 * time.Second, 1500 * time.Microsecond, 0,
	} {
		recorder.ObserveApplyDuration(duration)
	}

	recorder.RecordContainerScanned()
	recorder.RecordContainerScanned()
	recorder.RecordContainerSkipped("policy")
	recorder.RecordContainerSkipped("invalid_labels")
	recorder.RecordContainerSkipped("policy")
	recorder.AddDeviceFilesDiscovered(5)
	recorder.AddCandidatesSkipped("dangling", 2)
	recorder.AddCandidatesSkipped("outside_dev", 1)
	recorder.AddCandidatesSkipped("not_device", 0)
	recorder.AddRuleFailures(1)
	recorder.AddDryRunSkips(3)
	recorder.SetLastEvent(time.Date(2026, 10, 25, 14, 3, 4, 567891234, time.FixedZone("CET", 3600)))
	recorder.SetPendingContainers(4)
	recorder.SetPendingContainers(2)
	recorder.SetReloadIncomplete(true)
	recorder.SetReloadIncomplete(false)
}

// TestGoldenRecorderExposition captures what GET /metrics serves for the daemon's own families
// after recorderSequence: tests/testdata/metrics/recorder.prom holds the sda_ families (the
// HELP, TYPE and sample lines; the go_, process_ and promhttp_ families vary with the run and are
// left out), and recorder.headers the Content-Type of a request that sends no Accept.
func TestGoldenRecorderExposition(t *testing.T) {
	recorderSequence(NewRecorder())

	server := httptest.NewServer(newMetricsServer().Handler)
	defer server.Close()

	request, err := http.NewRequestWithContext(
		t.Context(),
		http.MethodGet,
		server.URL+"/metrics",
		http.NoBody,
	)
	if err != nil {
		t.Fatal(err)
	}

	response, err := http.DefaultClient.Do(request)
	if err != nil {
		t.Fatal(err)
	}

	defer func() { _ = response.Body.Close() }()

	body, err := io.ReadAll(response.Body)
	if err != nil {
		t.Fatal(err)
	}

	var exposition bytes.Buffer

	for line := range strings.Lines(string(body)) {
		if strings.HasPrefix(line, "sda_") || strings.HasPrefix(line, "# HELP sda_") ||
			strings.HasPrefix(line, "# TYPE sda_") {
			exposition.WriteString(line)
		}
	}

	goldencapture.Check(t, "tests/testdata/metrics/recorder.prom", exposition.Bytes())
	goldencapture.Check(t, "tests/testdata/metrics/recorder.headers",
		[]byte("Content-Type: "+response.Header.Get("Content-Type")+"\n"))
}
