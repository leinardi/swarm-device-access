---
name: go-style-guide
description: >
  Project-specific Go coding rules for swarm-device-access. Apply whenever writing,
  editing, or reviewing any .go file in this repository — new functions, new files,
  bug fixes, refactors, test additions. The rules here are enforced by golangci-lint
  (version 2, default: all linters) and the pre-commit hooks. Violations require
  manual fixup after the fact, so internalise them up-front instead. Use this skill
  proactively: consult it before generating Go code, not after lint fails.
---

# Go Style Guide — swarm-device-access

Rules derived from `.golangci.yaml` (golangci-lint v2, `default: all`) and verified
against the existing codebase in `cmd/` and `internal/`.

---

## 1. Import grouping

Three groups, separated by blank lines — this is enforced by both `gci` and
`goimports` simultaneously, and they must agree:

```go
import (
    // Group 1: stdlib
    "context"
    "errors"
    "fmt"

    // Group 2: third-party (everything that is NOT this module)
    "github.com/docker/docker/client"
    "github.com/prometheus/client_golang/prometheus"
    "golang.org/x/sys/unix"
    "gopkg.in/yaml.v3"

    // Group 3: local module (github.com/leinardi/swarm-device-access/...)
    "github.com/leinardi/swarm-device-access/internal/cgroup"
    "github.com/leinardi/swarm-device-access/internal/logger"
)
```

Within each group imports are sorted alphabetically. A blank line between groups
is required; no blank lines within a group. Getting this wrong triggers both
`gci` and `goimports`.

---

## 2. Error handling

### 2a. No inline error assignment in `if` (`noinlineerr`)

**Wrong:**

```go
if err := doSomething(); err != nil {
```

**Right:**

```go
err := doSomething()
if err != nil {
```

When a variable is already declared in the same scope, use `=` not `:=` for
the second and later assignments:

```go
err := firstThing()
if err != nil { ... }
err = secondThing()   // = not :=
if err != nil { ... }
```

### 2b. Wrap errors with `%w` (`errorlint`)

Always wrap errors so callers can use `errors.Is`/`errors.As`:

```go
return fmt.Errorf("inspect container %q: %w", id, err)
```

Use `errors.Is`/`errors.As` for comparisons, never `==` on error values.

### 2c. No dynamic `errors.New` content (`err113`)

`errors.New("static message")` is fine. `fmt.Errorf` with a dynamic value is
fine when the sentinel pattern is not needed. But wrapping a runtime value
inside what looks like a sentinel triggers `err113`. Add a nolint if unavoidable:

```go
return fmt.Errorf( //nolint:err113 // dynamic content includes the policy value
    "require-label %q: must be key=value format",
    policy,
)
```

### 2d. Aggregating multiple errors

Use `errors.Join`:

```go
var errs []error
for _, item := range items {
    if err := process(item); err != nil {
        errs = append(errs, fmt.Errorf("process %q: %w", item, err))
    }
}
return errors.Join(errs...)
```

---

## 3. `nolint` directives

`nolintlint` enforces three things:

- **Specific**: name every linter — no bare `//nolint`
- **Explanation required**: every directive needs `// reason`
- **No unused**: remove directives when the code no longer triggers that linter

### Inline (same-line) — for a single statement or return

```go
return fmt.Errorf( //nolint:err113 // dynamic content includes the policy value
    "...",
)
```

### Preceding-line — for a function or type declaration

```go
//nolint:cyclop,gocyclo // inherent: inspect + version-detect + path-resolve + apply
func processContainer(...) error {
```

### Multiple linters — comma-separated, no spaces

```go
//nolint:gocyclo,cyclop,gocognit // complexity is inherent: handles SIGHUP, reload, and logger update
```

Always name all linters that fire. If `gocyclo` AND `cyclop` both fire for a
complex function, suppress both. Same for `gocyclo`/`cyclop`/`gocognit` when
all three exceed their thresholds.

---

## 4. Complexity limits

| Linter     | Threshold     | Note                                               |
|------------|---------------|----------------------------------------------------|
| `gocyclo`  | 15            | Cyclomatic complexity                              |
| `cyclop`   | 15            | Same metric, different linter — both fire together |
| `gocognit` | 35            | Cognitive complexity                               |
| `funlen`   | 50 statements | Lines are disabled (`lines: -1`)                   |

Prefer extracting helpers over suppressing. When suppression is the right call
(e.g., a function that branches over many independent config fields), explain
why in the nolint comment.

---

## 5. Magic numbers (`mnd`)

Numbers 0, 1, 2, 3 are allowed everywhere. Any other literal integer/float in
an `argument`, `case`, `condition`, or `return` position needs a named constant:

```go
const (
    minBackoff      = 1 * time.Second
    maxBackoff      = 30 * time.Second   // 30 needs a const, not inline
    shutdownTimeout = 5 * time.Second    // 5 needs a const
)
```

`strings.SplitN` is excluded from mnd checks.

Test files (`_test.go`) are fully exempt from `mnd`.

---

## 6. Type aliases

Use `any` instead of `interface{}`. `gofmt` rewrites `interface{}` → `any`
automatically, but write `any` in new code to avoid the formatter changing
your diff.

---

## 7. Struct size (`gocritic hugeParam`)

Structs passed by value that are over ~80 bytes trigger `hugeParam`. Pass by
pointer instead — or add `//nolint:gocritic // <interface constraint reason>`
when the signature is fixed by an interface (e.g., `slog.Handler`).

---

## 8. Line length (`lll`)

Max 140 characters. `golines` wraps automatically, but try to stay within
bounds when writing new code — especially long function signatures and struct
tags.

---

## 9. Forbidden packages (`depguard`)

| Forbidden                    | Use instead                                                              |
|------------------------------|--------------------------------------------------------------------------|
| `github.com/sirupsen/logrus` | `github.com/leinardi/swarm-device-access/internal/logger` → `logger.L()` |
| `github.com/pkg/errors`      | stdlib `errors` + `fmt.Errorf(...%w...)`                                 |
| `github.com/instana/testify` | `github.com/stretchr/testify`                                            |

---

## 10. Build tags

All Linux-specific files start with `//go:build linux` as the very first line
(before the copyright block):

```go
//go:build linux

/*
 * Copyright ...
 */
```

This applies to all files under `cmd/`, `internal/cgroup/`, and
`internal/systemd/`. Test files mirror the build tag of the code they test.

---

## 11. Comments and `godox`

- `FIXME` is flagged by `godox`. Do not leave `FIXME` comments in committed code.
- `TODO` is allowed.
- Comment style: nolintlint's `whyNoLint` check is disabled, but all `//nolint`
  directives still need an explanation per the `require-explanation` setting.

---

## 12. Duplication (`dupl`)

Avoid copy-pasting blocks longer than ~100 tokens. Extract shared logic into a
helper. Test files are exempt from `dupl`.

---

## 13. Shadowing (`govet shadow`)

`govet` shadow detection is enabled. Avoid re-declaring variables with `:=`
when they shadow an outer-scope variable. Prefer distinct names or
restructuring to avoid shadows.

---

## 14. Variable naming (`varnamelen`)

Short variable names are fine in tight scopes (loop indices `i`, `k`, map
values `v`, single-letter receivers). In broader scopes, use names long enough
to be readable. Test files are exempt.

**Specific rules that bite most often:**

- **Receivers are exempt**: `(g Global)`, `(c Container)`, `(s *server)` — all fine.
- **Non-receiver function parameters are NOT exempt**, even in short functions.
  Use ≥ 3-char descriptive names:

  ```go
  // Wrong — 's', 'c', 'b' are too short for params
  func ParseMode(s string) (Mode, error)
  func (g Global) Enabled(c Container) bool
  func parseGlobList(raw, l string) ([]string, error)
  val, err := strconv.ParseBool(b)

  // Right
  func ParseMode(modeStr string) (Mode, error)
  func (g Global) Enabled(cpol Container) bool
  func parseGlobList(raw, labelName string) ([]string, error)
  val, err := strconv.ParseBool(raw)
  ```

- **Local variables that span multiple statements** are also checked. A variable
  named `c` that lives across 5+ lines will be flagged; rename to reflect its type
  or role (`cont`, `cfg`, `cpol`).

Rule of thumb: if the name alone doesn't tell you what the variable holds,
make it longer.

---

## 15. `modernize` — no pointer-boxing helpers

The `modernize` linter (`newexpr` check) flags helper functions whose sole
purpose is to return a pointer to a typed value:

```go
// Wrong — the linter flags both the declaration AND every call site
func boolPtr(b bool) *bool { return &b }
use: boolPtr(true), boolPtr(false)

// Also wrong (same pattern with other types)
func strPtr(s string) *string { return &s }
```

Fix: declare a local variable and take its address:

```go
// Right — in test tables
trueVal := true
falseVal := false
cases := []struct{ enable *bool }{
    {enable: &trueVal},
    {enable: &falseVal},
    {enable: nil},
}
```

For production code needing `*T` from a literal, assign then address:

```go
val := computeSomething()
cfg.Field = &val
```

A generic `func ptr[T any](v T) *T { return &v }` avoids the per-type helpers
but still fires `newexpr` in some linter versions — prefer the local-variable
pattern.

---

## 16. Constant strings (`goconst`)

String literals appearing 3+ times with length ≥ 2 should be extracted to a
named constant. Test files are exempt.

---

## 17. `internal/cgroup/` exemptions

Files matching `internal/cgroup/*.go` are NVIDIA-derived code kept close to
upstream. These files are exempt from: `cyclop`, `dupl`, `errcheck`,
`errorlint`, `forbidigo`, `funlen`, `gochecknoinits`, `gocognit`, `gocritic`,
`gocyclo`, `gosec`, `lll`, `mnd`, `nestif`, `nlreturn`, `revive`,
`stylecheck`, `unparam`, `varnamelen`. Do not add `//nolint` suppressions to
these files when modifying them unless absolutely necessary — the exclusion
already covers the expected violations.

---

## Quick checklist before submitting Go code

- [ ] Imports in 3 groups: stdlib / third-party / local, alphabetical within each
- [ ] No `if err := f(); err != nil` — split to two lines
- [ ] All errors wrapped with `%w`
- [ ] `any` not `interface{}`
- [ ] Numbers other than 0–3 extracted to named constants (non-test code)
- [ ] Each `//nolint` names specific linters and has `// explanation`
- [ ] No `FIXME` comments
- [ ] `//go:build linux` first line in Linux-specific files
- [ ] Function statement count ≤ 50 (non-cgroup code)
- [ ] No shadowed variables
