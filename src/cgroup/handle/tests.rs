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

//! Go's `TestOpenCgroup_RecordsInode`, and the membership check.

use std::os::unix::fs::MetadataExt as _;

use super::*;
use crate::test_support::TempDir;

/// Go: `TestOpenCgroup_RecordsInode`.
#[test]
fn opening_records_the_inode() {
    let dir = TempDir::new();
    let path = dir.path().to_str().unwrap();
    let handle = CgroupHandle::open(path).unwrap();
    assert_eq!(
        handle.identity(),
        &Identity {
            path: path.to_owned(),
            inode: std::fs::metadata(path).unwrap().ino(),
        }
    );
}

/// A pid is a member when `cgroup.procs` lists it as a whole field, read through the descriptor
/// even after the directory is renamed.
#[test]
fn membership_reads_cgroup_procs_through_the_descriptor() {
    let dir = TempDir::new();
    let cgroup = dir.path().join("cgroup");
    std::fs::create_dir(&cgroup).unwrap();
    std::fs::write(cgroup.join("cgroup.procs"), "1\n234\n").unwrap();
    let handle = CgroupHandle::open(cgroup.to_str().unwrap()).unwrap();
    std::fs::rename(&cgroup, dir.path().join("moved")).unwrap();
    assert!(handle.has_process(234).unwrap());
    assert!(handle.has_process(1).unwrap());
    assert!(!handle.has_process(23).unwrap());
}

/// The errors read as Go's.
#[test]
fn errors_read_as_go() {
    let dir = TempDir::new();
    let missing = dir.path().join("missing");
    let missing = missing.to_str().unwrap();
    assert_eq!(
        CgroupHandle::open(missing).unwrap_err().to_string(),
        format!(r#"open cgroup "{missing}": no such file or directory"#)
    );
    let path = dir.path().to_str().unwrap();
    let handle = CgroupHandle::open(path).unwrap();
    assert_eq!(
        handle.has_process(1).unwrap_err().to_string(),
        format!(r#"open cgroup.procs of "{path}": no such file or directory"#)
    );
    std::fs::create_dir(dir.path().join("cgroup.procs")).unwrap();
    assert_eq!(
        handle.has_process(1).unwrap_err().to_string(),
        format!(r#"read cgroup.procs of "{path}": read cgroup.procs: is a directory"#)
    );
    let file = dir.path().join("file");
    std::fs::write(&file, "").unwrap();
    let file = file.to_str().unwrap();
    assert_eq!(
        CgroupHandle::open(file).unwrap_err().to_string(),
        format!(r#"open cgroup "{file}": not a directory"#)
    );
}
