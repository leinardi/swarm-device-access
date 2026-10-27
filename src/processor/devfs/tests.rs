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

//! Go's `devfs.go` tests live in `rules_test.go`: `TestProbeOpenat2` and
//! `TestRealDevFS_Openat2Containment` are here, `TestCollect_RealDevNull` with the collection
//! tests.

use std::os::unix::fs::symlink;

use super::*;
use crate::test_support::TempDir;

/// Go: `TestProbeOpenat2`.
#[test]
fn probe_openat2_succeeds_or_reports_unsupported() {
    let result = probe_openat2();
    assert!(
        matches!(result, Ok(()) | Err(ProbeError::Unsupported(_))),
        "probe_openat2 = {result:?}, want Ok or Unsupported"
    );
}

/// Go: `TestRealDevFS_Openat2Containment`. The real `openat2` against regular files:
/// `RESOLVE_BENEATH` refuses an absolute symlink and a relative one that climbs out, and
/// `read_link` returns the link itself.
#[test]
fn real_dev_fs_openat2_containment() {
    if probe_openat2().is_err() {
        // Go skips without openat2; the daemon refuses to start there.
        return;
    }
    let outside_dir = TempDir::new();
    let outside = outside_dir.path().join("secret");
    let root = TempDir::new();
    std::fs::write(&outside, b"").unwrap();
    std::fs::write(root.path().join("inside"), b"").unwrap();
    symlink(&outside, root.path().join("abs")).unwrap();
    let climb = format!(
        "../{}/secret",
        outside_dir.path().file_name().unwrap().to_str().unwrap()
    );
    symlink(climb, root.path().join("climb")).unwrap();
    symlink("inside", root.path().join("rel")).unwrap();

    let dev = RealDevFs::open(root.path().to_str().unwrap(), "/sys").unwrap();
    for name in ["abs", "climb"] {
        let errno = dev.open_beneath(name).err().map(|error| error.errno());
        assert_eq!(errno, Some(Errno::XDEV), "open_beneath({name})");
    }
    assert_eq!(
        dev.read_link("abs"),
        Ok(outside.to_str().unwrap().to_owned())
    );
    let node = dev.open_beneath("rel").unwrap();
    assert_eq!(node.stat(), Ok(NodeKind::Other));
    assert_eq!(node.resolved(), "/dev/inside");
}

/// The texts of the real seam's failures, each the output of the Go code failing the same way.
#[test]
fn real_dev_fs_error_texts() {
    let missing = TempDir::new();
    let gone = missing.path().join("gone");
    let gone = gone.to_str().unwrap();
    assert_eq!(
        RealDevFs::open(gone, "/sys").unwrap_err().to_string(),
        format!("open {gone}: no such file or directory")
    );

    let root = TempDir::new();
    let dev = RealDevFs::open(root.path().to_str().unwrap(), gone).unwrap();
    assert_eq!(
        dev.open_beneath("nope").err().unwrap().to_string(),
        "openat2 nope: no such file or directory"
    );
    std::fs::write(root.path().join("file"), b"").unwrap();
    assert_eq!(
        dev.read_link("file").unwrap_err().to_string(),
        "readlink file: no such file or directory"
    );
    let uevent = format!("{gone}/dev/char/1:3/uevent");
    assert_eq!(
        dev.read_uevent(DeviceId {
            kind: DeviceType::Char,
            major: 1,
            minor: 3,
        })
        .unwrap_err()
        .to_string(),
        format!("read {uevent}: open {uevent}: no such file or directory")
    );
}

/// The real `/dev/null` is a character device 1:3 whose sysfs uevent names it.
#[test]
fn real_dev_fs_identifies_dev_null() {
    if probe_openat2().is_err() {
        return;
    }
    let dev = RealDevFs::open(DEV_ROOT, "/sys").unwrap();
    let null = DeviceId {
        kind: DeviceType::Char,
        major: 1,
        minor: 3,
    };
    let node = dev.open_beneath("null").unwrap();
    assert_eq!(node.stat(), Ok(NodeKind::Device(null)));
    assert_eq!(node.resolved(), "/dev/null");
    let uevent = String::from_utf8(dev.read_uevent(null).unwrap()).unwrap();
    assert!(
        uevent.lines().any(|line| line == "DEVNAME=null"),
        "{uevent}"
    );
}

/// Go's `filepath.WalkDir` order: the root, then every name in lexical order, directories before
/// their contents; a directory that cannot be listed is visited again with the error.
#[test]
fn walk_dir_visits_in_lexical_order() {
    let root = TempDir::new();
    std::fs::create_dir_all(root.path().join("b/c")).unwrap();
    std::fs::write(root.path().join("b/c/z"), b"").unwrap();
    std::fs::write(root.path().join("a"), b"").unwrap();
    std::fs::write(root.path().join("b/0"), b"").unwrap();
    let base = root.path().to_str().unwrap();

    let mut seen = Vec::new();
    walk_dir(base, &mut |path, is_dir, error| {
        let rel = path.strip_prefix(base).unwrap().to_owned();
        seen.push((rel, is_dir, error.map(|error| error.to_string())));
        WalkControl::Continue
    });
    let want: Vec<(String, bool, Option<String>)> = [
        ("", true),
        ("/a", false),
        ("/b", true),
        ("/b/0", false),
        ("/b/c", true),
        ("/b/c/z", false),
    ]
    .into_iter()
    .map(|(rel, is_dir)| (rel.to_owned(), is_dir, None))
    .collect();
    assert_eq!(seen, want);

    let mut visits = 0;
    walk_dir(base, &mut |_, _, _| {
        visits += 1;
        if visits == 2 {
            WalkControl::SkipAll
        } else {
            WalkControl::Continue
        }
    });
    assert_eq!(visits, 2, "SkipAll ends the walk");

    let gone = format!("{base}/gone");
    let mut errors = Vec::new();
    walk_dir(&gone, &mut |path, _, error| {
        errors.push((path.to_owned(), error.map(|error| error.to_string())));
        WalkControl::Continue
    });
    assert_eq!(
        errors,
        [(
            gone.clone(),
            Some(format!("lstat {gone}: no such file or directory"))
        )]
    );
}

#[test]
fn relative_beneath_keeps_only_names_under_the_directory() {
    for (dir, target, want) in [
        ("/dev", "/dev", Some(".")),
        ("/dev", "/dev/dri/card0", Some("dri/card0")),
        ("/dev/", "/dev/null", Some("null")),
        ("/dev", "/devices/x", None),
        ("/dev", "/tmp/x", None),
        ("/dev", "relative", None),
        ("/", "/x", Some("x")),
    ] {
        assert_eq!(
            relative_beneath(dir, target).as_deref(),
            want,
            "{dir} {target}"
        );
    }
}

#[test]
fn sysfs_root_prefers_the_host_mount() {
    let host = TempDir::new();
    let host_root = host.path().to_str().unwrap();
    assert_eq!(sysfs_root_for(host_root), "/sys");
    std::fs::create_dir_all(host.path().join("sys/dev")).unwrap();
    assert_eq!(sysfs_root_for(host_root), format!("{host_root}/sys"));
}

#[test]
fn device_id_and_sys_error_texts() {
    let sda = DeviceId {
        kind: DeviceType::Block,
        major: 8,
        minor: 0,
    };
    assert_eq!(sda.to_string(), "b 8:0");
    assert_eq!(
        SysError::new("openat2 sda", Errno::XDEV).to_string(),
        "openat2 sda: invalid cross-device link"
    );
    assert_eq!(
        SysError::new("", Errno::INVAL).to_string(),
        "invalid argument"
    );
}
