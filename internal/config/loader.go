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

package config

import (
	"fmt"
	"os"

	"gopkg.in/yaml.v3"
)

// FileSchema mirrors the CLI flags that can be set via the config file.
// All fields are optional; zero values mean "not set in file".
// YAML tag names match the CLI flag names (kebab-case) for user-facing consistency.
//
//nolint:tagliatelle // kebab-case tags match CLI flag names intentionally for user-facing consistency
type FileSchema struct {
	LogFormat    string   `yaml:"log-format"`
	LogLevel     string   `yaml:"log-level"`
	LogTime      *bool    `yaml:"log-time"`
	DockerSocket string   `yaml:"docker-socket"`
	DryRun       *bool    `yaml:"dry-run"`
	PolicyMode   string   `yaml:"policy-mode"`
	DeviceAllow  []string `yaml:"device-allow"`
	DeviceDeny   []string `yaml:"device-deny"`
	MetricsAddr  string   `yaml:"metrics-addr"`
	DebugAddr    string   `yaml:"debug-addr"`
}

// LoadFile reads and parses the YAML config file at path.
// Returns a zero-value FileSchema without error when path is empty.
func LoadFile(path string) (FileSchema, error) {
	if path == "" {
		return FileSchema{}, nil
	}

	data, err := os.ReadFile(path)
	if err != nil {
		return FileSchema{}, fmt.Errorf("read config file %q: %w", path, err)
	}

	var cfg FileSchema

	err = yaml.Unmarshal(data, &cfg)
	if err != nil {
		return FileSchema{}, fmt.Errorf("parse config file %q: %w", path, err)
	}

	return cfg, nil
}
