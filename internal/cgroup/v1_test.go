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

package cgroup

import (
	"os"
	"strings"
	"testing"
)

func TestScanMountInfoV1(t *testing.T) {
	cases := []struct {
		fixture    string
		wantPrefix string
		wantMount  string
		wantErrSub string
	}{
		{
			fixture:    "testdata/mountinfo_v1.txt",
			wantPrefix: "/",
			wantMount:  "/sys/fs/cgroup/devices",
		},
		{
			fixture:    "testdata/mountinfo_v1_prefix.txt",
			wantPrefix: "/docker/abc123",
			wantMount:  "/sys/fs/cgroup/devices",
		},
		{
			fixture:    "testdata/mountinfo_no_match.txt",
			wantErrSub: "no cgroup filesystem mounted",
		},
		{
			fixture:    "testdata/mountinfo_malformed.txt",
			wantErrSub: "malformed mountinfo",
		},
		// v2-only mountinfo should not match v1 parser
		{
			fixture:    "testdata/mountinfo_v2.txt",
			wantErrSub: "no cgroup filesystem mounted",
		},
	}

	for _, tc := range cases {
		t.Run(tc.fixture, func(t *testing.T) {
			f, err := os.Open(tc.fixture)
			if err != nil {
				t.Fatalf("open fixture: %v", err)
			}
			defer f.Close()

			gotPrefix, gotMount, err := scanMountInfoV1(f, tc.fixture)
			if tc.wantErrSub != "" {
				if err == nil {
					t.Fatalf("expected error containing %q, got nil", tc.wantErrSub)
				}

				if !strings.Contains(err.Error(), tc.wantErrSub) {
					t.Errorf("error %q does not contain %q", err.Error(), tc.wantErrSub)
				}

				return
			}

			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}

			if gotPrefix != tc.wantPrefix {
				t.Errorf("prefix = %q, want %q", gotPrefix, tc.wantPrefix)
			}

			if gotMount != tc.wantMount {
				t.Errorf("mount = %q, want %q", gotMount, tc.wantMount)
			}
		})
	}
}

func TestScanProcCgroupV1(t *testing.T) {
	cases := []struct {
		fixture    string
		prefix     string
		wantRoot   string
		wantErrSub string
	}{
		{
			fixture:  "testdata/cgroup_v1.txt",
			prefix:   "/",
			wantRoot: "/docker/abc123def456",
		},
		{
			fixture:  "testdata/cgroup_v1.txt",
			prefix:   "/docker",
			wantRoot: "/abc123def456",
		},
		// v2-only file has no devices subsystem
		{
			fixture:    "testdata/cgroup_v2.txt",
			prefix:     "/",
			wantErrSub: "no devices cgroup entries found",
		},
		{
			fixture:    "testdata/cgroup_no_match.txt",
			prefix:     "/",
			wantErrSub: "no devices cgroup entries found",
		},
		{
			fixture:    "testdata/cgroup_malformed.txt",
			prefix:     "/",
			wantErrSub: "malformed cgroup entry",
		},
	}

	for _, tc := range cases {
		t.Run(tc.fixture+"/prefix="+tc.prefix, func(t *testing.T) {
			f, err := os.Open(tc.fixture)
			if err != nil {
				t.Fatalf("open fixture: %v", err)
			}
			defer f.Close()

			got, err := scanProcCgroupV1(f, tc.fixture, tc.prefix)
			if tc.wantErrSub != "" {
				if err == nil {
					t.Fatalf("expected error containing %q, got nil", tc.wantErrSub)
				}

				if !strings.Contains(err.Error(), tc.wantErrSub) {
					t.Errorf("error %q does not contain %q", err.Error(), tc.wantErrSub)
				}

				return
			}

			if err != nil {
				t.Fatalf("unexpected error: %v", err)
			}

			if got != tc.wantRoot {
				t.Errorf("root = %q, want %q", got, tc.wantRoot)
			}
		})
	}
}
