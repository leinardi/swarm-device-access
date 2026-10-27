# Device filter goldens

What the Go daemon's cgroup v2 device filter code produces, for the Rust port to reproduce byte
for byte (`docs/rust-port.md`, "Go golden captures"). `TestGoldenDevfilter`
(`internal/cgroup/golden_capture_test.go`, behind the `goldencapture` tag) writes every file here;
`make golden-check` fails when the Go code would write something else, and `make golden-update`
rewrites them for a reviewed change.

- `{runc,crun,systemd}.{orig,reloaded,wrapped}` are copies of the runtime fixtures under
  `internal/cgroup/testdata/devfilter`, whose README says how they were captured from the kernel:
  one 8-byte instruction per line in hex.
- `golden.json` holds, per fixture, its canonical bytes, its gate verdict, the parses of its
  original and of the kernel's dump of its wrapper, the wrapper emitted for each rule set and nonce
  of the corpus (with its gate verdict and parse, or the error that refused it), and the parses of
  corrupted wrappers; then the gate verdicts of synthetic programs and the program builder's
  output for the cases of `ebpf_test.go`. A `null` verdict or error means none.

Instructions are in host byte order, as the kernel dumps them; the files were captured on a
little-endian (x86_64) host.
