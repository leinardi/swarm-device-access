---
name: go-style-guide
description: >
  Go coding rules for swarm-device-access: the golangci-lint v2 (default: all)
  settings and how to satisfy them, //go:build linux tagging, the internal/cgroup
  (NVIDIA-derived) lint exemptions, the helpers to reuse, test-wait classes and
  context handling. Use when writing, editing or reviewing any .go file in
  swarm-device-access — new code, bug fixes, refactors or tests — before generating
  Go code, not after lint fails.
---

# Go Style Guide — swarm-device-access

Rules derived from `.golangci.yaml` (golangci-lint v2, `default: all`) and verified
against the existing codebase in `cmd/` and `internal/`. §1–§17 follow from the linters
and the build, §18–§21 are review rules the linters cannot check.

golangci-lint is not on `PATH` in every environment; run it through pre-commit (see
*Lint and test loop* at the end). Never commit with `--no-verify` — fix the underlying
issue instead.

Linters that are **disabled** in `.golangci.yaml`, so their rules do not apply:

| Disabled linter | Reason |
| --- | --- |
| `exhaustruct`, `exhaustruct_v5` | Requires every struct field to be set; too noisy for short-lived structs |
| `gomodguard` | Replaced by `gomodguard_v2`, which is enabled |
| `gochecknoglobals` | Package-level lookup tables (`keyShapes`, `knownLabels`) and the logger singleton are intentional |
| `nonamedreturns` | Named returns are allowed |
| `wsl` | The deprecated v4 linter; its successor `wsl_v5` stays enabled |

The formatters (`gci`, `gofmt`, `gofumpt`, `goimports`, `golines`) run in the
`golangci-lint-fmt` hook and in CI.

---

## 1. Import grouping

Three groups separated by blank lines — stdlib, third-party, then local
`github.com/leinardi/swarm-device-access/...` — alphabetical within each group. `gci` and
`goimports` both enforce it and the `golangci-lint-fmt` hook rewrites it. The one alias in
use is `cerrdefs` for `github.com/containerd/errdefs`.

---

## 2. Error handling

### 2a. No inline error assignment in `if` (`noinlineerr`)

```go
// Wrong
if err := doSomething(); err != nil {

// Right
err := doSomething()
if err != nil {
    return err
}

err = secondThing() // = not := for the second and later assignments
```

### 2b. Wrap errors with `%w` (`errorlint`)

```go
return FileSchema{}, fmt.Errorf("read config file %q: %w", path, err)
```

The prefix is a short, lowercase phrase naming the operation, not a full sentence: no
capital letters, no trailing period. Compare with `errors.Is`/`errors.As` (or the generic
`errors.AsType[T]`), never `==` on error values. Docker API errors are classified with
`cerrdefs.IsNotFound` (`github.com/containerd/errdefs`). Prefer flat code with early
returns; no `else` after a `return` (`revive`'s `indent-error-flow`).

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

Use `errors.Join` over a slice of wrapped errors.

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

The explanation says why the fix does not apply here, not which rule fired (§19). A
statement gets an inline directive; a function or type gets it on the preceding line:

```go
//nolint:gocritic // hugeParam: settings is passed by value on purpose, the result must not share it
func mergeSettings(defaults settings, cliSet map[string]bool, file config.FileSchema) settings {
```

Multiple linters are comma-separated with no spaces. Name all that fire: `gocyclo` and
`cyclop` measure the same thing, and `gocognit` often joins them.

---

## 4. Complexity limits

| Linter | Threshold | Note |
| --- | --- | --- |
| `gocyclo` | 15 | Cyclomatic complexity |
| `cyclop` | 15 | Same metric, different linter — both fire together |
| `gocognit` | 35 | Cognitive complexity |
| `funlen` | 50 statements | Lines are disabled (`lines: -1`) |

Prefer extracting helpers over suppressing; when suppression is the right call, the
nolint comment says why. Test files are exempt from `funlen`, `gocognit`, `gocyclo`, `maintidx` and the
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

`strings.SplitN` is excluded from mnd checks. Test files are fully exempt from `mnd`.

---

## 6. Type aliases

Use `any` instead of `interface{}`; `gofmt`'s rewrite rule changes it anyway.

---

## 7. Struct size (`gocritic hugeParam`)

Structs over ~80 bytes passed by value trigger `hugeParam`. Pass by pointer — or suppress
when an interface fixes the signature (`slog.Handler` takes a `slog.Record` by value) or a
copy is the point (§3). The same applies to `rangeValCopy`: iterate large slices by index
and take a pointer (`item := &items[idx]`).

---

## 8. Line length (`lll`)

Max 140 characters. `golines` wraps automatically. Test files are exempt.

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

- `FIXME` is flagged by `godox`. `TODO` is allowed.
- gocritic's `whyNoLint` check is disabled, but every `//nolint` still needs an
  explanation (`require-explanation`).
- Every exported function, type, and variable has a doc comment beginning with
  the symbol name (`// IsMountSource reports whether path is /dev or a path under /dev/.`).
- Unexported symbols get one when their purpose is not obvious from the name.
- Inline comments explain *why*, not *what* (§19).
- Do not add doc comments or comment scaffolding to code you did not otherwise
  change, e.g. as a side effect of a bug fix.

---

## 12. Duplication (`dupl`)

Avoid copy-pasting blocks longer than ~100 tokens (`threshold: 100`). Test files are
exempt.

---

## 13. Shadowing (`govet shadow`)

`govet` shadow detection is enabled. Name errors after their source rather than reusing
`err` in a nested scope: `subErr`, `streamErr`, `inspectErr`.

---

## 14. Variable naming (`varnamelen`)

`varnamelen` flags a name shorter than 3 characters whose last use is more than 5 lines
from its declaration (defaults: `min-name-length: 3`, `max-distance: 5`). Test files and
`internal/cgroup/` are exempt.

- **Receivers are exempt**: `(g Global)`, `(c *coordinator)`, `(p *Processor)` are fine.
- **Parameters are checked like locals.** A one-letter parameter passes in a three-line
  function and is flagged as soon as the body grows, so name them from the start:

  ```go
  // Wrong
  func ParseMode(s string) (Mode, error)

  // Right
  func ParseMode(modeStr string) (Mode, error)
  ```

Local variables follow the same distance rule; rename them to reflect their type or role
(`cont`, `cfg`, `cpol`).

---

## 15. `modernize` — no pointer-boxing helpers

The `modernize` linter (`newexpr` check) flags any function whose sole purpose
is to return a pointer to its argument — the generic `func ptr[T any](v T) *T`
included — at the declaration and at every call site. The Go version in `go.mod` lets
`new` take an expression, so no helper is needed:

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

String literals appearing 3+ times with length ≥ 2 become a named constant. Test files
are exempt.

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

Tests reach for a sleep for five different reasons, and only some of them justify one. A
positive eventual (something another goroutine will do) never sleeps: inside
`internal/daemon` it polls with `waitFor` (§18); every other sleep says in a comment which
class it is.

Writing, converting or reviewing a wait or a sleep in a test? Read
[references/test-waits.md](references/test-waits.md) first: it defines the five classes and
the shape each one takes.

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
- [ ] Every `time.Sleep` in a test is classified per §20 and
      [references/test-waits.md](references/test-waits.md): a positive eventual polls with a
      deadline (`waitFor` in `internal/daemon`), and any surviving sleep says which of the other
      classes it is
- [ ] I/O takes `ctx` first; shutdown is not logged or counted as a failure (§21)

## Lint and test loop

1. Run `pre-commit run golangci-lint-fmt --files <changed .go files>` and
   `pre-commit run golangci-lint-full --files <changed .go files>` (or `--all-files`).
2. Run `make go-vet` and `make go-test` (Linux only; on another host run the tests in a Linux
   container, as AGENTS.md shows).
3. Fix each report and re-run from step 1 until all of them are clean.
4. Check `git status`: the formatter hook rewrites files in place, so review and keep its changes.
