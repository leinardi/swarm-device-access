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

//! The `bpf(2)` calls against the running kernel: every runtime fixture under
//! `tests/testdata/devfilter`, and the daemon's wrapper around it, loaded and dumped back, which
//! must give the committed `.reloaded` and `.wrapped` dumps, as the Go capture test
//! (`TestCaptureFixtures`) recorded them. Nothing is attached or detached.
//!
//! Needs `CAP_BPF` and `CAP_SYS_ADMIN`, so it is built only with the `privileged-tests` feature
//! and run only on throwaway hosts, as root:
//! `cargo test --features privileged-tests --test bpf_privileged`.
use std::error::Error;
use std::path::Path;
use std::sync::Arc;

use swarm_device_access::cgroup::{
    DeviceRule, KernelOps, OWNED_PROGRAM_NAME, V2Ops as _, canonical_bytes, emit_owned,
    from_canonical,
};
use swarm_device_access::clock::SystemClock;
use swarm_device_access::logger::LogHandle;

/// The nonce and rules the `.wrapped` fixtures were produced with.
const FIXTURE_NONCE: u64 = 0x0123_4567_89ab_cdef;

fn fixture_rules() -> Vec<DeviceRule> {
    let rule = |kind: &str, major, minor, access: &str| DeviceRule {
        allow: true,
        kind: kind.to_owned(),
        major: Some(major),
        minor: Some(minor),
        access: access.to_owned(),
    };
    vec![rule("c", 10, 200, "rwm"), rule("b", 8, 0, "r")]
}

fn read_fixture(name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/testdata/devfilter")
        .join(name);
    let hex: String = std::fs::read_to_string(&path)?.split_whitespace().collect();
    let bytes = (0..hex.len())
        .step_by(2)
        .map(|index| u8::from_str_radix(hex.get(index..index + 2).unwrap_or("?"), 16))
        .collect::<Result<_, _>>()?;
    Ok(bytes)
}

/// Loads `raw` as a device program named `name` and returns the kernel's dump of it.
fn load_and_dump(ops: &KernelOps, raw: &[u8], name: &str) -> Result<Vec<u8>, Box<dyn Error>> {
    let instructions = from_canonical(raw)?;
    let prog = ops.load(name, &instructions)?;
    let (dumped, meta) = ops.instructions(&prog)?;
    assert!(meta.map_ids_known);
    assert_eq!(meta.map_count, 0);
    Ok(canonical_bytes(&dumped)?)
}

#[test]
fn fixtures_load_and_dump_as_captured() -> Result<(), Box<dyn Error>> {
    let ops = KernelOps::new(LogHandle::stdout(Arc::new(SystemClock), |_| None));
    for name in ["runc", "crun", "systemd"] {
        let original = read_fixture(&format!("{name}.orig"))?;
        assert_eq!(
            load_and_dump(&ops, &original, "")?,
            read_fixture(&format!("{name}.reloaded"))?,
            "{name}: the original reloaded"
        );
        let wrapper = emit_owned(&fixture_rules(), FIXTURE_NONCE, &original)?;
        assert_eq!(
            load_and_dump(&ops, &wrapper, OWNED_PROGRAM_NAME)?,
            read_fixture(&format!("{name}.wrapped"))?,
            "{name}: the wrapper"
        );
    }
    Ok(())
}
