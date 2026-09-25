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

| Linter | Threshold | Note |
| --- | --- | --- |
| `gocyclo` | 15 | Cyclomatic complexity |
| `cyclop` | 15 | Same metric, different linter — both fire together |
| `gocognit` | 35 | Cognitive complexity |
| `funlen` | 50 statements | Lines are disabled (`lines: -1`) |

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

| Forbidden | Use instead |
| --- | --- |
| `github.com/sirupsen/logrus` | `github.com/leinardi/swarm-device-access/internal/logger` → `logger.L()` |
| `github.com/pkg/errors` | stdlib `errors` + `fmt.Errorf(...%w...)` |
| `github.com/instana/testify` | `github.com/stretchr/testify` |

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

## 18. Comments carry rationale; history goes in the commit

A comment says **why the code is the way it is** — the constraint, the failure it avoids, the
alternative that was rejected and what broke. It does not narrate what changed, when, or at whose
request. That belongs in the commit body, where `git log` and `git blame` can find it and where it
does not rot as the code moves.

```go
// Bad — history in the code.
// Changed after the daemon-reload bug report; used to reuse the processed map.

// Good — rationale in the code.
// A fresh map on purpose: daemon-reload has already wiped every BPF program, so
// containers the shared processed map remembers still need their rules re-applied.
fresh := make(map[string]time.Time)
```

The same rule is what makes `//nolint` explanations useful: say why the fix does not apply here,
not that the linter complained.

---

## 19. `time.Sleep` in tests: classify before you write one

A sleep is the right tool for exactly three of the five things tests use it for. Decide which of
these a site is *before* writing or converting it; the class dictates the shape.

**Positive eventual — never a sleep.** "Something another goroutine will do has happened": an
event was consumed, a container was inspected, a rule was applied. Poll with a deadline, never a
guessed sleep: a slow machine then costs milliseconds instead of flaking, and the timeout message
says which contract was broken rather than "unexpected nil". There is no shared `testsync` package
here. Inside `internal/daemon`, reuse `waitFor(t, cond)` from `internal/daemon/daemon_test.go`
(2-second deadline, fails with `condition not met before deadline`). Anywhere else, add a small
`waitFor`-style helper only when you convert a site that needs it — not speculatively. A
hand-rolled `for { … deadline … time.Sleep }` in a test body is this class too — convert it.

This class needs something *observable* to poll. Where the only honest observable is unexported,
prefer adding a small read-only seam on the production type over poking at internals — or, as the
daemon tests do, record calls in the fake (`recordingInspector.inspected`) and poll that.

**Negative assertion — a sleep, bounded and commented.** "Nothing happens": a deduplicated event
never re-applies, a filtered action never reaches `apply`. There is no condition to poll for; the
test gives the wrong behavior a bounded chance to appear and then asserts it did not. Say so in a
comment, so the next reader does not "fix" it into a `waitFor` that cannot exist.

**Real elapsed window — a sleep, and the duration is the point.** A dedup TTL, a backoff step, a
resubscribe cursor with nanosecond resolution. The duration is under test; shortening it changes
what is asserted. Name the window as a constant or express it as a multiple of the interval under
test (`2 * minBackoff`), never a bare literal chosen by feel.

**Ordering barrier with no quiescence signal — a sleep, and say why no seam exists.** "Let
`consumeEvents` drain the buffered message before cancelling the context". These are the ones worth
revisiting when a seam appears; the comment is what makes that possible.

**Poll tick inside an eventual-wait helper — already correct.** The sleep inside `waitFor` itself.
Leave it.

Two shapes are always wrong: a sleep whose comment says "give X time to Y" where Y is observable,
and a sleep added to make a flaky test pass without deciding which class it belongs to.

**Known follow-up.** `internal/daemon` has 8 `time.Sleep` calls in its tests, not yet classified
or converted:

- `daemon_test.go:142` — the poll tick inside `waitFor` (already correct);
- `daemon_test.go:460` and `events_test.go:208`, `:240`, `:271`, `:292`, `:349`, `:380` — each a
  goroutine that sleeps 20 ms and then cancels the context so `consumeEvents` returns. Each is
  either a positive eventual (poll the fake's `apply` record, then cancel) or a negative assertion
  (dedup: nothing else may be applied) — classify each one, then convert or comment it.

Do not add to that list; a new test uses the shape its class dictates.

---

## 20. Reuse before writing

Every helper below exists so the hand-written version of it is written once. Before adding a
logger, a path check, a wait or a metric guard, check whether one of these already answers the
question — and if it nearly does, extend it rather than forking it.

| Need | Use | Not |
| --- | --- | --- |
| Logging from any package | `logger.L()` (`internal/logger`) — lazily initialised, safe before `Configure` | `slog.Default()`, a package-level `slog.New`, or a logger threaded through only to log |
| Is this mount source under `/dev` | `processor.IsMountSource` | a fresh `strings.HasPrefix(path, "/dev")`, which also matches `/devops/…` |
| A wait in a loop that must stop on shutdown | `sleepCtx(ctx, d)` (`internal/daemon/events.go`) | a bare `time.Sleep` that ignores `ctx` |
| Reconnect backoff | `minBackoff`, `maxBackoff` and `nextBackoff` (`internal/daemon/events.go`) | new duration literals or a second doubling loop at the call site |
| Recording a metric from code that tests run without a registry | the `*observability.Recorder` methods (`RecordEvent`, `RecordRuleApplied`, `IncReloadReapply`, `IncDockerReconnect`, `ObserveApplyDuration`, `RecordContainerScanned`, `RecordContainerSkipped`, `AddDeviceFilesDiscovered`, `AddRuleFailures`, `AddDryRunSkips`) — each is nil-safe, so tests pass `nil` | an `if rec != nil` guard at the call site, or a fake recorder |
| Waiting in an `internal/daemon` test | `waitFor(t, cond)` (`internal/daemon/daemon_test.go`) | `time.Sleep` with a guessed duration (see §19) |

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
- [ ] Checked §20 for an existing helper before writing a new one
- [ ] Comments say why, not what changed — history is in the commit body (§18)
- [ ] Every `time.Sleep` in a test is classified per §19: a positive eventual polls with a
      deadline (`waitFor` in `internal/daemon`), and any surviving sleep says which of the other
      classes it is
