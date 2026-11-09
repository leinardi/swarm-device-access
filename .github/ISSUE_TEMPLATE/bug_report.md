---
name: Bug report
about: Report a bug or unexpected behavior
title: "[Bug]: "
labels: ["bug"]
assignees: []
---
## Step 1: Are you in the right place?

* [ ] I have checked that there are no duplicate active or recent issues (bugs, questions, or
  feature requests) describing this problem.
* [ ] I am using the latest release (the binary, or `ghcr.io/leinardi/swarm-device-access:latest`),
  or have tested a build of `master`.
* [ ] This is not a security vulnerability (those go through the private process in SECURITY.md).

## Step 2: Describe your environment

<!-- The version is in the daemon's startup line (`swarm-device-access starting version=...
commit=...`); the binary has no --version. -->

* swarm-device-access version and commit (startup line, or the image tag and digest): `?`
* How it runs (the launcher as a Swarm service, `docker run`, the release binary, own build): `?`
* Operating system, kernel version (`uname -r`) and cgroup version (v1 or v2): `?`
* Docker Engine version (`docker version`), and whether the node runs in Swarm mode: `?`
* Container runtime (runc, crun, ...) and init system (systemd or other): `?`
* Daemon flags or config file (redact anything private): `?`

## Step 3: Describe the problem

### Steps to reproduce

1. ---
2. ---
3. ---

### Observed results

<!-- What happened? This could be a description, error message, or log output. -->

*

### Expected results

<!-- What did you expect to happen? -->

*

### Relevant code, configuration, or data

<!-- Paste the smallest service definition, command line or config file that reproduces the
issue, with the device paths it bind-mounts. Never paste a secret, such as a registry token. -->

```yaml
# Example snippet (replace with your own)
```

### Error / log output

<!-- If you are getting an error, paste the output here. -->

```text
<paste logs or error messages here>
```

<!-- Adding screenshots, screen recordings, or additional context is always helpful. -->
