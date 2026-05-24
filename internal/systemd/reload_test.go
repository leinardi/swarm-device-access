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

package systemd

import (
	"testing"

	"github.com/godbus/dbus/v5"
)

func TestIsReloadCompleted(t *testing.T) {
	t.Parallel()

	tests := []struct {
		name string
		sig  *dbus.Signal
		want bool
	}{
		{
			name: "completion edge (active=false)",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{false},
			},
			want: true,
		},
		{
			name: "start edge (active=true)",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{true},
			},
			want: false,
		},
		{
			name: "nil signal",
			sig:  nil,
			want: false,
		},
		{
			name: "wrong signal name",
			sig: &dbus.Signal{
				Name: "org.freedesktop.systemd1.Manager.UnitNew",
				Body: []any{false},
			},
			want: false,
		},
		{
			name: "empty body",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{},
			},
			want: false,
		},
		{
			name: "non-boolean body",
			sig: &dbus.Signal{
				Name: reloadingFullName,
				Body: []any{"not-a-bool"},
			},
			want: false,
		},
	}

	for _, tc := range tests {
		t.Run(tc.name, func(t *testing.T) {
			t.Parallel()

			got := isReloadCompleted(tc.sig)
			if got != tc.want {
				t.Errorf("isReloadCompleted() = %v, want %v", got, tc.want)
			}
		})
	}
}
