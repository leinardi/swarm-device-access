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
	"go/ast"
	"go/parser"
	"go/token"
	"io/fs"
	"path/filepath"
	"strings"
	"testing"
)

func TestPublisher_GenerationsGrow(t *testing.T) {
	t.Parallel()

	store, publisher := NewStore(Runtime{})
	if store.Generation() != 1 {
		t.Fatalf("initial generation = %d, want 1", store.Generation())
	}

	gen := publisher.Publish(Runtime{DryRun: true})

	rt, snapGen := store.Snapshot()
	if gen != 2 || snapGen != 2 || !rt.DryRun {
		t.Errorf(
			"after Publish: returned %d, snapshot %d %+v; want 2, 2, dry-run",
			gen,
			snapGen,
			rt,
		)
	}
}

// TestPublisher_OnlyProcessorPublishes enforces that, outside tests, the
// config changes only through Processor.PublishAndReconcile: every other
// path would publish without taking the processor's lock or requesting a
// reconciliation pass.
func TestPublisher_OnlyProcessorPublishes(t *testing.T) {
	t.Parallel()

	root := filepath.Join("..", "..")
	fset := token.NewFileSet()

	var (
		offenders []string
		allowed   int
	)

	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}

		if entry.IsDir() {
			if strings.HasPrefix(entry.Name(), ".") && path != root {
				return filepath.SkipDir
			}

			return nil
		}

		if !strings.HasSuffix(path, ".go") || strings.HasSuffix(path, "_test.go") {
			return nil
		}

		file, parseErr := parser.ParseFile(fset, path, nil, 0)
		if parseErr != nil {
			return fmt.Errorf("parse %s: %w", path, parseErr)
		}

		ast.Inspect(file, func(node ast.Node) bool {
			call, ok := node.(*ast.CallExpr)
			if !ok {
				return true
			}

			sel, ok := call.Fun.(*ast.SelectorExpr)
			if !ok || sel.Sel.Name != "Publish" {
				return true
			}

			if filepath.ToSlash(filepath.Dir(path)) == "../../internal/processor" {
				allowed++
			} else {
				offenders = append(offenders, fset.Position(call.Pos()).String())
			}

			return true
		})

		return nil
	})
	if err != nil {
		t.Fatal(err)
	}

	// Guards the scan itself: it must see the one allowed call.
	if allowed != 1 {
		t.Errorf("found %d Publish calls in internal/processor, want 1", allowed)
	}

	if len(offenders) > 0 {
		t.Errorf("config published outside Processor.PublishAndReconcile: %v", offenders)
	}
}
