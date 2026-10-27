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

//! Go's `v1set_test.go`, with its model of the kernel behind a cgroup v1 directory.

use std::collections::HashSet;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use super::*;
use crate::cgroup::api::Interface as _;
use crate::cgroup::handle::Identity;
use crate::cgroup::ledger::Ledger;
use crate::cgroup::testing::rule;
use crate::test_support::TempDir;

/// Stands in for the kernel behind a cgroup v1 directory made of temporary files. Reads and writes
/// go through the real `openat` code ([`OpenatFiles`]) on the directory descriptor; after every
/// successful write the model applies cgroup v1 exception semantics (allow ORs access into the
/// exact entry, deny clears it, wildcard entries are never touched by an exact write) and
/// rewrites `devices.list`.
#[derive(Debug)]
struct Model {
    dir: TempDir,
    state: Mutex<ModelState>,
}

#[derive(Debug, Default)]
struct ModelState {
    entries: Vec<ListEntry>,
    /// `allow <line>`, `deny <line>` or `read`: the calls that fail with an injected error.
    fail: HashSet<String>,
    /// The kernel accepts writes without effect.
    ignore_writes: bool,
    writes: Vec<String>,
}

const INJECTED: &str = "injected";

fn injected() -> io::Error {
    io::Error::other(INJECTED)
}

impl Model {
    fn new(list: &str) -> Arc<Self> {
        let model = Arc::new(Self {
            dir: TempDir::new(),
            state: Mutex::new(ModelState {
                entries: parse_devices_list(list.as_bytes()).unwrap(),
                ..ModelState::default()
            }),
        });
        for name in [DEVICES_ALLOW, DEVICES_DENY] {
            std::fs::write(model.dir.path().join(name), "").unwrap();
        }
        model.sync(&model.state());
        model
    }

    fn state(&self) -> MutexGuard<'_, ModelState> {
        self.state.lock().unwrap()
    }

    fn sync(&self, state: &ModelState) {
        let mut lines = String::new();
        for entry in &state.entries {
            lines.push_str(&format_entry(*entry));
            lines.push('\n');
        }
        std::fs::write(self.dir.path().join(DEVICES_LIST), lines).unwrap();
    }

    fn list(&self) -> String {
        let entries: Vec<String> = self
            .state()
            .entries
            .iter()
            .map(|entry| format_entry(*entry))
            .collect();
        entries.join(", ")
    }

    /// Mutates the exact entry for `line`, as the kernel does.
    fn apply(&self, allow: bool, line: &str) {
        let mut state = self.state();
        self.apply_locked(&mut state, allow, line);
    }

    fn apply_locked(&self, state: &mut ModelState, allow: bool, line: &str) {
        let parsed = parse_devices_list(line.as_bytes()).unwrap();
        let [rule] = parsed.as_slice() else {
            panic!("bad rule line {line:?}");
        };
        let index = state.entries.iter().position(|entry| {
            entry.kind == rule.kind && entry.major == rule.major && entry.minor == rule.minor
        });
        match (allow, index) {
            (true, None) => state.entries.push(*rule),
            (true, Some(index)) => state.entries[index].access |= rule.access,
            (false, Some(index)) => {
                state.entries[index].access &= !rule.access;
                if state.entries[index].access == 0 {
                    state.entries.remove(index);
                }
            }
            (false, None) => {}
        }
        self.sync(state);
    }

    fn writes(&self) -> Vec<String> {
        self.state().writes.clone()
    }

    fn writes_with(&self, verb: &str) -> Vec<String> {
        self.writes()
            .into_iter()
            .filter(|call| call.starts_with(&format!("{verb} ")))
            .collect()
    }

    fn fail(&self, call: &str) {
        self.state().fail.insert(call.to_owned());
    }

    /// Opens the model's directory as a real cgroup handle, with a controller on the model.
    fn setup(self: &Arc<Self>, ledger: &Arc<Ledger>) -> (CgroupV1, CgroupHandle) {
        let handle = CgroupHandle::open(self.dir.path().to_str().unwrap()).unwrap();
        let cgroup = CgroupV1 {
            ledger: Arc::clone(ledger),
            files: Box::new(ModelFiles(Arc::clone(self))),
        };
        (cgroup, handle)
    }
}

#[derive(Debug)]
struct ModelFiles(Arc<Model>);

impl V1Files for ModelFiles {
    fn read(&self, dir: BorrowedFd<'_>, name: &str) -> Result<Vec<u8>, FileError> {
        if self.0.state().fail.contains("read") {
            return Err(FileError::Open {
                name: name.to_owned(),
                source: injected(),
            });
        }
        OpenatFiles.read(dir, name)
    }

    fn write(&self, dir: BorrowedFd<'_>, name: &str, line: &str) -> Result<(), FileError> {
        let mut state = self.0.state();
        let verb = if name == DEVICES_ALLOW {
            "allow"
        } else {
            "deny"
        };
        let call = format!("{verb} {line}");
        state.writes.push(call.clone());
        if state.fail.contains(&call) {
            return Err(FileError::Write {
                line: line.to_owned(),
                name: name.to_owned(),
                source: injected(),
            });
        }
        OpenatFiles.write(dir, name, line)?;
        if !state.ignore_writes {
            self.0.apply_locked(&mut state, name == DEVICES_ALLOW, line);
        }
        Ok(())
    }
}

fn format_entry(entry: ListEntry) -> String {
    let number = |value: i64| {
        if value == WILDCARD {
            "*".to_owned()
        } else {
            value.to_string()
        }
    };
    format!(
        "{} {}:{} {}",
        char::from(entry.kind),
        number(entry.major),
        number(entry.minor),
        access_string(entry.access)
    )
}

fn bit(kind: u8, major: i64, minor: i64, access: u8) -> DevBit {
    DevBit {
        kind,
        major,
        minor,
        access,
    }
}

fn owned_lines(ledger: &Ledger, id: &Identity) -> Vec<String> {
    ledger.owned(id).into_iter().map(DevBit::line).collect()
}

/// Go: `TestCovered_Table`.
#[test]
fn coverage_follows_cgroup_v1_matching() {
    let cases = [
        ("c 10:200 rw", bit(b'c', 10, 200, ACC_READ), true),
        ("c 10:200 rw", bit(b'c', 10, 200, ACC_MKNOD), false),
        ("c 10:200 rw", bit(b'c', 10, 201, ACC_READ), false),
        ("c 10:200 rw", bit(b'b', 10, 200, ACC_READ), false),
        ("a *:* rwm", bit(b'b', 8, 0, ACC_WRITE), true),
        ("a *:* m", bit(b'c', 1, 3, ACC_READ), false),
        ("c 1:* m", bit(b'c', 1, 3, ACC_MKNOD), true),
        ("c 1:* m", bit(b'c', 1, 3, ACC_READ), false),
        ("c 1:* m", bit(b'c', 2, 3, ACC_MKNOD), false),
        ("b 8:* r", bit(b'b', 8, 16, ACC_READ), true),
        ("b 8:* r", bit(b'b', 8, 16, ACC_WRITE), false),
        ("c *:* m", bit(b'c', 99, 1, ACC_MKNOD), true),
        ("c *:5 w", bit(b'c', 99, 5, ACC_WRITE), true),
    ];
    for (list, bit, want) in cases {
        let entries = parse_devices_list(list.as_bytes()).unwrap();
        assert_eq!(
            covered(&entries, bit),
            want,
            "covered({list:?}, {})",
            bit.line()
        );
    }
}

/// Go: `TestParseDevicesList_RejectsMalformed`, with the texts of a scratch Go program.
#[test]
fn malformed_list_entries_are_refused_with_go_s_text() {
    let cases = [
        ("x 1:3 rwm", r#"malformed devices.list entry: "x 1:3 rwm""#),
        ("c 1 rwm", r#"malformed devices.list entry: "c 1 rwm""#),
        (
            "c a:3 rwm",
            r#"malformed devices.list entry: "c a:3 rwm": device number "a": strconv.ParseInt: parsing "a": invalid syntax"#,
        ),
        (
            "c 1:3 rwx",
            r#"malformed devices.list entry: "c 1:3 rwx": unknown device access 'x'"#,
        ),
        ("c 1:3", r#"malformed devices.list entry: "c 1:3""#),
    ];
    for (bad, want) in cases {
        let error = parse_devices_list(bad.as_bytes()).unwrap_err();
        assert!(matches!(error, Error::ListEntry { .. }), "{bad}: {error:?}");
        assert_eq!(error.to_string(), want);
    }
    // Lines are trimmed, blank ones skipped, and a line too long for Go's scanner fails the read.
    assert_eq!(
        parse_devices_list(b"  c 1:3 rwm \r\n\n\tb 8:* r\n").unwrap(),
        [
            ListEntry {
                kind: b'c',
                major: 1,
                minor: 3,
                access: ACC_READ | ACC_WRITE | ACC_MKNOD
            },
            ListEntry {
                kind: b'b',
                major: 8,
                minor: WILDCARD,
                access: ACC_READ
            },
        ]
    );
    assert_eq!(
        parse_devices_list("x".repeat(70_000).as_bytes())
            .unwrap_err()
            .to_string(),
        "read devices.list: bufio.Scanner: token too long"
    );
}

/// Go: `TestSet_AccessSubsetAndSuperset`: only the bits the baseline lacks are written and owned;
/// a superset request over a partial baseline owns the difference, a subset request owns nothing.
#[test]
fn only_bits_outside_the_baseline_are_written_and_owned() {
    let model = Model::new("c 1:* m\nc 10:200 r\n");
    let ledger = Arc::new(Ledger::default());
    let (cgroup, handle) = model.setup(&ledger);
    cgroup
        .set_device_rules(
            &handle,
            &[
                rule("c", 10, 200, "rw"),
                rule("c", 1, 3, "rwm"),
                rule("c", 1, 5, "m"),
            ],
        )
        .unwrap();
    assert_eq!(
        owned_lines(&ledger, handle.identity()),
        ["c 1:3 r", "c 1:3 w", "c 10:200 w"]
    );
    assert_eq!(
        model.writes_with("allow"),
        ["allow c 1:3 r", "allow c 1:3 w", "allow c 10:200 w"]
    );
}

/// Go: `TestSet_GrantTwiceThenEmptyRevokesAndKeepsBaseline`.
#[test]
fn a_repeated_grant_then_an_empty_set_revokes_and_keeps_the_baseline() {
    let model = Model::new("c 10:200 m\n");
    let ledger = Arc::new(Ledger::default());
    let (cgroup, handle) = model.setup(&ledger);
    let rules = [rule("c", 10, 200, "rwm")];
    for _ in 0..2 {
        cgroup.set_device_rules(&handle, &rules).unwrap();
    }
    assert_eq!(model.list(), "c 10:200 rwm");
    assert_eq!(model.writes_with("allow").len(), 2, "r and w once each");

    cgroup.set_device_rules(&handle, &[]).unwrap();
    assert_eq!(model.list(), "c 10:200 m", "baseline m only");
    assert!(owned_lines(&ledger, handle.identity()).is_empty());
    assert_eq!(
        model.writes_with("deny"),
        ["deny c 10:200 r", "deny c 10:200 w"]
    );
}

/// Go: `TestSet_RedundantGrantIsNotRecordedOrRevoked`.
#[test]
fn a_grant_the_runtime_made_is_neither_recorded_nor_revoked() {
    let model = Model::new("c 1:3 rwm\n");
    let ledger = Arc::new(Ledger::default());
    let (cgroup, handle) = model.setup(&ledger);
    cgroup
        .set_device_rules(&handle, &[rule("c", 1, 3, "rwm")])
        .unwrap();
    assert!(
        owned_lines(&ledger, handle.identity()).is_empty(),
        "the runtime already grants /dev/null"
    );
    cgroup.set_device_rules(&handle, &[]).unwrap();
    assert!(model.writes().is_empty());
    assert_eq!(model.list(), "c 1:3 rwm", "baseline touched");
}

/// Checks that the ledger holds exactly the bits that are effective and not part of
/// `baseline_list`.
fn assert_ledger_matches_files(model: &Model, ledger: &Ledger, id: &Identity, baseline_list: &str) {
    let baseline = parse_devices_list(baseline_list.as_bytes()).unwrap();
    let mut effective = BTreeSet::new();
    for entry in &model.state().entries {
        for one in ACCESS_BITS {
            let candidate = bit(entry.kind, entry.major, entry.minor, one);
            if entry.access & one != 0 && !covered(&baseline, candidate) {
                effective.insert(candidate);
            }
        }
    }
    let effective: Vec<String> = effective.into_iter().map(DevBit::line).collect();
    assert_eq!(
        owned_lines(ledger, id),
        effective,
        "ledger does not match the effective non-baseline bits (list {:?})",
        model.list()
    );
}

/// Go: `TestSet_InjectedFailureAtEachStepKeepsLedgerConsistent`: an A to B transition with every
/// write, in turn, failing; the ledger still describes the files after the failed call, and a
/// retry without the fault converges on B.
#[test]
fn a_failure_at_any_step_keeps_the_ledger_consistent() {
    const BASELINE: &str = "c 1:3 rwm\n";
    let steps = [
        "deny c 10:200 r",
        "deny c 10:200 w",
        "deny c 10:200 m",
        "allow c 10:201 r",
        "allow c 10:201 w",
        "allow c 10:201 m",
    ];
    for failing in steps {
        let model = Model::new(BASELINE);
        let ledger = Arc::new(Ledger::default());
        let (cgroup, handle) = model.setup(&ledger);
        cgroup
            .set_device_rules(&handle, &[rule("c", 10, 200, "rwm")])
            .unwrap();

        model.fail(failing);
        let error = cgroup
            .set_device_rules(&handle, &[rule("c", 10, 201, "rwm")])
            .unwrap_err();
        assert!(error.to_string().contains(INJECTED), "{failing}: {error}");
        assert_ledger_matches_files(&model, &ledger, handle.identity(), BASELINE);

        model.state().fail.clear();
        cgroup
            .set_device_rules(&handle, &[rule("c", 10, 201, "rwm")])
            .unwrap();
        assert_eq!(model.list(), "c 1:3 rwm, c 10:201 rwm", "{failing}");
        assert_ledger_matches_files(&model, &ledger, handle.identity(), BASELINE);
    }
}

/// Go: `TestSet_DenyFailureSkipsEveryGrant`, with the error's text.
#[test]
fn a_failed_revoke_skips_every_grant() {
    let model = Model::new("");
    let ledger = Arc::new(Ledger::default());
    let (cgroup, handle) = model.setup(&ledger);
    cgroup
        .set_device_rules(&handle, &[rule("c", 10, 200, "rw")])
        .unwrap();
    model.state().writes.clear();
    model.fail("deny c 10:200 w");

    let error = cgroup
        .set_device_rules(&handle, &[rule("c", 10, 201, "rwm")])
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        r#"revoke "c 10:200 w": write "c 10:200 w" to devices.deny: write devices.deny: injected"#
    );
    assert!(model.writes_with("allow").is_empty());
    assert_eq!(
        owned_lines(&ledger, handle.identity()),
        ["c 10:200 w"],
        "the bit whose deny failed"
    );
}

/// Go: `TestSet_RestoresExternallyRemovedOwnedBit`.
#[test]
fn an_owned_bit_removed_from_outside_is_restored() {
    let model = Model::new("");
    let ledger = Arc::new(Ledger::default());
    let (cgroup, handle) = model.setup(&ledger);
    let rules = [rule("c", 10, 200, "rw")];
    cgroup.set_device_rules(&handle, &rules).unwrap();

    model.apply(false, "c 10:200 r");
    model.state().writes.clear();
    cgroup.set_device_rules(&handle, &rules).unwrap();
    assert_eq!(model.writes(), ["allow c 10:200 r"]);
    assert_eq!(model.list(), "c 10:200 rw");
}

/// Go: `TestSet_VerifyMismatchIsReported`, with the drift error's text.
#[test]
fn a_write_without_effect_is_reported_as_drift() {
    let model = Model::new("");
    model.state().ignore_writes = true;
    let (cgroup, handle) = model.setup(&Arc::new(Ledger::default()));
    let error = cgroup
        .set_device_rules(&handle, &[rule("c", 10, 200, "r")])
        .unwrap_err();
    assert!(error.is_drift());
    assert_eq!(
        error.to_string(),
        "cgroup v1 device exceptions differ from the daemon-owned set: c 10:200 r (want true)"
    );
}

/// Grant failures are collected and joined with the verification's, as Go's `errors.Join`; a
/// failed read after the writes is a verify error, and a failed first read stops the call.
#[test]
fn grant_failures_are_collected_and_joined() {
    let model = Model::new("");
    let (cgroup, handle) = model.setup(&Arc::new(Ledger::default()));
    model.fail("allow c 10:200 r");
    model.fail("allow c 10:200 w");
    let error = cgroup
        .set_device_rules(&handle, &[rule("c", 10, 200, "rw")])
        .unwrap_err();
    assert_eq!(
        error.to_string(),
        [
            r#"grant "c 10:200 r": write "c 10:200 r" to devices.allow: write devices.allow: injected"#,
            r#"grant "c 10:200 w": write "c 10:200 w" to devices.allow: write devices.allow: injected"#,
            "cgroup v1 device exceptions differ from the daemon-owned set: c 10:200 r (want true), \
             c 10:200 w (want true)",
        ]
        .join("\n")
    );
    assert!(error.is_drift());

    model.fail("read");
    let error = cgroup.set_device_rules(&handle, &[]).unwrap_err();
    assert_eq!(error.to_string(), "open devices.list: injected");
    assert!(!error.is_drift());
}

/// Go: `TestSet_RejectsRulesThatAreNotConcreteAllows`, with the texts.
#[test]
fn rules_that_are_not_concrete_allows_are_refused() {
    let model = Model::new("");
    let (cgroup, handle) = model.setup(&Arc::new(Ledger::default()));
    let mut deny = rule("c", 10, 200, "r");
    deny.allow = false;
    let missing_minor = DeviceRule {
        minor: None,
        ..rule("c", 10, 200, "r")
    };
    let cases = [
        (deny, "cgroup v1 set accepts allow rules only".to_owned()),
        (
            rule("a", 10, 200, "r"),
            "cgroup v1 set needs a c or b rule with concrete major and minor: \
             {Allow:true Type:a Major:10 Minor:200 Access:r}"
                .to_owned(),
        ),
        (
            rule("c", -1, 200, "r"),
            "cgroup v1 set needs a c or b rule with concrete major and minor: \
             {Allow:true Type:c Major:-1 Minor:200 Access:r}"
                .to_owned(),
        ),
        (
            missing_minor,
            "cgroup v1 set needs a c or b rule with concrete major and minor: \
             {Allow:true Type:c Major:10 Minor:<nil> Access:r}"
                .to_owned(),
        ),
        (
            rule("c", 10, 200, "x"),
            "unknown device access 'x'".to_owned(),
        ),
    ];
    for (bad, want) in cases {
        let error = cgroup.set_device_rules(&handle, &[bad]).unwrap_err();
        assert_eq!(error.to_string(), want);
    }
    assert!(model.writes().is_empty());
}

/// Go: `TestSet_LedgerIsKeyedByInode`: a cgroup removed and recreated at the same path; the new
/// directory (other inode) owns nothing, so an empty set on it revokes nothing, while the old
/// identity keeps its record.
#[test]
fn the_ledger_is_keyed_by_inode() {
    let ledger = Arc::new(Ledger::default());
    let old_model = Model::new("");
    let (old_cgroup, old_handle) = old_model.setup(&ledger);
    old_cgroup
        .set_device_rules(&old_handle, &[rule("c", 10, 200, "r")])
        .unwrap();

    let new_model = Model::new("c 10:200 r\n");
    let (new_cgroup, mut new_handle) = new_model.setup(&ledger);
    new_handle
        .identity
        .path
        .clone_from(&old_handle.identity().path);
    assert_ne!(
        new_handle.identity(),
        old_handle.identity(),
        "distinct inodes"
    );

    new_cgroup.set_device_rules(&new_handle, &[]).unwrap();
    assert!(
        new_model.writes().is_empty(),
        "a recreated cgroup starts with nothing owned"
    );
    assert_eq!(owned_lines(&ledger, old_handle.identity()), ["c 10:200 r"]);

    ledger.forget(old_handle.identity());
    assert!(owned_lines(&ledger, old_handle.identity()).is_empty());
}

/// The production files' errors read as Go's `openatFiles` (texts from a scratch Go test).
#[test]
fn openat_file_errors_read_as_go() {
    let dir = TempDir::new();
    std::fs::create_dir(dir.path().join(DEVICES_ALLOW)).unwrap();
    let handle = CgroupHandle::open(dir.path().to_str().unwrap()).unwrap();
    assert_eq!(
        OpenatFiles
            .write(handle.dir(), DEVICES_ALLOW, "c 1:3 r")
            .unwrap_err()
            .to_string(),
        "open devices.allow: is a directory"
    );
    assert_eq!(
        OpenatFiles
            .read(handle.dir(), DEVICES_ALLOW)
            .unwrap_err()
            .to_string(),
        "read devices.allow: read devices.allow: is a directory"
    );
    assert_eq!(
        OpenatFiles
            .read(handle.dir(), DEVICES_LIST)
            .unwrap_err()
            .to_string(),
        "open devices.list: no such file or directory"
    );
}
