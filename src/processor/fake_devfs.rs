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

//! Go's `fakeDevFS` of `rules_test.go`, shared by the collection tests and the reconcile tests
//! that drive `compute_desired` over it.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;

use rustix::io::Errno;

use super::devfs::{DevFs, DevNode, DeviceId, DeviceType, NodeKind, SysError, Visit, walk_dir};
use crate::test_support::TempDir;

/// Go's `fakeDevFS`: scripts what each name under `/dev` resolves to, as `openat2` beneath `/dev`
/// would. Every registered name also exists as a file (or a directory) under `dir`, so directory
/// mounts can be enumerated.
pub(super) struct FakeDevFs {
    pub(super) dir: TempDir,
    root: String,
    pub(super) nodes: HashMap<String, FakeNode>,
    pub(super) errs: HashMap<String, SysError>,
    pub(super) links: HashMap<String, String>,
    pub(super) uevents: HashMap<DeviceId, String>,
    /// An error for a name (relative to `dir`) injected into the walk.
    pub(super) walk_errs: HashMap<String, SysError>,
}

#[derive(Debug, Clone)]
pub(super) struct FakeNode {
    pub(super) kind: NodeKind,
    /// The resolved `/dev` path.
    pub(super) path: String,
}

impl DevNode for FakeNode {
    fn stat(&self) -> Result<NodeKind, SysError> {
        Ok(self.kind)
    }

    fn resolved(&self) -> String {
        self.path.clone()
    }
}

impl FakeDevFs {
    pub(super) fn new() -> Self {
        let dir = TempDir::new();
        let root = dir.path().to_str().unwrap().to_owned();
        Self {
            dir,
            root,
            nodes: HashMap::new(),
            errs: HashMap::new(),
            links: HashMap::new(),
            uevents: HashMap::new(),
            walk_errs: HashMap::new(),
        }
    }

    /// Creates the name under `dir` so a walk enumerates it.
    pub(super) fn touch(&self, rel: &str) {
        let path = self.dir.path().join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, b"").unwrap();
    }

    /// Registers a device node at `rel` with its sysfs `DEVNAME`.
    pub(super) fn device(&mut self, rel: &str, id: DeviceId, devname: &str) {
        self.touch(rel);
        self.nodes.insert(
            rel.to_owned(),
            FakeNode {
                kind: NodeKind::Device(id),
                path: format!("/dev/{rel}"),
            },
        );
        self.uevents
            .insert(id, format!("MAJOR={}\nDEVNAME={devname}\n", id.major));
    }

    /// Registers a symlink at `rel` the kernel follows (relative, inside `/dev`) to the node
    /// registered at `target`.
    pub(super) fn alias(&mut self, rel: &str, target: &str) {
        self.touch(rel);
        let node = self.nodes[target].clone();
        self.nodes.insert(rel.to_owned(), node);
    }

    /// Registers an absolute symlink at `rel`: `openat2` beneath `/dev` refuses it with `EXDEV`,
    /// and `read_link` returns `target`.
    pub(super) fn abs_link(&mut self, rel: &str, target: &str) {
        self.touch(rel);
        self.errs.insert(
            rel.to_owned(),
            SysError::new(format!("openat2 {rel}"), Errno::XDEV),
        );
        self.links.insert(rel.to_owned(), target.to_owned());
    }

    /// Registers `rel` as a directory.
    pub(super) fn directory(&mut self, rel: &str) {
        std::fs::create_dir_all(self.dir.path().join(rel)).unwrap();
        self.nodes.insert(
            rel.to_owned(),
            FakeNode {
                kind: NodeKind::Dir,
                path: format!("/dev/{rel}"),
            },
        );
    }

    /// Registers `rel` as a regular file resolving to `path`.
    pub(super) fn other(&mut self, rel: &str, kind: NodeKind, path: &str) {
        self.touch(rel);
        self.nodes.insert(
            rel.to_owned(),
            FakeNode {
                kind,
                path: path.to_owned(),
            },
        );
    }
}

impl DevFs for FakeDevFs {
    fn root(&self) -> &str {
        &self.root
    }

    fn open_beneath(&self, rel: &str) -> Result<Box<dyn DevNode>, SysError> {
        // A component that fails the lookup fails every name below it.
        let parts: Vec<&str> = rel.split('/').collect();
        for index in 0..parts.len() {
            if let Some(error) = self.errs.get(&parts[..=index].join("/")) {
                return Err(error.clone());
            }
        }
        match self.nodes.get(rel) {
            Some(node) => Ok(Box::new(node.clone())),
            None => Err(SysError::new(format!("openat2 {rel}"), Errno::NOENT)),
        }
    }

    fn read_link(&self, rel: &str) -> Result<String, SysError> {
        self.links
            .get(rel)
            .cloned()
            .ok_or_else(|| SysError::new("", Errno::INVAL))
    }

    fn read_uevent(&self, id: DeviceId) -> Result<Vec<u8>, SysError> {
        self.uevents
            .get(&id)
            .map(|data| data.as_bytes().to_vec())
            .ok_or_else(|| SysError::new(format!("read uevent {id}"), Errno::NOENT))
    }

    fn walk(&self, base: &str, visit: &mut Visit<'_>) {
        walk_dir(base, &mut |path, is_dir, error| {
            let rel = Path::new(path)
                .strip_prefix(&self.root)
                .ok()
                .and_then(Path::to_str)
                .unwrap_or_default();
            match (error, self.walk_errs.get(rel)) {
                (None, Some(injected)) => visit(path, is_dir, Some(injected.clone())),
                (error, _) => visit(path, is_dir, error),
            }
        });
    }
}

pub(super) const fn char_dev(major: u32, minor: u32) -> DeviceId {
    DeviceId {
        kind: DeviceType::Char,
        major,
        minor,
    }
}

pub(super) const DEV_SDA: DeviceId = DeviceId {
    kind: DeviceType::Block,
    major: 8,
    minor: 0,
};
pub(super) const DEV_CARD: DeviceId = char_dev(226, 0);
pub(super) const DEV_REND: DeviceId = char_dev(226, 128);
pub(super) const DEV_NULL: DeviceId = char_dev(1, 3);

/// The fake shared with a processor's `/dev` seam.
impl DevFs for Arc<FakeDevFs> {
    fn root(&self) -> &str {
        self.as_ref().root()
    }

    fn open_beneath(&self, rel: &str) -> Result<Box<dyn DevNode>, SysError> {
        self.as_ref().open_beneath(rel)
    }

    fn read_link(&self, rel: &str) -> Result<String, SysError> {
        self.as_ref().read_link(rel)
    }

    fn read_uevent(&self, id: DeviceId) -> Result<Vec<u8>, SysError> {
        self.as_ref().read_uevent(id)
    }

    fn walk(&self, base: &str, visit: &mut Visit<'_>) {
        self.as_ref().walk(base, visit);
    }
}
