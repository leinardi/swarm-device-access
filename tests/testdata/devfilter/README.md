# Device filter goldens

What the Go daemon's cgroup v2 device filter code produces, for the Rust port to reproduce byte
for byte (`docs/rust-port.md`, "Go golden captures"). `TestGoldenDevfilter` and `TestGoldenAsm`
(`internal/cgroup/golden_capture_test.go`, behind the `goldencapture` tag) write every file here;
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
- `asm.tsv` (`TestGoldenAsm`) has one row per raw input: every opcode byte with nine register,
  offset and immediate variants, a `ld_imm64` with each kind of second half, and truncated input.
  Its columns are the raw bytes, then either `decode error` and the error, or `ok`, cilium's text
  of each instruction (joined by a semicolon and a space), the canonical re-encoding (or
  `encode error:`, a space and the error) and the reloadability gate's verdict (`-` for none).

Instructions are in host byte order, as the kernel dumps them; the files were captured on a
little-endian (x86_64) host.
