# Waiting in tests: classify before you write a sleep

Back to [SKILL.md](../SKILL.md) (§20).

Tests reach for a sleep for five different reasons, and only some of them justify one: decide
which of these a site is *before* writing or converting it — the class dictates the shape.

**Positive eventual — never a sleep.** "Something another goroutine will do has happened": an
event was consumed, a container was inspected, a rule was applied. Wait on the signal, or poll
with a deadline: a slow machine then costs milliseconds instead of flaking, and the failure names
the contract that was broken rather than "unexpected nil". Inside `internal/daemon`, reuse
`waitFor(t, cond)` (2-second deadline, fails with `condition not met before deadline`). Anywhere
else, look for an existing helper in the package first, and add a small `waitFor`-style one only
when a site needs it — not speculatively. A hand-rolled `for { … deadline … time.Sleep }` in a
test body is this class too — convert it.

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

A new test uses the shape its class dictates. When you touch a test that has an unclassified
sleep or a hand-rolled deadline loop, convert it or add the comment that names its class.

## Known unconverted sites (verify before relying on it)

Not yet converted or commented, as of the last update to this file — check the code first, and
remove an entry once it is fixed:

- two hand-rolled deadline loops outside `internal/daemon`: `waitStuckConn` in
  `internal/launcher/launcher_test.go`, and the inspect wait in
  `TestReconcile_SerializesConfigLoadThroughApply` (`internal/processor/serialize_test.go`);
- the negative-assertion sleep in `TestCoordinator_ReservedPendingDoesNotSpin`
  (`internal/daemon/coordinator_test.go`), which has no comment saying so.

Do not add to this list; a new test uses the shape its class dictates.
