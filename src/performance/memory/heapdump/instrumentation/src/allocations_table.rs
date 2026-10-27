// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub use heapdump_vmo::allocations_table_v1::AllocationKey;
use heapdump_vmo::allocations_table_v1::{AllocationsTableWriter, ResourceKey};

/// We cap the size of our backing VMO at 2 GiB, then preallocate it and map it entirely.
/// Actual memory for each page will only be committed when we first write to that page.
const VMO_SIZE: usize = 1 << 31;

const ALLOCATIONS_VMO_NAME: zx::Name = zx::Name::new_lossy("heapdump-allocations");
const CONTROL_VMO_NAME: zx::Name = zx::Name::new_lossy("heapdump-control");

pub const NUM_SHARDS: usize = 32;

/// Tracks live allocations by storing their metadata in a dedicated VMO.
pub struct AllocationsTable {
    vmo: zx::Vmo,
}

/// Mediates write access to a single shard of an `AllocationsTable`.
pub struct AllocationsTableShard {
    writer: AllocationsTableWriter<NUM_SHARDS>,
}

impl AllocationsTable {
    pub fn new() -> (AllocationsTable, [AllocationsTableShard; NUM_SHARDS]) {
        let vmo = zx::Vmo::create(VMO_SIZE as u64).expect("failed to create allocations VMO");
        vmo.set_name(&ALLOCATIONS_VMO_NAME).expect("failed to set VMO name");

        // SAFETY: Nobody else will directly access this VMO and each shard writer operates on a
        // disjoint subset of buckets. Even when we share the VMO with the collector, it will always
        // take a snapshot first and then read the snapshot instead of the original one.
        let writers = unsafe {
            AllocationsTableWriter::new(&vmo, &CONTROL_VMO_NAME).expect("failed to create writers")
        };
        (AllocationsTable { vmo }, writers.map(|writer| AllocationsTableShard { writer }))
    }

    pub fn compute_key(address: u64) -> AllocationKey<NUM_SHARDS> {
        AllocationKey::make(address)
    }

    /// Duplicate the handle to the underlying VMO.
    pub fn share_vmo(&self) -> zx::Vmo {
        self.vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("failed to share allocations VMO")
    }

    // Take a snapshot of the underlying VMO.
    pub fn snapshot_vmo(&self) -> zx::Vmo {
        self.vmo
            .create_child(
                zx::VmoChildOptions::SNAPSHOT | zx::VmoChildOptions::NO_WRITE,
                0,
                VMO_SIZE as u64,
            )
            .expect("failed to snapshot allocations VMO")
    }
}

impl AllocationsTableShard {
    pub fn try_record_allocation(
        &mut self,
        key: AllocationKey<NUM_SHARDS>,
        size: u64,
        thread_info_key: ResourceKey,
        stack_trace_key: ResourceKey,
        timestamp: zx::MonotonicInstant,
    ) -> bool {
        self.writer
            .insert_allocation(key, size, thread_info_key, stack_trace_key, timestamp)
            .expect("out of space")
    }

    pub fn update_allocation(
        &mut self,
        key: AllocationKey<NUM_SHARDS>,
        size: u64,
        thread_info_key: ResourceKey,
        stack_trace_key: ResourceKey,
        timestamp: zx::MonotonicInstant,
    ) -> u64 {
        if let Some(old_size) = self
            .writer
            .replace_allocation(key, size, thread_info_key, stack_trace_key, timestamp)
            .expect("out of space")
        {
            old_size
        } else {
            panic!("Block 0x{:x} was not allocated", key.address());
        }
    }

    pub fn forget_allocation(&mut self, key: AllocationKey<NUM_SHARDS>) -> u64 {
        if let Some(size) = self.writer.erase_allocation(key) {
            size
        } else {
            panic!("Block 0x{:x} was not allocated", key.address());
        }
    }
}
