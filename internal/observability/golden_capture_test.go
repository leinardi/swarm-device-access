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
	"slices"
	"strings"
	"testing"
	"time"

	"github.com/prometheus/client_golang/prometheus"
	"github.com/prometheus/client_golang/prometheus/collectors"
	"github.com/prometheus/client_golang/prometheus/promhttp"

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

// negotiationCorpus holds the Accept and Accept-Encoding header values of the negotiation
// golden: each list is the header's values in order, nil for no header.
var negotiationCorpus = []struct{ accept, acceptEncoding []string }{
	{nil, nil},
	{[]string{"text/plain"}, nil},
	{[]string{"text/plain;version=1.0.0"}, nil},
	{[]string{"text/plain;version=1.0.0;escaping=allow-utf-8"}, nil},
	{[]string{"application/json"}, nil},
	{[]string{"text/plain;version=0.0.4;escaping=underscores"}, nil},
	{[]string{"text/plain;version=0.0.4;escaping=dots"}, nil},
	{[]string{"text/plain;version=0.0.4;escaping=values"}, nil},
	{[]string{"text/plain;version=0.0.4;escaping=bogus"}, nil},
	{[]string{"text/plain; version=0.0.4; escaping=allow-utf-8"}, nil},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited,text/plain;version=0.0.4;escaping=allow-utf-8",
		},
		nil,
	},
	{
		[]string{
			"text/plain;version=0.0.4;escaping=allow-utf-8;q=0.5,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.9",
		},
		nil,
	},
	{
		[]string{
			"application/openmetrics-text;version=1.0.0;escaping=allow-utf-8,text/plain;version=0.0.4;escaping=dots;q=0.5",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=text",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=compact-text",
		},
		nil,
	},
	{[]string{"application/openmetrics-text;version=1.0.0"}, nil},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"zstd,gzip"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"*"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"deflate"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"identity;q=0.5, gzip;q=0.6"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"GZIP"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip, identity"}},
	{
		[]string{
			"application/openmetrics-text;version=1.0.0;escaping=allow-utf-8;q=0.7,application/openmetrics-text;version=0.0.1;q=0.6,text/plain;version=1.0.0;escaping=allow-utf-8;q=0.5,text/plain;version=0.0.4;q=0.4,*/*;q=0.3",
		},
		nil,
	},
	{
		[]string{
			"application/openmetrics-text;version=1.0.0;escaping=allow-utf-8;q=0.6,application/openmetrics-text;version=0.0.1;escaping=allow-utf-8;q=0.5,text/plain;version=1.0.0;escaping=allow-utf-8;q=0.4,text/plain;version=0.0.4;escaping=allow-utf-8;q=0.3,*/*;q=0.2",
		},
		nil,
	},
	{[]string{"*/*"}, nil},
	{[]string{"*"}, nil},
	{[]string{"text/*"}, nil},
	{[]string{"application/*"}, nil},
	{[]string{"*/plain"}, nil},
	{[]string{"text/plain/x"}, nil},
	{[]string{"text"}, nil},
	{[]string{" text/plain "}, nil},
	{[]string{"text/plain;version=0.0.4;q=0"}, nil},
	{
		[]string{
			"text/plain;q=0.5,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.5",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.5,text/plain;q=0.5",
		},
		nil,
	},
	{
		[]string{
			"*/*;q=1,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.9",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.1,*/*",
		},
		nil,
	},
	{[]string{"text/plain;escaping=allow-utf-8;escaping=dots"}, nil},
	{[]string{"text/plain;escaping=dots;version=0.0.4"}, nil},
	{[]string{"application/json;escaping=allow-utf-8,text/plain"}, nil},
	{[]string{"application/json;escaping=allow-utf-8;q=0.1,text/plain"}, nil},
	{[]string{"text/plain;version=0.0.5"}, nil},
	{[]string{"text/plain;version="}, nil},
	{[]string{"text/plain;version=0.0.4=1"}, nil},
	{[]string{"text/plain;;version=0.0.4"}, nil},
	{[]string{"text/plain; q=0.5"}, nil},
	{
		[]string{
			"text/plain;q= 0.5,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.4",
		},
		nil,
	},
	{
		[]string{
			"text/plain;q=0x1p-1,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.4",
		},
		nil,
	},
	{
		[]string{
			"text/plain;q=1_0,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.4",
		},
		nil,
	},
	{
		[]string{
			"text/plain;q=nan,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0",
		},
		nil,
	},
	{
		[]string{
			"text/plain;q=-1,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=-0.5",
		},
		nil,
	},
	{
		[]string{
			"text/plain;q=inf,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.30000001,text/plain;q=0.3",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;Q=0.1,text/plain;q=0.2",
		},
		nil,
	},
	{[]string{"TEXT/PLAIN"}, nil},
	{[]string{"text/plain;charset=latin1"}, nil},
	{
		[]string{
			"text/plain;version=0.0.4;escaping=allow-utf-8,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited",
		},
		nil,
	},
	{[]string{",,text/plain,"}, nil},
	{
		[]string{
			"a/b,c/d,e/f,g/h,i/j,k/l,m/n,o/p,q/r,s/t,u/v,w/x,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited",
		},
		nil,
	},
	{
		[]string{
			"*/*;q=0.5,text/*;q=0.5,text/plain;q=0.5,application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;q=0.5;escaping=dots",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited;escaping=allow-utf-8",
		},
		nil,
	},
	{[]string{"application/vnd.google.protobuf;proto=other;encoding=delimited"}, nil},
	{[]string{"application/vnd.google.protobuf"}, nil},
	{[]string{"application/openmetrics-text"}, nil},
	{[]string{"application/openmetrics-text;version=0.0.1;escaping=values"}, nil},
	{
		[]string{
			"text/plain",
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=delimited",
			"text/plain",
		},
		nil,
	},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0.5, identity;q=0.5"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"identity;q=0, gzip;q=0"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0.0001"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=1.5"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=2"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=.5"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip; q=0.5, identity;q=0.4"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip ;q=0.5,identity;q=0.4"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;Q=0.5"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;level=1"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0.5;level=1"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"x-gzip"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"br, gzip"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"*;q=0.5, gzip;q=0.4"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0.4, *;q=0.5"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"identity"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"identity;q=0"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"*;q=0"}},
	{[]string{"text/plain;version=0.0.4"}, []string{""}},
	{[]string{"text/plain;version=0.0.4"}, []string{" "}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip,"}},
	{[]string{"text/plain;version=0.0.4"}, []string{",gzip"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0.12345678901234567890123"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"deflate", "gzip"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"gzip;q=0", "identity"}},
	{[]string{"text/plain;version=0.0.4"}, []string{"zstd;q=1,gzip;q=0.9"}},
	{[]string{"*/plain;version=0.0.4"}, nil},
	{[]string{"text/*;version=0.0.4"}, nil},
	{[]string{"application/*;proto=io.prometheus.client.MetricFamily;encoding=delimited"}, nil},
	{[]string{"*/*;proto=io.prometheus.client.MetricFamily;encoding=text"}, nil},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=compact-text",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=text;escaping=dots",
		},
		nil,
	},
	{
		[]string{
			"application/vnd.google.protobuf;proto=io.prometheus.client.MetricFamily;encoding=other,text/plain;escaping=values",
		},
		nil,
	},
	{[]string{"application/openmetrics-text;version=1.0.0;escaping=dots,application/json"}, nil},
	{[]string{"*/*;escaping=values"}, nil},
	{
		[]string{
			"application/*;proto=io.prometheus.client.MetricFamily;encoding=delimited,text/plain;escaping=dots",
		},
		nil,
	},
	{[]string{"*/plain;version=0.0.4;escaping=values,text/plain;escaping=dots"}, nil},
}

// TestGoldenNegotiation captures what the daemon's /metrics handler answers for each pair of
// negotiationCorpus: tests/testdata/metrics/negotiation.tsv holds, per row, the Accept and
// Accept-Encoding values (each list joined with |, - for no header), the Content-Type and the
// Content-Encoding. The corpus is swarm-scheduler-exporter's, whose captures came from a newer
// prometheus/common, plus rows on the wildcard types where the two negotiate differently. The
// handler is called directly, so a value keeps the spaces an HTTP client would trim.
func TestGoldenNegotiation(t *testing.T) {
	handler := newMetricsServer().Handler

	var table strings.Builder

	for _, row := range negotiationCorpus {
		request := httptest.NewRequestWithContext(
			t.Context(),
			http.MethodGet,
			"/metrics",
			http.NoBody,
		)
		if row.accept != nil {
			request.Header["Accept"] = row.accept
		}

		if row.acceptEncoding != nil {
			request.Header["Accept-Encoding"] = row.acceptEncoding
		}

		recorder := httptest.NewRecorder()
		handler.ServeHTTP(recorder, request)

		if recorder.Code != http.StatusOK {
			t.Fatalf("%q %q: status %d", row.accept, row.acceptEncoding, recorder.Code)
		}

		table.WriteString(strings.Join([]string{
			headerField(row.accept),
			headerField(row.acceptEncoding),
			recorder.Header().Get("Content-Type"),
			recorder.Header().Get("Content-Encoding"),
		}, "\t") + "\n")
	}

	goldencapture.Check(t, "tests/testdata/metrics/negotiation.tsv", []byte(table.String()))
}

// headerField is a header's values as a negotiation.tsv column.
func headerField(values []string) string {
	if values == nil {
		return "-"
	}

	return strings.Join(values, "|")
}

// TestGoldenRuntimeFamilies captures the families the Prometheus Go client adds beside the
// daemon's own, as the fifth scrape sees them: tests/testdata/metrics/runtime.prom holds every
// HELP and TYPE line of the process_ and promhttp_ families and the promhttp_ samples (the
// process_ samples vary with the run). promhttp.Handler instruments the default registry, which
// other tests scrape too, so the capture builds the same handler on a registry of its own with
// the default registry's process collector, and checks that the two serve the same families but
// the go_ ones (Q3).
func TestGoldenRuntimeFamilies(t *testing.T) {
	registry := prometheus.NewRegistry()
	registry.MustRegister(collectors.NewProcessCollector(collectors.ProcessCollectorOpts{}))
	handler := promhttp.InstrumentMetricHandler(
		registry,
		promhttp.HandlerFor(registry, promhttp.HandlerOpts{}),
	)

	var body string
	for range 5 {
		body = scrape(t, handler)
	}

	var exposition bytes.Buffer

	for line := range strings.Lines(body) {
		if strings.HasPrefix(line, "promhttp_") || strings.HasPrefix(line, "# HELP process_") ||
			strings.HasPrefix(
				line,
				"# TYPE process_",
			) || strings.HasPrefix(line, "# HELP promhttp_") ||
			strings.HasPrefix(line, "# TYPE promhttp_") {
			exposition.WriteString(line)
		}
	}

	if got, want := familyNames(
		body,
	), familyNames(
		scrape(t, newMetricsServer().Handler),
	); !slices.Equal(
		got,
		want,
	) {
		t.Fatalf("families %v, the default handler serves %v", got, want)
	}

	goldencapture.Check(t, "tests/testdata/metrics/runtime.prom", exposition.Bytes())
}

// scrape is the body handler serves for GET /metrics without headers.
func scrape(t *testing.T, handler http.Handler) string {
	t.Helper()

	recorder := httptest.NewRecorder()
	handler.ServeHTTP(
		recorder,
		httptest.NewRequestWithContext(t.Context(), http.MethodGet, "/metrics", http.NoBody),
	)

	return recorder.Body.String()
}

// familyNames lists the families of an exposition but the go_ and sda_ ones.
func familyNames(body string) []string {
	var names []string

	for line := range strings.Lines(body) {
		name, ok := strings.CutPrefix(line, "# TYPE ")
		if !ok {
			continue
		}

		name, _, _ = strings.Cut(name, " ")
		if !strings.HasPrefix(name, "go_") && !strings.HasPrefix(name, "sda_") {
			names = append(names, name)
		}
	}

	return names
}
