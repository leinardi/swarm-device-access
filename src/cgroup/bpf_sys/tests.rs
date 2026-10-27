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

use std::mem::{offset_of, size_of};

use super::*;

/// The attributes are the kernel's `union bpf_attr` members, sized as cilium's.
#[test]
fn attribute_layouts_match_the_kernel_and_go() {
    // The whole member, though the Go daemon passed only the first 32 bytes: the kernel writes
    // `revision` back.
    assert_eq!(size_of::<ProgQueryAttr>(), 64);
    assert_eq!(offset_of!(ProgQueryAttr, attach_type), 4);
    assert_eq!(offset_of!(ProgQueryAttr, query_flags), 8);
    assert_eq!(offset_of!(ProgQueryAttr, attach_flags), 12);
    assert_eq!(offset_of!(ProgQueryAttr, prog_ids), 16);
    assert_eq!(offset_of!(ProgQueryAttr, prog_cnt), 24);
    assert_eq!(offset_of!(ProgQueryAttr, prog_attach_flags), 32);
    assert_eq!(offset_of!(ProgQueryAttr, link_ids), 40);
    assert_eq!(offset_of!(ProgQueryAttr, link_attach_flags), 48);
    assert_eq!(offset_of!(ProgQueryAttr, revision), 56);

    assert_eq!(size_of::<ProgGetFdByIdAttr>(), 4);

    assert_eq!(size_of::<MapCreateAttr>(), 96);
    assert_eq!(offset_of!(MapCreateAttr, map_name), 28);
    assert_eq!(offset_of!(MapCreateAttr, map_ifindex), 44);
    assert_eq!(offset_of!(MapCreateAttr, map_extra), 64);
    assert_eq!(offset_of!(MapCreateAttr, value_type_btf_obj_fd), 72);
    assert_eq!(offset_of!(MapCreateAttr, excl_prog_hash), 80);
    assert_eq!(offset_of!(MapCreateAttr, excl_prog_hash_size), 88);

    assert_eq!(size_of::<ObjGetInfoByFdAttr>(), 16);
    assert_eq!(offset_of!(ObjGetInfoByFdAttr, info_len), 4);
    assert_eq!(offset_of!(ObjGetInfoByFdAttr, info), 8);

    assert_eq!(size_of::<ProgInfo>(), 232);
    assert_eq!(offset_of!(ProgInfo, id), 4);
    assert_eq!(offset_of!(ProgInfo, tag), 8);
    assert_eq!(offset_of!(ProgInfo, jited_prog_len), 16);
    assert_eq!(offset_of!(ProgInfo, xlated_prog_len), 20);
    assert_eq!(offset_of!(ProgInfo, jited_prog_insns), 24);
    assert_eq!(offset_of!(ProgInfo, xlated_prog_insns), 32);
    assert_eq!(offset_of!(ProgInfo, load_time), 40);
    assert_eq!(offset_of!(ProgInfo, created_by_uid), 48);
    assert_eq!(offset_of!(ProgInfo, nr_map_ids), 52);
    assert_eq!(offset_of!(ProgInfo, map_ids), 56);
    assert_eq!(offset_of!(ProgInfo, name), 64);
    assert_eq!(offset_of!(ProgInfo, _rest), 80);

    assert_eq!(size_of::<ProgLoadAttr>(), 168);
    assert_eq!(offset_of!(ProgLoadAttr, insn_cnt), 4);
    assert_eq!(offset_of!(ProgLoadAttr, insns), 8);
    assert_eq!(offset_of!(ProgLoadAttr, license), 16);
    assert_eq!(offset_of!(ProgLoadAttr, log_level), 24);
    assert_eq!(offset_of!(ProgLoadAttr, log_size), 28);
    assert_eq!(offset_of!(ProgLoadAttr, log_buf), 32);
    assert_eq!(offset_of!(ProgLoadAttr, kern_version), 40);
    assert_eq!(offset_of!(ProgLoadAttr, prog_flags), 44);
    assert_eq!(offset_of!(ProgLoadAttr, prog_name), 48);
    assert_eq!(offset_of!(ProgLoadAttr, _middle), 64);
    assert_eq!(offset_of!(ProgLoadAttr, log_true_size), 140);
    assert_eq!(offset_of!(ProgLoadAttr, _rest), 144);

    assert_eq!(size_of::<ProgAttachAttr>(), 32);
    assert_eq!(offset_of!(ProgAttachAttr, attach_bpf_fd), 4);
    assert_eq!(offset_of!(ProgAttachAttr, attach_type), 8);
    assert_eq!(offset_of!(ProgAttachAttr, attach_flags), 12);
    assert_eq!(offset_of!(ProgAttachAttr, replace_bpf_fd), 16);
    assert_eq!(offset_of!(ProgAttachAttr, relative_fd), 20);
    assert_eq!(offset_of!(ProgAttachAttr, expected_revision), 24);

    assert_eq!(size_of::<ProgDetachAttr>(), 32);
    assert_eq!(offset_of!(ProgDetachAttr, attach_bpf_fd), 4);
    assert_eq!(offset_of!(ProgDetachAttr, attach_type), 8);
    assert_eq!(offset_of!(ProgDetachAttr, attach_flags), 12);
    assert_eq!(offset_of!(ProgDetachAttr, relative_fd), 20);
    assert_eq!(offset_of!(ProgDetachAttr, expected_revision), 24);
}

/// The kernel refuses a descriptor that is not a program or a cgroup, and the error comes back
/// as an errno, without privileges.
#[test]
fn a_non_bpf_descriptor_is_refused() {
    let file = std::fs::File::open("/dev/null").unwrap();
    let fd = std::os::fd::AsFd::as_fd(&file);
    let mut ids = [0_u32; 4];
    assert!(prog_query(fd, &mut ids).errno.is_some());
    assert!(prog_info(fd).is_err());
    assert!(prog_detach(fd, fd).is_err());
}
