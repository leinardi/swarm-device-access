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

//! Go has no unit test of `pin.go`: its pinner is faked in the reconcile tests, which come with
//! the reconcile. These tests run the real pidfd and `/proc` reads; every expected error text is
//! the output of a Go program calling `pidfdPinner.pin` and `readProcCgroup` the same way.

use std::process::Command;

use super::*;
use crate::test_support::TempDir;

#[test]
fn pin_refuses_a_pid_that_is_not_a_process() {
    for (pid, want) in [
        (-1, "pidfd_open(-1): invalid argument"),
        (0, "pidfd_open(0): invalid argument"),
    ] {
        let error = PidfdPinner.pin(pid).err().unwrap();
        assert_eq!(error.to_string(), want);
    }
}

#[test]
fn pinned_process_is_alive_until_it_exits() {
    let pinned = PidfdPinner.pin(i64::from(std::process::id())).unwrap();
    assert_eq!(pinned.alive(), Ok(()));

    let mut child = Command::new("true").spawn().unwrap();
    let pinned = PidfdPinner.pin(i64::from(child.id())).unwrap();
    child.wait().unwrap();
    let error = pinned.alive().unwrap_err();
    assert_eq!(error, PinError::Gone(Errno::SRCH));
    assert_eq!(
        error.to_string(),
        "pinned container process exited: no such process"
    );
}

#[test]
fn read_proc_cgroup_of_this_process() {
    let resolved = read_proc_cgroup("/", i64::from(std::process::id())).unwrap();
    assert!(!resolved.mount_point.is_empty(), "{resolved:?}");
}

/// A fake `<root>/proc/<pid>` holding `files`; a name ending in `/` is a directory.
fn fake_proc(pid: u32, files: &[(&str, &str)]) -> TempDir {
    let root = TempDir::new();
    let dir = root.path().join(format!("proc/{pid}"));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, content) in files {
        if let Some(directory) = name.strip_suffix('/') {
            std::fs::create_dir(dir.join(directory)).unwrap();
        } else {
            std::fs::write(dir.join(name), content).unwrap();
        }
    }
    root
}

#[test]
fn read_proc_cgroup_error_texts() {
    let missing = TempDir::new();
    let missing_root = missing.path().to_str().unwrap().to_owned();
    assert_eq!(
        read_proc_cgroup(&missing_root, 1).unwrap_err().to_string(),
        format!("open \"{missing_root}/proc/1\": no such file or directory")
    );

    let no_mountinfo = fake_proc(77, &[("cgroup", "0::/x\n")]);
    assert_eq!(
        read_proc_cgroup(no_mountinfo.path().to_str().unwrap(), 77)
            .unwrap_err()
            .to_string(),
        "open mountinfo: no such file or directory"
    );

    let malformed = fake_proc(78, &[("cgroup", "garbage\n"), ("mountinfo", "")]);
    assert_eq!(
        read_proc_cgroup(malformed.path().to_str().unwrap(), 78)
            .unwrap_err()
            .to_string(),
        "resolve cgroup of pid 78: malformed cgroup entry: garbage"
    );

    let unreadable = fake_proc(79, &[("cgroup/", "")]);
    assert_eq!(
        read_proc_cgroup(unreadable.path().to_str().unwrap(), 79)
            .unwrap_err()
            .to_string(),
        "read cgroup: read cgroup: is a directory"
    );
}
