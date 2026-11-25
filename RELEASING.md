# Releasing

The short version. How it works, the one-time setup and what to do when something fails are in
[docs/internal/release.md](docs/internal/release.md).

Needs the GitHub CLI (`gh`) with write access to the repository. Release from `master`, once CI
and the integration suite have passed on the commit's push: the `ci-green` job waits for both for
up to 30 minutes and fails if one did not pass.

## A release

```sh
gh workflow run release.yaml --ref master -f dry_run=true   # rehearse: builds, tests, scans, publishes nothing
gh workflow run release.yaml --ref master                   # release the version the commits give
gh workflow run release.yaml --ref master -f version=2.1.0  # or force a version
```

Without `version`, svu derives it from the Conventional Commits since the last tag: `feat` a minor,
`fix` a patch, `!` or `BREAKING CHANGE:` a major; with only other types it fails with "nothing to
bump". A merged Dependabot pull request with a `fix(deps)` commit releases a patch on its own.

## When something fails

Use "Re-run all jobs" on the failed run (not "Re-run failed jobs", unless `audit` itself failed),
or dispatch again with the same `version` while `master` still points at the tagged commit. The
recovery reuses what was already published for each platform and builds only what is missing; a
wrong release gets a new version, never a moved tag. See
[Recovering a failed publish](docs/internal/release.md#recovering-a-failed-publish).
