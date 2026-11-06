# Go release parity captures

What the Go 1.x release (`v1.0.3`) answered, for `tests/parity.rs` to compare the Rust binary
with (`docs/rust-port.md`, end-to-end parity with the Go release). The ignored test
`capture_go_parity` writes the `go-*` files from the Go binary that `SDA_GO_BINARY` names:

```bash
SDA_GO_BINARY=/path/to/swarm-device-access-v1.0.3 cargo test --test parity -- --ignored
```

- `go-cli.txt`: each command-line case of `CLI_CASES` in Go's single-dash spelling, run in the
  repository root with an empty environment: the arguments, the exit code, stdout and stderr,
  each case ending with `#END`.
- `go-requests.txt`: the requests one daemon run (`-dry-run -log-level=debug`, a config file, a
  metrics address) sent the Engine on the test's scripted Engine (the startup, a `SIGHUP` reload
  that applies, one that is rejected, a scrape and `SIGTERM`), with the user agent up to its
  architecture.
- `go-metrics.txt`: the `sda_` families of its scrape.

The run masks what changes from run to run: the version stamps, the temporary directory, the metrics
port, the events `since` and the two time-valued samples. The daemon's log lines are not compared
since it logs through `tracing`; the `RLIMIT_MEMLOCK` warning, which depends on the host, is checked
on its own. The `.conf` files are the YAML config fixtures,
named so that the YAML hooks leave them as they are, of the command-line cases: `tab-value.conf` has
a tab after its colon (Q6).
