# Release

How swarm-device-access versions are decided and what has to be set up by hand for that to hold.

## Commit messages are checked on every pull request

Every commit must be a Conventional Commit with a scope (see [CONTRIBUTING.md](../CONTRIBUTING.md#commit-messages)). The local
`commit-msg` hook only runs where somebody ran `make pre-commit-install`, so a commit made without it would otherwise reach
`master` unchecked. The `conventional-commits` job in [`.github/workflows/ci.yaml`](../.github/workflows/ci.yaml) closes that gap:
on every pull request it runs the repository's own `conventional-pre-commit` hook, `--force-scope` included, over every commit
between the pull request's base and head. Merge commits are skipped.

Only merge commits are enabled on this repository; squash and rebase merging are disabled. Every commit in a pull request
therefore lands on `master` as it is, and the pull-request title never becomes a commit subject, which is why there is no
title check. If squash merging is ever enabled, the title becomes the commit subject, and a pull-request title check has to be
added to CI first.

## Mandatory setup: make the check blocking

A failing CI job does not block a merge on its own: the `master` ruleset ("Protect default branch", id `16797240`) has no
required status checks. Once the `conventional-commits` job has run at least once on a pull request (GitHub can only require a
check whose context it has already seen), add it to that ruleset as a required status check, keeping every existing rule:

```json
{
  "type": "required_status_checks",
  "parameters": {
    "strict_required_status_checks_policy": false,
    "required_status_checks": [{ "context": "conventional-commits", "integration_id": 15368 }]
  }
}
```

`15368` is the GitHub Actions app, so only a check reported by Actions satisfies the rule. Apply it with
`gh api -X PUT repos/leinardi/swarm-device-access/rulesets/16797240`, sending the ruleset's current `rules` array with this entry
appended; a `PUT` replaces the whole list, so leaving out the existing rules would delete them. Check the result with
`gh api repos/leinardi/swarm-device-access/rulesets/16797240`.

Repository admins keep the ruleset's pull-request bypass, so an admin can still merge a pull request whose check fails. That
is a deliberate escape hatch, not a gap in the check.
