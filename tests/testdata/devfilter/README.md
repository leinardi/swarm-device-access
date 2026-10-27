# Device filter goldens

What the Go daemon's cgroup v2 device filter code produces, for the Rust port to reproduce byte
for byte (`docs/rust-port.md`, "Go golden captures"). `TestGoldenDevfilter` and `TestGoldenAsm`
(`internal/cgroup/golden_capture_test.go`, behind the `goldencapture` tag) wrote every file here.
They are frozen 1.x captures: the Go code and its capture tests were removed at the cutover, and
are in the git history at the parent of the commit that deleted `go.mod` (`git log -1 -- go.mod`
names that commit).

- `{runc,crun,systemd}.{orig,reloaded,wrapped}` are real `BPF_CGROUP_DEVICE` programs, as the
  daemon reads them back from the kernel (translated dump, canonical bytes, one 8-byte instruction
  per line in hex), captured on Linux 7.0 (x86_64): `systemd` from systemd 259 (`sd_devices`) for a
  Docker 29 container (systemd cgroup driver), `runc` from runc 1.5.1 for the same container, and
  `crun` from crun 1.28 under podman 5.8 (cgroupfs manager). `.orig` is the program as dumped from
  the container's cgroup, `.reloaded` the dump after loading `.orig` back unchanged (it must equal
  `.orig` for the reloadability gate's premise to hold), and `.wrapped` the dump after loading the
  daemon's wrapper around `.orig` for the fixture rules and nonce. The Go capture
  (`TestCaptureFixtures`, behind the `fixturecapture` tag) only loaded and dumped programs.
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
