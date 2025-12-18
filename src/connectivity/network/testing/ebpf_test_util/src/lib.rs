// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ebpf_api::{AttachType, ProgramType};
use fidl_fuchsia_ebpf as febpf;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

// Results struct. Must match the `struct test_result` in
// `ebpf_test_progs.c`.
#[derive(FromBytes, Immutable, KnownLayout)]
#[repr(C)]
// LINT.IfChange
pub struct TestResult {
    pub cookie: u64,
    pub uid: u32,
    pub ifindex: u32,
    pub proto: u32,
    pub ip_proto: u8,
}
// LINT.ThenChange(//src/connectivity/network/testing/ebpf_test_util/ebpf/ebpf_test_progs.c)

/// Loads the test program, verifies it for the specified `program_type`,
/// initializes all required maps and returns the verified program and the maps.
pub fn load_test_program(
    program_type: ProgramType,
) -> (febpf::VerifiedProgram, Vec<ebpf_api::PinnedMap>) {
    let prog =
        ebpf_loader::load_ebpf_program("/pkg/data/ebpf_test_progs.o", ".text", "skb_test_prog")
            .expect("Failed to load test prog");
    let maps_schema = prog.maps.iter().map(|m| m.schema).collect();
    let calling_context = program_type
        .create_calling_context(AttachType::Unspecified, maps_schema)
        .expect("Failed to create CallingContext");
    let verified = ebpf::verify_program(prog.code, calling_context, &mut ebpf::NullVerifierLogger)
        .expect("Failed to verify loaded program");

    let maps: Vec<_> = prog
        .maps
        .iter()
        .map(|def| ebpf_api::Map::new(def.schema, &def.name()).expect("Failed to create a map"))
        .collect();
    let shared_maps = maps.iter().map(|map| map.share().expect("Failed to share a map")).collect();

    let code: Vec<u64> = <[u64]>::ref_from_bytes(verified.code().as_bytes()).unwrap().to_owned();
    let struct_access_instructions = verified
        .struct_access_instructions()
        .iter()
        .map(|s| febpf::StructAccess {
            pc: s.pc.try_into().unwrap(),
            struct_memory_id: s.memory_id.id(),
            field_offset: s.field_offset.try_into().unwrap(),
            is_32_bit_ptr_load: s.is_32_bit_ptr_load,
        })
        .collect();

    (
        febpf::VerifiedProgram {
            code: Some(code),
            struct_access_instructions: Some(struct_access_instructions),
            maps: Some(shared_maps),
            __source_breaking: Default::default(),
        },
        maps,
    )
}
