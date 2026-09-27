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
	"bytes"
	"errors"
	"fmt"
	"io"
	"os"
	"slices"

	"gopkg.in/yaml.v3"

	"github.com/leinardi/swarm-device-access/internal/policy"
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

var (
	errShape          = errors.New("wrong shape")
	errMergeKey       = errors.New("YAML merge keys are not supported")
	errMultipleDocs   = errors.New("multiple YAML documents")
	errUnknownValue   = errors.New("unknown value")
	errAliasCycle     = errors.New("alias cycle")
	errDocumentNotMap = errors.New("the document must be a mapping")
)

// LogFormats and LogLevels are the accepted log-format and log-level values.
//
//nolint:gochecknoglobals // read-only enum lists
var (
	LogFormats = []string{"text", "json", "plain"}
	LogLevels  = []string{"debug", "info", "warn", "error"}
)

// keyShape is what a config key's value must look like.
type keyShape int

const (
	shapeBool keyShape = iota
	shapeNonEmptyString
	shapeString
	shapeNonEmptyStringSeq
)

func (s keyShape) String() string {
	switch s {
	case shapeBool:
		return "a boolean (true or false)"
	case shapeNonEmptyString:
		return "a non-empty string"
	case shapeString:
		return "a string (empty disables it)"
	case shapeNonEmptyStringSeq:
		return "a list of non-empty strings"
	default:
		return "unknown"
	}
}

// keyShapes lists every key the file may set. An explicit null never
// matches a shape, so it cannot silently stand for "not set" and fall back
// to the flag's value.
//
//nolint:gochecknoglobals // read-only table
var keyShapes = map[string]keyShape{
	"dry-run":       shapeBool,
	"log-time":      shapeBool,
	"policy-mode":   shapeNonEmptyString,
	"log-format":    shapeNonEmptyString,
	"log-level":     shapeNonEmptyString,
	"docker-socket": shapeNonEmptyString,
	"metrics-addr":  shapeString,
	"debug-addr":    shapeString,
	"device-allow":  shapeNonEmptyStringSeq,
	"device-deny":   shapeNonEmptyStringSeq,
}

// LoadFile reads and parses the YAML config file at path.
// Returns a zero-value FileSchema without error when path is empty.
//
// The file is checked strictly, in two passes over the same bytes. The
// first walks the parsed nodes (following aliases) and requires every
// known key to have exactly its shape: yaml.v3 would otherwise turn a
// quoted "yes" into true, an explicit null into "not set", and [null] into
// [""], which reads as allow-all. Merge keys are refused, since they could
// smuggle a value past that check. The second decodes with unknown keys
// rejected and requires the file to hold a single document. Enum values
// are validated last.
func LoadFile(path string) (FileSchema, error) {
	if path == "" {
		return FileSchema{}, nil
	}

	data, err := os.ReadFile(path)
	if err != nil {
		return FileSchema{}, fmt.Errorf("read config file %q: %w", path, err)
	}

	var doc yaml.Node

	err = yaml.Unmarshal(data, &doc)
	if err != nil {
		return FileSchema{}, fmt.Errorf("parse config file %q: %w", path, err)
	}

	if isEmptyDocument(&doc) {
		// Still exactly one document: "---" followed by a second document
		// is not an empty file.
		err = skipFirstDocument(data)
		if err != nil {
			return FileSchema{}, fmt.Errorf("config file %q: %w", path, err)
		}

		return FileSchema{}, nil
	}

	err = checkShapes(&doc)
	if err != nil {
		return FileSchema{}, fmt.Errorf("config file %q: %w", path, err)
	}

	cfg, err := decodeStrict(data)
	if err != nil {
		return FileSchema{}, fmt.Errorf("config file %q: %w", path, err)
	}

	err = cfg.validateEnums()
	if err != nil {
		return FileSchema{}, fmt.Errorf("config file %q: %w", path, err)
	}

	return cfg, nil
}

// decodeStrict is the second pass: unknown keys are errors and the file
// must hold exactly one document.
func decodeStrict(data []byte) (FileSchema, error) {
	var cfg FileSchema

	dec := yaml.NewDecoder(bytes.NewReader(data))
	dec.KnownFields(true)

	err := dec.Decode(&cfg)
	if err != nil {
		return FileSchema{}, fmt.Errorf("decode: %w", err)
	}

	err = singleDocument(dec)
	if err != nil {
		return FileSchema{}, err
	}

	return cfg, nil
}

// singleDocument requires dec, past its first document, to be at the end
// of the input.
func singleDocument(dec *yaml.Decoder) error {
	var rest yaml.Node

	err := dec.Decode(&rest)

	switch {
	case errors.Is(err, io.EOF):
		return nil
	case err != nil:
		return fmt.Errorf("parse trailing content: %w", err)
	default:
		return errMultipleDocs
	}
}

// skipFirstDocument checks that data holds at most one document.
func skipFirstDocument(data []byte) error {
	dec := yaml.NewDecoder(bytes.NewReader(data))

	var first yaml.Node

	err := dec.Decode(&first)
	if errors.Is(err, io.EOF) {
		return nil
	}

	if err != nil {
		return fmt.Errorf("decode: %w", err)
	}

	return singleDocument(dec)
}

// isEmptyDocument reports whether the first document is empty: no content
// at all, or a bare "---" (an implicit null).
func isEmptyDocument(doc *yaml.Node) bool {
	if doc.Kind == 0 || len(doc.Content) == 0 {
		return true
	}

	root := doc.Content[0]

	return root.Kind == yaml.ScalarNode && root.Tag == "!!null" && root.Value == ""
}

// checkShapes is the first pass over the parsed document.
func checkShapes(doc *yaml.Node) error {
	root, err := deref(doc.Content[0], map[*yaml.Node]bool{})
	if err != nil {
		return err
	}

	if root.Kind != yaml.MappingNode {
		return errDocumentNotMap
	}

	for idx := 0; idx+1 < len(root.Content); idx += 2 {
		keyNode, valueNode := root.Content[idx], root.Content[idx+1]

		if keyNode.Tag == "!!merge" || keyNode.Value == "<<" {
			return errMergeKey
		}

		shape, known := keyShapes[keyNode.Value]
		if !known {
			continue // the strict decode reports it as unknown
		}

		value, derefErr := deref(valueNode, map[*yaml.Node]bool{})
		if derefErr != nil {
			return fmt.Errorf("key %q: %w", keyNode.Value, derefErr)
		}

		if !hasShape(value, shape) {
			return fmt.Errorf("key %q must be %s: %w", keyNode.Value, shape, errShape)
		}
	}

	return nil
}

// hasShape reports whether node (aliases already followed) has shape.
func hasShape(node *yaml.Node, shape keyShape) bool {
	switch shape {
	case shapeBool:
		return node.Kind == yaml.ScalarNode && node.Tag == "!!bool"
	case shapeNonEmptyString:
		return node.Kind == yaml.ScalarNode && node.Tag == "!!str" && node.Value != ""
	case shapeString:
		return node.Kind == yaml.ScalarNode && node.Tag == "!!str"
	case shapeNonEmptyStringSeq:
		if node.Kind != yaml.SequenceNode {
			return false
		}

		for _, elem := range node.Content {
			item, err := deref(elem, map[*yaml.Node]bool{})
			if err != nil || !hasShape(item, shapeNonEmptyString) {
				return false
			}
		}

		return true
	default:
		return false
	}
}

// deref follows aliases to the node they name.
func deref(node *yaml.Node, visited map[*yaml.Node]bool) (*yaml.Node, error) {
	for node.Kind == yaml.AliasNode {
		if visited[node] {
			return nil, errAliasCycle
		}

		visited[node] = true
		node = node.Alias
	}

	return node, nil
}

// validateEnums checks the enumerated values the file sets.
func (f *FileSchema) validateEnums() error {
	return ValidateEnums(f.LogFormat, f.LogLevel, f.PolicyMode, "key")
}

// ValidateEnums checks log-format, log-level and policy-mode values;
// empty values are not set and pass. source names what set them in the
// error ("key" for the file, "flag" for the effective settings).
func ValidateEnums(logFormat, logLevel, policyMode, source string) error {
	if logFormat != "" && !slices.Contains(LogFormats, logFormat) {
		return fmt.Errorf(
			"%s %q: %w %q, must be one of %v",
			source,
			"log-format",
			errUnknownValue,
			logFormat,
			LogFormats,
		)
	}

	if logLevel != "" && !slices.Contains(LogLevels, logLevel) {
		return fmt.Errorf(
			"%s %q: %w %q, must be one of %v",
			source,
			"log-level",
			errUnknownValue,
			logLevel,
			LogLevels,
		)
	}

	if policyMode != "" {
		_, err := policy.ParseMode(policyMode)
		if err != nil {
			return fmt.Errorf("%s %q: %w", source, "policy-mode", err)
		}
	}

	return nil
}
