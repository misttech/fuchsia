// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ebpf_api::{AttachType, ProgramType};
use fidl_fuchsia_ebpf as febpf;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zx::{AsHandleRef as _, HandleBased as _};

// Results struct. Must match the `struct test_result` in
// `ebpf_test_progs.c`.
#[derive(Clone, FromBytes, Immutable, KnownLayout)]
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

#[derive(Debug)]
pub struct TestProgram {
    handle: febpf::ProgramHandle,
    server_handle: zx::EventPair,
    program: ebpf::VerifiedEbpfProgram,
    maps: Vec<ebpf_api::PinnedMap>,
}

impl TestProgram {
    /// Loads the test program, verifies it for the specified `program_type`.
    pub fn load(program_type: ProgramType) -> Self {
        let prog =
            ebpf_loader::load_ebpf_program("/pkg/data/ebpf_test_progs.o", ".text", "skb_test_prog")
                .expect("Failed to load test prog");
        let maps_schema = prog.maps.iter().map(|m| m.schema).collect();
        let calling_context = program_type
            .create_calling_context(AttachType::Unspecified, maps_schema)
            .expect("Failed to create CallingContext");
        let verified =
            ebpf::verify_program(prog.code, calling_context, &mut ebpf::NullVerifierLogger)
                .expect("Failed to verify loaded program");

        let maps = prog
            .maps
            .iter()
            .map(|def| ebpf_api::Map::new(def.schema, &def.name()).expect("Failed to create a map"))
            .collect();

        let (handle, server_handle) = zx::EventPair::create();
        let handle = febpf::ProgramHandle { handle };
        Self { program: verified, maps, handle, server_handle }
    }

    pub fn maps(&self) -> &[ebpf_api::PinnedMap] {
        &self.maps
    }

    pub fn get_fidl_program(&self) -> febpf::VerifiedProgram {
        let code: Vec<u64> =
            <[u64]>::ref_from_bytes(self.program.code().as_bytes()).unwrap().to_owned();
        let struct_access_instructions = self
            .program
            .struct_access_instructions()
            .iter()
            .map(|s| febpf::StructAccess {
                pc: s.pc.try_into().unwrap(),
                struct_memory_id: s.memory_id.id(),
                field_offset: s.field_offset.try_into().unwrap(),
                is_32_bit_ptr_load: s.is_32_bit_ptr_load,
            })
            .collect();
        febpf::VerifiedProgram {
            code: Some(code),
            struct_access_instructions: Some(struct_access_instructions),
            maps: Some(self.maps.iter().map(|m| m.share().expect("share map")).collect()),
            __source_breaking: Default::default(),
        }
    }

    pub fn get_program_handle(&self) -> febpf::ProgramHandle {
        febpf::ProgramHandle {
            handle: self
                .handle
                .handle
                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                .expect("duplicate handle"),
        }
    }

    pub fn get_program_id(&self) -> febpf::ProgramId {
        febpf::ProgramId { id: self.handle.handle.get_koid().expect("get koid").raw_koid() }
    }

    // Mark the program defunct and return the server handle.
    pub fn mark_defunct(self) -> zx::EventPair {
        let Self { handle, server_handle, .. } = self;
        handle
            .handle
            .signal_handle(
                zx::Signals::empty(),
                zx::Signals::from_bits_truncate(febpf::PROGRAM_DEFUNCT_SIGNAL),
            )
            .expect("signal EventPair");
        server_handle
    }

    pub fn read_test_result(&self) -> TestResult {
        let result = self.maps[0].load(&[0; 4]).expect("retrieve test result");
        TestResult::ref_from_bytes(&result).expect("convert test results struct").clone()
    }
}
