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

package toolchain_test

import (
	"fmt"
	"go/ast"
	"go/build/constraint"
	"go/parser"
	"go/token"
	"io/fs"
	"path/filepath"
	"slices"
	"strconv"
	"strings"
	"testing"

	"github.com/leinardi/swarm-device-access/test/internal/toolgate"
)

// dockerClientPackage is the Docker SDK. Untagged tests import it for its types, to build fakes;
// only the integration suite may construct a client that talks to a daemon.
const dockerClientPackage = "github.com/moby/moby/client"

// dockerClientConstructors are the SDK functions that build a client or its daemon connection.
var dockerClientConstructors = []string{"New", "NewClientWithOpts", "FromEnv"}

// dockerTags are the build tags that Docker-dependent tests sit behind: the integration suite and
// the engine-wire captures.
var dockerTags = []string{"integration", "wirecapture"}

// testFile is one parsed _test.go file of the repository.
type testFile struct {
	rel    string
	file   *ast.File
	tagged bool
}

// TestUntaggedTestsAvoidDocker keeps Docker out of the untagged go test run that make go-test and
// the test CI job make: no untagged test file constructs a Docker client or runs docker itself.
// Docker tests sit behind the integration tag and run only in make go-test-integration.
func TestUntaggedTestsAvoidDocker(t *testing.T) {
	t.Parallel()

	untagged := 0

	for _, file := range parseTestFiles(t) {
		if file.tagged {
			continue
		}

		untagged++

		for _, problem := range dockerUses(file.file) {
			t.Errorf("%s is untagged and %s", file.rel, problem)
		}
	}

	if untagged == 0 {
		t.Fatal("no untagged test file found; the check would pass on nothing")
	}
}

// TestToolGateListIsClosed keeps the tools a meta-test may skip on to droast and the shellcheck
// its required bridge runs: toolgate's list is exactly those, every gated test names a toolgate
// constant, and no test looks a tool up on its own, which would be a skip CI cannot turn into a
// failure.
func TestToolGateListIsClosed(t *testing.T) {
	t.Parallel()

	if !slices.Equal(toolgate.Tools, []string{toolgate.Droast, toolgate.ShellCheck}) {
		t.Errorf("toolgate.Tools = %v, want exactly droast and shellcheck", toolgate.Tools)
	}

	for _, file := range parseTestFiles(t) {
		if len(execCalls(file.file, "LookPath")) > 0 {
			t.Errorf("%s looks a tool up itself; gate it through toolgate.Lookup", file.rel)
		}

		ast.Inspect(file.file, func(node ast.Node) bool {
			call, ok := node.(*ast.CallExpr)
			if !ok || !isSelector(call.Fun, "toolgate", "Lookup") || len(call.Args) != 2 {
				return true
			}

			if selector, isSel := call.Args[1].(*ast.SelectorExpr); !isSel ||
				!isIdent(selector.X, "toolgate") {
				t.Errorf("%s gates on a tool that is not a toolgate constant", file.rel)
			}

			return true
		})
	}
}

// TestDockerCheckRecognizesItsTargets is the negative control of the checks above.
func TestDockerCheckRecognizesItsTargets(t *testing.T) {
	t.Parallel()

	for source, want := range map[string]int{
		`package sample
import osexec "os/exec"
func run() { _ = osexec.Command("docker", "ps"); _, _ = osexec.LookPath("node") }
`: 1,
		`package sample
import dockerclient "github.com/moby/moby/client"
func run() { _, _ = dockerclient.New(dockerclient.FromEnv) }
`: 2,
		`package sample
import "github.com/moby/moby/client"
func run() { _, _ = client.NewClientWithOpts() }
`: 1,
		`package sample
import "github.com/moby/moby/client"
var _ client.APIClient
`: 0,
	} {
		file, err := parser.ParseFile(
			token.NewFileSet(),
			"sample_test.go",
			source,
			parser.ParseComments,
		)
		if err != nil {
			t.Fatal(err)
		}

		if got := dockerUses(file); len(got) != want {
			t.Errorf("%d Docker uses found, want %d, in:\n%s", len(got), want, source)
		}
	}

	file, err := parser.ParseFile(token.NewFileSet(), "sample_test.go",
		"package sample\nimport \"os/exec\"\nfunc run() { _, _ = exec.LookPath(\"x\") }\n",
		parser.ParseComments)
	if err != nil {
		t.Fatal(err)
	}

	if len(execCalls(file, "LookPath")) != 1 {
		t.Error("exec.LookPath was not recognized")
	}

	for constraintLine, tagged := range map[string]bool{
		"//go:build integration":                 true,
		"//go:build integration && linux":        true,
		"//go:build linux":                       false,
		"//go:build linux && fixturecapture":     false,
		"//go:build !integration":                false,
		"//go:build integration || linux":        false,
		"//go:build (integration || x) && linux": false,
	} {
		sample, parseErr := parser.ParseFile(token.NewFileSet(), "sample_test.go",
			constraintLine+"\n\npackage sample\n", parser.ParseComments)
		if parseErr != nil {
			t.Fatal(parseErr)
		}

		if hasBuildTag(sample) != tagged {
			t.Errorf("%q: read as tagged %v, want %v", constraintLine, !tagged, tagged)
		}
	}

	if hasBuildTag(file) {
		t.Error("a file without a //go:build line was read as tagged")
	}
}

// dockerUses lists what in file reaches a Docker daemon: a docker command, or a call to a Docker
// SDK constructor.
func dockerUses(file *ast.File) []string {
	var uses []string

	for _, call := range execCalls(file, "Command", "CommandContext") {
		if slices.ContainsFunc(call.Args, isLiteral("docker")) {
			uses = append(uses, "runs docker")
		}
	}

	local := importName(file, dockerClientPackage, "client")
	if local == "" {
		return uses
	}

	ast.Inspect(file, func(node ast.Node) bool {
		selector, ok := node.(*ast.SelectorExpr)
		if ok && isIdent(selector.X, local) &&
			slices.Contains(dockerClientConstructors, selector.Sel.Name) {
			uses = append(uses, "uses the Docker client constructor "+selector.Sel.Name)
		}

		return true
	})

	return uses
}

func parseTestFiles(t *testing.T) []testFile {
	t.Helper()

	root := toolgate.RepoRoot(t)
	fset := token.NewFileSet()

	var files []testFile

	err := filepath.WalkDir(root, func(path string, entry fs.DirEntry, walkErr error) error {
		if walkErr != nil {
			return walkErr
		}

		if entry.IsDir() {
			switch entry.Name() {
			case ".git", "dist", "testdata":
				return filepath.SkipDir
			}

			return nil
		}

		if !strings.HasSuffix(path, "_test.go") {
			return nil
		}

		file, err := parser.ParseFile(fset, path, nil, parser.ParseComments)
		if err != nil {
			return fmt.Errorf("parse: %w", err)
		}

		rel, err := filepath.Rel(root, path)
		if err != nil {
			return fmt.Errorf("relative path of %s: %w", path, err)
		}

		files = append(
			files,
			testFile{rel: filepath.ToSlash(rel), file: file, tagged: hasBuildTag(file)},
		)

		return nil
	})
	if err != nil {
		t.Fatal(err)
	}

	return files
}

// hasBuildTag reports whether the file builds only with a Docker tag: its //go:build constraint is
// false under every assignment of its other tags once the Docker tags are false. A file with no
// constraint, or one such as linux, is compiled by an untagged go test somewhere.
func hasBuildTag(file *ast.File) bool {
	for _, group := range file.Comments {
		if group.Pos() > file.Package {
			break
		}

		for _, comment := range group.List {
			if !constraint.IsGoBuild(comment.Text) {
				continue
			}

			expr, err := constraint.Parse(comment.Text)
			if err != nil {
				return false
			}

			return !satisfiableWithoutDocker(expr)
		}
	}

	return false
}

func satisfiableWithoutDocker(expr constraint.Expr) bool {
	var others []string

	expr.Eval(func(tag string) bool {
		if !slices.Contains(dockerTags, tag) && !slices.Contains(others, tag) {
			others = append(others, tag)
		}

		return false
	})

	for assignment := range 1 << len(others) {
		if expr.Eval(func(tag string) bool {
			index := slices.Index(others, tag)

			return index >= 0 && assignment&(1<<index) != 0
		}) {
			return true
		}
	}

	return false
}

// importName returns the name under which file imports path, or "" when it does not.
func importName(file *ast.File, path, defaultName string) string {
	for _, spec := range file.Imports {
		imported, _ := strconv.Unquote(spec.Path.Value)
		if imported != path {
			continue
		}

		if spec.Name != nil {
			return spec.Name.Name
		}

		return defaultName
	}

	return ""
}

// execCalls returns the calls to os/exec functions with the given names, under whatever name the
// file imports os/exec.
func execCalls(file *ast.File, names ...string) []*ast.CallExpr {
	local := importName(file, "os/exec", "exec")
	if local == "" {
		return nil
	}

	var calls []*ast.CallExpr

	ast.Inspect(file, func(node ast.Node) bool {
		call, ok := node.(*ast.CallExpr)
		if !ok {
			return true
		}

		for _, name := range names {
			if isSelector(call.Fun, local, name) {
				calls = append(calls, call)
			}
		}

		return true
	})

	return calls
}

func isSelector(expr ast.Expr, pkg, name string) bool {
	selector, ok := expr.(*ast.SelectorExpr)

	return ok && selector.Sel.Name == name && isIdent(selector.X, pkg)
}

func isIdent(expr ast.Expr, name string) bool {
	ident, ok := expr.(*ast.Ident)

	return ok && ident.Name == name
}

func isLiteral(value string) func(ast.Expr) bool {
	return func(expr ast.Expr) bool {
		literal, ok := expr.(*ast.BasicLit)
		if !ok || literal.Kind != token.STRING {
			return false
		}

		unquoted, err := strconv.Unquote(literal.Value)

		return err == nil && unquoted == value
	}
}
