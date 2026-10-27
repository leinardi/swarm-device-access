# Core — swarm-device-access

Linux-only daemon that sets cgroup device-allow rules for Docker Swarm containers
that bind-mount `/dev/...` paths. Solves the Docker Swarm gap where `devices:` /
`device_cgroup_rules:` are silently ignored. Rust since 2.0 (the Go 1.x code was
removed at the cutover); 1.x behaviour is the contract, except the Q entries in the
README's "Migrating from 1.x".

## Source map

```
Cargo.toml                 # workspace: "." (daemon lib + bin), meta, testenv
src/
  main.rs                  # hand-built tokio runtime (2 workers), exit code
  run.rs                   # wiring: flags, settings, openat2 probe, RLIMIT_MEMLOCK, store,
                           #   Docker client, metrics server, processor, daemon::run; SIGHUP;
                           #   launch dispatch; SHUTDOWN_BUDGET; HOST_ROOT_PATH = "/host"
  cli.rs                   # clap flags, double-dash only (Q1); daemon and launch flag sets
  cli/settings.rs          # Settings::merge/validate, cold_changes, Reloader, watch_sighup
  config/                  # strict YAML loader (load_file, KEYS shapes in yaml.rs); Store/Publisher
  policy/                  # Mode, Global (enabled, denied, authorized), Container::parse, unknown_labels
  launcher/                # launch mode: self_id, stale, spec (daemon_spec: privileged set), engine
                           #   (the only Docker writes), supervision
  daemon/                  # run (subscribe, coordinator, startup pass, reload watcher, events)
    events.rs              # listen_events/consume_events, reconnect backoff
    coordinator.rs         # passes, retries, reservations, processed dedup (PROCESSED_TTL), sweep
  processor/               # one idempotent reconcile path
    reconcile.rs           # Processor: reconcile, compute_desired, apply_pinned, terminate,
                           #   sweep, publish_and_reconcile; the blocking transaction
    rules.rs               # collect_container_rules, collect_mount_rules, evaluate_candidate
    devfs.rs               # DevFs seam: openat2 beneath /dev, fstat, sysfs uevent; probe_openat2
    pin.rs                 # pidfd pinning, /proc/<pid>/cgroup
    lifecycle.rs           # per-container run history
  cgroup/                  # device rules (api/ebpf/v1/v2 NVIDIA-derived, Apache 2.0)
    api.rs                 # DeviceRule, Interface::set_device_rules(handle, rules), new, /proc parsing
    handle.rs              # CgroupHandle (directory fd + identity)
    v1.rs, v1set.rs, ledger.rs                  # devices.allow/devices.deny, owned-exception ledger
    v2.rs, v2plan.rs, v2ops.rs, owned.rs, ebpf.rs, asm.rs  # owned wrapper around runtime filter
    bpf_sys.rs             # raw bpf(2); the only unsafe module
  docker/                  # read-only Engine client on hyper (GET + HEAD /_ping, unix socket only)
  metrics/, server.rs      # Recorder (noop form), exposition; /metrics, /healthz, /readyz
  logger/                  # slog-compatible text/json/plain handlers behind LogHandle
  systemd.rs               # zbus D-Bus Reloading watcher
  go_compat/               # Go stdlib formatting/parsing the 1.x text depends on
  clock.rs                 # the one wall-clock read; Clock trait
tests/                     # goldens vs frozen 1.x captures (tests/testdata), wire, parity
meta/                      # repository meta-tests (pins, Dockerfile, CI gates, links, boundaries)
testenv/                   # integration harness (feature integration), host Docker
deployments/docker/        # Dockerfile, compose file running the launcher, example config
docs/                      # architecture.md, testing.md, release.md, rust-port.md (ledger)
```

## Key invariants

- `src/cgroup/{api,ebpf,v1,v2}.rs` keep NVIDIA's Apache 2.0 header — do not relicense or reformat.
- `unsafe` and `libc` only in `src/cgroup/bpf_sys.rs`; Docker writes only in `src/launcher/engine.rs`.
- Every trigger runs `Processor::reconcile`; anything uncertain applies the empty set (fail closed).
- Log, error, metric and Engine-request text is the 1.x contract; a difference needs a README Q entry.
- `docs/rust-port.md` is the reuse ledger: check and update it when a reused component changes or
  an upstream fix is taken.
- Container dedup: the coordinator's `processed` map bridges enumeration and the event stream.

See `mem:tech_stack`, `mem:conventions`, `mem:suggested_commands`, `mem:task_completion`.
