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
against the existing codebase in `cmd/` and `internal/`. §1–§17 follow from the linters
and the build, §18–§21 are review rules the linters cannot check.

golangci-lint is not on `PATH` in every environment; run it through pre-commit
(`pre-commit run golangci-lint-full --all-files`). Never commit with `--no-verify` —
fix the underlying issue instead.

Linters that are **disabled** in `.golangci.yaml`, so their rules do not apply:

| Disabled linter | Reason |
| --- | --- |
| `exhaustruct`, `exhaustruct_v5` | Requires every struct field to be set; too noisy for short-lived structs |
| `gomodguard` | Replaced by `gomodguard_v2`, which is enabled |
| `gochecknoglobals` | Package-level lookup tables (`keyShapes`, `knownLabels`) and the logger singleton are intentional |
| `nonamedreturns` | Named returns are allowed |
| `wsl` | Whitespace style is enforced by `gofumpt` instead |

The formatters (`gci`, `gofmt`, `gofumpt`, `goimports`, `golines`) run in the
`golangci-lint-fmt` hook and in CI.

---

## 1. Import grouping

Three groups, separated by blank lines — this is enforced by both `gci` (explicit
`sections: standard, default, prefix(github.com/leinardi/swarm-device-access)`) and
`goimports` (`local-prefixes: github.com/leinardi/swarm-device-access`)
simultaneously, and they must agree:

```go
import (
    // Group 1: stdlib
    "context"
    "errors"
    "fmt"

    // Group 2: third-party (everything that is NOT this module)
    "github.com/moby/moby/client"
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
err = secondThing() // = not :=
if err != nil { ... }
```

### 2b. Wrap errors with `%w` (`errorlint`)

Always wrap errors so callers can use `errors.Is`/`errors.As`:

```go
return FileSchema{}, fmt.Errorf("read config file %q: %w", path, err)
```

The prefix is a short, lowercase phrase naming the operation, not a full
sentence: no capital letters, no trailing period.

Use `errors.Is`/`errors.As` (or Go 1.26's `errors.AsType[T]`) for comparisons, never
`==` on error values — `err113` flags that too. Docker API errors are classified with
`cerrdefs.IsNotFound` (`github.com/containerd/errdefs`).

Prefer flat code with early returns; no `else` after a `return` (`revive`'s
`indent-error-flow`).

### 2c. Errors are sentinels, detail is wrapped (`err113`)

`err113` flags every `errors.New` inside a function body and every `fmt.Errorf`
without a `%w` verb — a static message included. Declare the error once as a
package-level sentinel and attach the runtime detail by wrapping it:

```go
var errGlobNotClean = errors.New("glob pattern is not a clean path")

return fmt.Errorf("%w: %q", errGlobNotClean, pattern)
return fmt.Errorf("%w: %w", ErrContainerGone, inspectErr)
```

Export a sentinel (`Err…`) when callers need `errors.Is`; unexported is fine for
package-internal use. Suppress with `//nolint:err113` only when no sentinel can fit,
and say why (§3).

### 2d. Aggregating multiple errors

Use `errors.Join`:

```go
var errs []error
for _, item := range items {
    processErr := process(item)
    if processErr != nil {
        errs = append(errs, fmt.Errorf("process %q: %w", item, processErr))
    }
}
return errors.Join(errs...)
```

### 2e. Ignoring errors explicitly

When an error return genuinely cannot be acted upon (writing a `/healthz` body to a
`ResponseWriter`, printing to stdout), assign it to the blank identifier:

```go
_, _ = resp.Write([]byte("ok"))
```

---

## 3. `nolint` directives

`nolintlint` enforces three things:

- **Specific**: name every linter — no bare `//nolint`
- **Explanation required**: every directive needs `// reason`
- **No unused**: remove directives when the code no longer triggers that linter

The explanation says why the fix does not apply here, not which rule fired (§19).

### Inline (same-line) — for a single statement or return

```go
switch msg.Action { //nolint:exhaustive // the stream is filtered to these four actions
```

### Preceding-line — for a function or type declaration

```go
//nolint:ireturn // processPinner is the seam that lets tests fake pidfds
func (p *Processor) processPinner() processPinner {
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

Test files are exempt from `funlen`, `gocognit`, `gocyclo`, `maintidx` and the
`cyclop` "calculated cyclomatic complexity" check; `internal/cgroup/` is exempt
from most of them too (§17).

---

## 5. Magic numbers (`mnd`)

Numbers 0, 1, 2, 3 are allowed everywhere. Any other literal integer/float in
an `argument`, `case`, `condition`, or `return` position needs a named constant.
Any literal used more than once, or that needs explaining, is a constant too:

```go
const (
    minBackoff      = 1 * time.Second
    maxBackoff      = 30 * time.Second
    shutdownTimeout = 5 * time.Second
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
when the signature is fixed by an interface (e.g., `slog.Handler`, which takes a
`slog.Record` by value).

The same applies to `rangeValCopy`: iterate large slices by index and take a
pointer (`item := &items[idx]`).

---

## 8. Line length (`lll`)

Max 140 characters. `golines` wraps automatically, but try to stay within
bounds when writing new code — especially long function signatures and struct
tags. Test files are exempt.

---

## 9. Forbidden packages (`depguard`)

| Forbidden | Use instead |
| --- | --- |
| `github.com/sirupsen/logrus` (rule `logger`; allowed only in `internal/logger`) | `github.com/leinardi/swarm-device-access/internal/logger` → `logger.L()` |
| `github.com/pkg/errors` (rule `forbidden-forks`) | stdlib `errors` + `fmt.Errorf(...%w...)` |
| `github.com/instana/testify` (rule `forbidden-forks`) | `github.com/stretchr/testify` |
| `github.com/docker/docker/…` (rule `docker-sdk`) | `github.com/moby/moby/client` + `github.com/moby/moby/api` (GO-2026-4887, GO-2026-4883) |

depguard only sees this repo's own imports; `make audit-deps` also fails when
`github.com/docker/docker` comes back transitively (`BANNED_GO_MODULES`).

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
- Comment style: gocritic's `whyNoLint` check is disabled, but all `//nolint`
  directives still need an explanation per nolintlint's `require-explanation` setting.

Doc comments:

- Every exported function, type, and variable has a doc comment beginning with
  the symbol name (`// IsMountSource reports whether path is /dev or a path under /dev/.`).
- Unexported symbols get one when their purpose is not obvious from the name.
- Inline comments explain *why*, not *what* (§19).
- Do not add doc comments or comment scaffolding to code you did not otherwise
  change, e.g. as a side effect of a bug fix.

---

## 12. Duplication (`dupl`)

Avoid copy-pasting blocks longer than ~100 tokens. Extract shared logic into a
helper. Test files are exempt from `dupl`.

---

## 13. Shadowing (`govet shadow`)

`govet` shadow detection is enabled. Avoid re-declaring variables with `:=`
when they shadow an outer-scope variable. Prefer distinct names or
restructuring to avoid shadows — this is why the codebase names errors after
their source (`subErr`, `streamErr`, `inspectErr`) rather than reusing `err`.

---

## 14. Variable naming (`varnamelen`)

Short variable names are fine in tight scopes (loop indices `i`, `k`, map
values `v`). `varnamelen` flags a name shorter than 3 characters whose last use
is more than 5 lines from its declaration (its defaults: `min-name-length: 3`,
`max-distance: 5`). Test files and `internal/cgroup/` are exempt.

**Specific rules that bite most often:**

- **Receivers are exempt**: `(g Global)`, `(c *coordinator)`, `(p *Processor)` — all fine.
- **Parameters are checked like locals.** A one-letter parameter passes in a
  three-line function and is flagged as soon as the body grows, so give
  parameters ≥ 3-char descriptive names from the start:

  ```go
  // Wrong — 's', 'c', 'l' are too short for params
  func ParseMode(s string) (Mode, error)
  func (g Global) Enabled(c Container) bool
  func parseGlobList(raw, l string) ([]string, error)

  // Right
  func ParseMode(modeStr string) (Mode, error)
  func (g Global) Enabled(cpol Container) bool
  func parseGlobList(raw, labelName string) ([]string, error)
  ```

- **Local variables** follow the same distance rule: a variable named `c` that
  is still used more than 5 lines later is flagged; rename it to reflect its type
  or role (`cont`, `cfg`, `cpol`).

Rule of thumb: if the name alone doesn't tell you what the variable holds,
make it longer.

---

## 15. `modernize` — no pointer-boxing helpers

The `modernize` linter (`newexpr` check) flags any function whose sole purpose
is to return a pointer to its argument — the generic `func ptr[T any](v T) *T`
included — at the declaration and at every call site. Go 1.26's `new` takes an
expression, so no helper is needed:

```go
// Wrong — flagged twice
func boolPtr(b bool) *bool { return &b }
cases := []struct{ enable *bool }{{enable: boolPtr(true)}}

// Right
cases := []struct{ enable *bool }{
    {enable: new(true)},
    {enable: new(false)},
    {enable: nil},
}
```

Taking the address of a local (`val, err := strconv.ParseBool(raw)`, then `&val`)
is fine too, and reads better when the value is computed or used more than once.

---

## 16. Constant strings (`goconst`)

String literals appearing 3+ times with length ≥ 2 should be extracted to a
named constant. Test files are exempt.

---

## 17. `internal/cgroup/` exemptions

Files matching `internal/cgroup/*.go` are NVIDIA-derived code kept close to
upstream. These files are exempt from: `cyclop`, `dupl`, `errcheck`,
`errorlint`, `forbidigo`, `funlen`, `gochecknoinits`, `gocognit`, `gocritic`,
`gocyclo`, `gosec`, `lll`, `mnd`, `nestif`, `nlreturn`, `revive`, `unparam`,
`varnamelen`. Do not add `//nolint` suppressions to these files when modifying
them unless absolutely necessary — the exclusion already covers the expected
violations.

---

## 18. Reuse before writing

Every helper below exists so the hand-written version of it is written once. Before adding a
logger, a path check, a wait or a metric guard, check whether one of these already answers the
question — and if it nearly does, extend it rather than forking it.

| Need | Use | Not |
| --- | --- | --- |
| Logging from any package | `logger.L()` (`internal/logger`) — lazily initialised, safe before `Configure` | `slog.Default()`, a package-level `slog.New`, or a logger threaded through only to log |
| Is this mount source under `/dev` | `processor.IsMountSource` | a fresh `strings.HasPrefix(path, "/dev")`, which also matches `/devops/…` |
| A wait in a loop that must stop on shutdown | `sleepCtx(ctx, d)` (`internal/daemon/events.go`) | a bare `time.Sleep` that ignores `ctx` |
| Reconnect backoff | `minBackoff`, `maxBackoff` and `nextBackoff` (`internal/daemon/events.go`) | new duration literals or a second doubling loop at the call site |
| Recording a metric from code that tests run without a registry | the `*observability.Recorder` methods (`RecordEvent`, `RecordRuleApplied`, `IncDockerReconnect`, …) — every one is nil-safe, so tests pass `nil` | an `if rec != nil` guard at the call site, or a fake recorder |
| Waiting for a condition in an `internal/daemon` test | `waitFor(t, cond)` / `waitForWithin(t, limit, cond)` (`internal/daemon/daemon_test.go`) | `time.Sleep` with a guessed duration, or another hand-rolled deadline loop (see §20) |

---

## 19. Comments carry rationale; history goes in the commit

A comment says **why the code is the way it is** — the constraint, the failure it avoids, the
alternative that was rejected and what broke. It does not narrate what changed, when, or at whose
request. That belongs in the commit body, where `git log` and `git blame` can find it and where it
does not rot as the code moves.

```go
// Bad — history in the code.
// Changed after the shutdown bug report; used to classify the stream error first.

// Good — rationale in the code.
// Checked before the error itself: once ctx is canceled the client can end the stream with a
// closed-body read error rather than context.Canceled, and that is a shutdown, not a stream failure.
if ctx.Err() != nil {
```

The same rule is what makes `//nolint` explanations useful: say why the fix does not apply here,
not that the linter complained.

---

## 20. Waiting in tests: classify before you write a sleep

Tests reach for a sleep for five different reasons, and only some of them justify one: decide
which of these a site is *before* writing or converting it — the class dictates the shape.

**Positive eventual — never a sleep.** "Something another goroutine will do has happened": an
event was consumed, a container was inspected, a rule was applied. Wait on the signal, or poll
with a deadline: a slow machine then costs milliseconds instead of flaking, and the failure names
the contract that was broken rather than "unexpected nil". There is no shared `testsync` package
here. Inside `internal/daemon`, reuse `waitFor(t, cond)` (2-second deadline, fails with
`condition not met before deadline`). Anywhere else, add a small `waitFor`-style helper only when
you convert a site that needs it — not speculatively. A hand-rolled `for { … deadline …
time.Sleep }` in a test body is this class too — convert it.

This class needs something *observable* to poll. Where the only honest observable is unexported,
prefer a small read-only seam on the production type over poking at internals — or, as the
daemon tests do, record calls in the fake (`recordingInspector.inspected`) and poll that.

**Negative assertion — a sleep, bounded and commented.** "Nothing happens": no further pass
starts, a reserved container is not retried. There is no condition to poll for; give the wrong
behavior a bounded window to appear, then assert it did not, and say so in a comment so the next
reader does not "fix" it into a `waitFor` that cannot exist.

**Real elapsed window — a sleep, and the duration is the point.** An establishment timeout the
event stream must outlive, a shared shutdown budget. Shortening it changes what is asserted. Name
the window as a constant or a multiple of the interval under test (`2 * testCallTimeout`), never
a bare literal chosen by feel.

**Ordering barrier with no quiescence signal — a sleep, and say why no seam exists.** These are
the ones worth revisiting when a seam appears; the comment is what makes that possible.

**Poll tick inside an eventual-wait helper — already correct.** The sleep inside
`waitForWithin`. Leave it.

Two shapes are always wrong: a sleep whose comment says "give X time to Y" where Y is observable,
and a sleep added to make a flaky test pass without deciding which class it belongs to.

**Known follow-up.** Most surviving sleeps already say their class in a comment (the real elapsed
windows in `internal/daemon/timeout_test.go` and `internal/launcher/launcher_test.go`, the negative
assertion in `internal/processor/serialize_test.go`). Not yet converted or commented:

- two hand-rolled deadline loops outside `internal/daemon`: `waitStuckConn` in
  `internal/launcher/launcher_test.go`, and the inspect wait in
  `TestReconcile_SerializesConfigLoadThroughApply` (`internal/processor/serialize_test.go`);
- the negative-assertion sleep in `TestCoordinator_ReservedPendingDoesNotSpin`
  (`internal/daemon/coordinator_test.go`), which has no comment saying so.

Do not add to that list; a new test uses the shape its class dictates.

---

## 21. Context

- A function that does I/O, calls Docker or waits takes `ctx context.Context` as its first
  parameter.
- The root context is created once, in `run()` (and `runLaunch()` for the launcher), with
  `signal.NotifyContext(context.Background(), syscall.SIGINT, syscall.SIGTERM)`; `defer` its cancel.
- A derived context's cancel is deferred, or handed to the one owner that ends it (`subscribe`
  returns the event stream's cancel to `consumeEvents`).
- Cleanup that must still happen during shutdown — a stop, a remove, a drain, the HTTP `Shutdown`
  — derives from `context.WithoutCancel(ctx)` under its own timeout, never from the context that
  is already done.
- Shutdown is not a failure: check `ctx.Err()` before treating an error as one, and do not log or
  count it (`consumeEvents` returns before counting a reconnect).
- A wait inside a loop that must stop on shutdown is `sleepCtx(ctx, d)` (§18), never a bare
  `time.Sleep`.

---

## What to avoid

- logrus outside `internal/logger`, `pkg/errors`, or `github.com/docker/docker` (§9).
- `log.Fatal` or `os.Exit` outside `main`: `main()` calls `os.Exit(run())` exactly once so
  deferred cleanup (cancel, `Close`) always runs.
- `interface{}` (§6) and pointer-boxing helpers (§15).
- A `//nolint` in `internal/cgroup/` for a linter that is already excluded there (§17).
- Designing for hypothetical requirements: no configurability, abstractions or helpers for
  features that do not exist yet.
- Skipping or suppressing pre-commit hooks (`--no-verify`).
- Adding comments to code you did not change (§11).

---

## Quick checklist before submitting Go code

- [ ] Imports in 3 groups: stdlib / third-party / local, alphabetical within each
- [ ] No `if err := f(); err != nil` — split to two lines
- [ ] All errors wrapped with `%w`; `errors.Is` / `cerrdefs.IsNotFound` for classification
- [ ] No `errors.New` or `%w`-less `fmt.Errorf` in a function body: wrap a package-level sentinel
- [ ] `any` not `interface{}`; `new(expr)`, not a pointer-boxing helper
- [ ] Numbers other than 0–3 extracted to named constants (non-test code)
- [ ] Each `//nolint` names specific linters and explains why the fix does not apply
- [ ] No `FIXME` comments
- [ ] `//go:build linux` first line in Linux-specific files
- [ ] Function statement count ≤ 50 (non-test, non-cgroup code)
- [ ] No shadowed variables
- [ ] Checked §18 for an existing helper before writing a new one
- [ ] Comments say why, not what changed — history is in the commit body (§19)
- [ ] Every `time.Sleep` in a test is classified per §20: a positive eventual polls with a
      deadline (`waitFor` in `internal/daemon`), and any surviving sleep says which of the other
      classes it is
- [ ] I/O takes `ctx` first; shutdown is not logged or counted as a failure (§21)
