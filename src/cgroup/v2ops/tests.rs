/*
 * Copyright 2026 Roberto Leinardi.
 *
 * Licensed under the Apache License, Version 2.0 (the "License");
 * you may not use this file except in compliance with the License.
 * You may obtain a copy of the License at
 *
 *     https://www.apache.org/licenses/LICENSE-2.0
 *
 * Unless required by applicable law or agreed to in writing, software
 * distributed under the License is distributed on an "AS IS" BASIS,
 * WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
 * See the License for the specific language governing permissions and
 * limitations under the License.
 */

use rustix::io::Errno;

use super::{
    F_ALLOW_MULTI, F_ALLOW_OVERRIDE, FetchError, InfoError, InstructionsError, LoadError,
    MAX_VERIFIER_LOG_SIZE, MIN_VERIFIER_LOG_SIZE, OpsError, ReplaceProbe, describe_attach_flags,
    kernel_at_least, retry_log,
};
use crate::cgroup::asm::{DecodeCause, DecodeError, EncodeCause};
use crate::cgroup::bpf_sys::LoadFailure;

/// Go: `TestKernelAtLeast`, with more releases, each verdict printed by the Go function.
#[test]
fn kernel_at_least_parses_like_go() {
    let cases = [
        ("4.16.0", true),
        ("4.15.18", false),
        ("5.4.0-100-generic", true),
        ("3.99", false),
        ("junk", false),
        (" 5.0", true),
        ("\t4.16", true),
        ("\n4.16", false),
        ("\r\n4.16", false),
        ("\r4.16", true),
        ("+4.16", true),
        ("-5.0", false),
        ("4. 16", true),
        ("4 .16", false),
        ("4.", false),
        ("4", false),
        ("", false),
        ("99999999999999999999.0", false),
        ("5.-1", true),
        ("4.+16", true),
        ("\u{664}.\u{661}\u{666}", false),
        ("4.16\n", true),
        ("\u{a0}4.16", true),
        ("4.0x10", false),
        ("04.016", true),
        ("4.15", false),
        ("4.17", true),
        ("5.0", true),
        ("4.16x", true),
        ("4.\n16", false),
        ("4.\u{3000}16", true),
        ("4_0.1", false),
        ("9223372036854775807.0", true),
        ("-9223372036854775808.0", false),
        ("+-4.16", false),
    ];
    for (release, want) in cases {
        assert_eq!(kernel_at_least(release, 4, 16), want, "{release:?}");
    }
}

#[test]
fn attach_flags_are_named() {
    assert_eq!(describe_attach_flags(0), "none (exclusive)");
    assert_eq!(
        describe_attach_flags(F_ALLOW_OVERRIDE),
        "BPF_F_ALLOW_OVERRIDE"
    );
    assert_eq!(describe_attach_flags(F_ALLOW_MULTI), "BPF_F_ALLOW_MULTI");
    assert_eq!(
        describe_attach_flags(F_ALLOW_OVERRIDE | F_ALLOW_MULTI | 4),
        "BPF_F_ALLOW_OVERRIDE|BPF_F_ALLOW_MULTI"
    );
}

/// The first success or `EINVAL` decides, for good; any other failure decides nothing.
#[test]
fn the_replace_probe_keeps_the_first_answer() {
    let probe = ReplaceProbe::new();
    probe.record(Err(&OpsError::Detach(Errno::BUSY)));
    assert!(probe.usable());
    probe.record(Err(&OpsError::Attach {
        flags: F_ALLOW_MULTI,
        errno: Errno::INVAL,
    }));
    assert!(!probe.usable());
    probe.record(Ok(()));
    assert!(!probe.usable());

    let probe = ReplaceProbe::new();
    probe.record(Ok(()));
    probe.record(Err(&OpsError::Attach {
        flags: F_ALLOW_MULTI,
        errno: Errno::INVAL,
    }));
    assert!(probe.usable());
    assert!(ReplaceProbe::known(true).usable());
    assert!(!ReplaceProbe::known(false).usable());
}

fn failure(errno: Errno, log_true_size: u32) -> LoadFailure {
    LoadFailure {
        errno,
        log_true_size,
    }
}

/// cilium's `retryLogAttrs`: the log is enabled after a failure without it, grown on `ENOSPC`
/// to the size the kernel reports or by doubling, and never past its maximum.
#[test]
fn a_failed_load_is_retried_with_a_growing_log() {
    let (mut level, mut size) = (0, 0);
    assert!(retry_log(&mut level, &mut size, failure(Errno::INVAL, 0)));
    assert_eq!((level, size), (1, MIN_VERIFIER_LOG_SIZE));
    assert!(!retry_log(&mut level, &mut size, failure(Errno::INVAL, 0)));

    assert!(retry_log(&mut level, &mut size, failure(Errno::NOSPC, 0)));
    assert_eq!(size, 2 * MIN_VERIFIER_LOG_SIZE);
    assert!(retry_log(
        &mut level,
        &mut size,
        failure(Errno::NOSPC, 300_000)
    ));
    assert_eq!(size, 300_000);

    size = MAX_VERIFIER_LOG_SIZE / 2;
    assert!(retry_log(&mut level, &mut size, failure(Errno::NOSPC, 0)));
    assert_eq!(size, MAX_VERIFIER_LOG_SIZE);
    assert!(!retry_log(&mut level, &mut size, failure(Errno::NOSPC, 0)));
}

/// cilium's `VerifierError` text for each log, printed by a Go program running cilium's
/// `internal/errors.go`.
#[test]
fn a_verifier_error_reads_as_cilium() {
    let cases = [
        ("", "load program: invalid argument"),
        (
            "0: (b7) r0 = 0\n1: (95) exit\nR0 !read_ok\nprocessed 2 insns (limit 1000000) \
             max_states_per_insn 0\n",
            "load program: invalid argument: 1: (95) exit: R0 !read_ok (2 line(s) omitted)",
        ),
        (
            "0: (61) r2 = *(u32 *)(r1 +12)\ninvalid bpf_context access off=12 size=4\n\
             processed 1 insns\n",
            "load program: invalid argument: 0: (61) r2 = *(u32 *)(r1 +12): invalid \
             bpf_context access off=12 size=4 (1 line(s) omitted)",
        ),
        (
            "func#0 @0\n0: R1=ctx() R10=fp0\n0: (85) call bpf_ktime_get_ns#5\n\tunknown func\n",
            "load program: invalid argument: 0: (85) call bpf_ktime_get_ns#5: unknown func (2 \
             line(s) omitted)",
        ),
        (
            "processed 1 insns (limit 1000000)\n",
            "load program: invalid argument",
        ),
        ("  \n\n\t ", "load program: invalid argument"),
        (
            "first line  \nsecond\t\n",
            "load program: invalid argument: second (1 line(s) omitted)",
        ),
        ("only line", "load program: invalid argument: only line"),
        ("abc\0def", "load program: invalid argument: abc"),
        (
            "one\ntwo\n   R1 type=ctx\n",
            "load program: invalid argument: R1 type=ctx (2 line(s) omitted)",
        ),
        (
            "one\nR1 type=ctx expected=fp\n",
            "load program: invalid argument: one: R1 type=ctx expected=fp",
        ),
        ("R2 !read_ok", "load program: invalid argument: R2 !read_ok"),
        (
            "a\n\nb\n",
            "load program: invalid argument: b (2 line(s) omitted)",
        ),
    ];
    for (log, want) in cases {
        let error = LoadError::from_failure(Errno::INVAL, log.as_bytes());
        assert_eq!(error.to_string(), want, "{log:?}");
    }
}

/// cilium's hints on a failed load, and its reading of the log tail.
#[test]
fn load_failures_carry_cilium_hints() {
    let cases = [
        (
            LoadError::from_failure(Errno::PERM, &[0; 16]),
            "load program: operation not permitted (MEMLOCK may be too low, consider \
             rlimit.RemoveMemlock)",
        ),
        (
            LoadError::from_failure(Errno::PERM, b"denied\n"),
            "load program: operation not permitted: denied",
        ),
        (
            LoadError::from_failure(Errno::PERM, b""),
            "load program: operation not permitted",
        ),
        (
            LoadError::from_failure(Errno::FAULT, b"x"),
            "load program: bad address (hit verifier bug, increase LogSizeStart to fit the log \
             and check dmesg)",
        ),
        (
            LoadError::from_failure(Errno::INVAL, b"0: call\ninvalid func unknown#195896080\n"),
            "load program: bad CO-RE relocation: invalid func unknown#195896080 (1 line(s) \
             omitted)",
        ),
        (
            LoadError::from_failure(Errno::INVAL, b"invalid func unknown#233685214\n"),
            "load program: unknown kfunc: invalid func unknown#233685214",
        ),
        (
            LoadError::from_failure(Errno::ACCESS, b"(18) r10 = 0xbad2310\n"),
            "load program: bad CO-RE relocation: (18) r10 = 0xbad2310",
        ),
        (
            LoadError::from_failure(Errno::from_raw_os_error(524), b""),
            "load program: operation not supported",
        ),
        (
            LoadError::Attempts(Errno::NOSPC),
            "load program: no space left on device (bug: hit 30 verifier attempts)",
        ),
        (LoadError::Empty, "instructions cannot be empty"),
        (
            LoadError::Encode {
                index: 3,
                cause: EncodeCause::InvalidOpCode,
            },
            "instruction 3: invalid opcode",
        ),
    ];
    for (error, want) in cases {
        assert_eq!(error.to_string(), want);
    }
}

/// Go's wording of each kernel operation's failure.
#[test]
fn ops_errors_read_as_go() {
    let cases = [
        (
            OpsError::Query(Errno::BADF),
            "bpf_prog_query(BPF_CGROUP_DEVICE) failed: bad file descriptor",
        ),
        (
            OpsError::IncompleteProgList,
            "could not get complete list of CGROUP_DEVICE programs",
        ),
        (
            OpsError::Fetch(FetchError::GetById(Errno::NOENT)),
            "cannot fetch program from id: get program by id: no such file or directory",
        ),
        (
            OpsError::Fetch(FetchError::Discover(Errno::ACCESS)),
            "cannot fetch program from id: discover program type: getting object info: \
             permission denied",
        ),
        (
            OpsError::Load(LoadError::from_failure(Errno::INVAL, b"")),
            "unable to create new device filters program: load program: invalid argument",
        ),
        (
            OpsError::Attach {
                flags: F_ALLOW_MULTI,
                errno: Errno::INVAL,
            },
            "failed to call BPF_PROG_ATTACH (BPF_CGROUP_DEVICE, flags BPF_F_ALLOW_MULTI): \
             attach program: invalid argument",
        ),
        (
            OpsError::Detach(Errno::NOENT),
            "failed to call BPF_PROG_DETACH (BPF_CGROUP_DEVICE): can't detach program: no such \
             file or directory",
        ),
        (
            OpsError::OldKernel,
            "device filter program cannot be wrapped safely: kernel older than 4.16",
        ),
        (
            OpsError::Info(InfoError::ObjInfo(Errno::INVAL)),
            "unable to get Info() of the original device filters program: getting object info: \
             invalid argument",
        ),
        (
            OpsError::Info(InfoError::Xlated(Errno::FAULT)),
            "unable to get Info() of the original device filters program: bad address",
        ),
        (
            OpsError::Instructions(InstructionsError::Restricted),
            "unable to get the instructions of the original device filters program: \
             instructions: restricted by kernel.kptr_restrict and/or net.core.bpf_jit_harden \
             sysctls",
        ),
        (
            OpsError::Instructions(InstructionsError::NotSupported),
            "unable to get the instructions of the original device filters program: \
             insufficient permissions or unsupported kernel: not supported",
        ),
        (
            OpsError::Instructions(InstructionsError::Decode(DecodeError {
                offset: 8,
                cause: DecodeCause::UnexpectedEof,
            })),
            "unable to get the instructions of the original device filters program: \
             unmarshaling instructions: offset 8: unexpected EOF",
        ),
    ];
    for (error, want) in cases {
        assert_eq!(error.to_string(), want);
    }
    assert!(FetchError::GetById(Errno::PERM).is_permission());
    assert!(FetchError::Discover(Errno::ACCESS).is_permission());
    assert!(!FetchError::GetById(Errno::NOENT).is_permission());
    assert!(OpsError::OldKernel.is_not_wrappable());
    assert_eq!(OpsError::Detach(Errno::BUSY).errno(), Some(Errno::BUSY));
}
