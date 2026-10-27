// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod atomic_vec;
pub use atomic_vec::{AtomicBitVec, Growable, NoOverflow, Overflow};

use memory_mapped_vmo::{ReadOnlyMemoryMappedVmo, ReadWriteMemoryMappedVmo};
use std::sync::atomic::{AtomicU64, Ordering};
use zx::Rights;

/// Stores monotonic counters of page refaults and total page faults in a shared VMO.
///
/// Both counters are measured in **pages supplied** by the userspace pager (including readahead),
/// rather than the number of pager requests:
/// - `refault_count_ptr` (offset 0): pages supplied that had already been supplied before and were
///   subsequently evicted.
/// - `fault_count_ptr` (offset 8): total pages supplied by the userspace pager.
pub struct PageFaultCounter {
    vmo: zx::Vmo,
    _storage: ReadOnlyMemoryMappedVmo,
    refault_count_ptr: *const AtomicU64,
    fault_count_ptr: *const AtomicU64,
}

// SAFETY: both `vmo` and `_storage` are Send, and they are not modified once created (thus, they
// are Sync). `refault_count_ptr` and `fault_count_ptr` are pointers pointing to the memory mapped
// region managed by `ReadOnlyMemoryMappedVmo`. They are valid as long as `_storage` is valid, and
// are not invalidated by moving `PageFaultCounter` as the memory mapped region stays in the same
// place.
unsafe impl Send for PageFaultCounter {}
unsafe impl Sync for PageFaultCounter {}

const VMO_SIZE: usize = 2 * size_of::<AtomicU64>();
const REFAULT_OFFSET: usize = 0;
const FAULT_OFFSET: usize = size_of::<AtomicU64>();

impl PageFaultCounter {
    /// Creates a new read-write PageFaultCounter.
    pub fn new() -> Result<Self, zx::Status> {
        let vmo = zx::Vmo::create(VMO_SIZE.try_into().unwrap())?;
        vmo.set_name(&zx::Name::new_lossy("page_fault_counter"))?;

        // SAFETY: all accesses to [storage] are synchronized (through Atomics).
        let mut storage = unsafe { ReadWriteMemoryMappedVmo::new(&vmo)? };
        let refault_count_ptr: *const AtomicU64 = storage
            .get_object_mut::<AtomicU64>(REFAULT_OFFSET)
            .map_err(|_| zx::Status::INVALID_ARGS)?;
        let fault_count_ptr: *const AtomicU64 = storage
            .get_object_mut::<AtomicU64>(FAULT_OFFSET)
            .map_err(|_| zx::Status::INVALID_ARGS)?;
        Ok(PageFaultCounter {
            vmo,
            _storage: storage.into_readonly(),
            refault_count_ptr,
            fault_count_ptr,
        })
    }

    /// Creates a new read-only PageFaultCounter from the provided VMO. The VMO must have the
    /// READ, MAP, and GET_PROPERTY rights.
    pub fn from_vmo_readonly(vmo: zx::Vmo) -> Result<Self, zx::Status> {
        if vmo.get_size()? < VMO_SIZE.try_into().unwrap() {
            return Err(zx::Status::INVALID_ARGS);
        }
        // SAFETY: all accesses to [storage] are synchronized (through Atomics).
        let storage = unsafe { ReadOnlyMemoryMappedVmo::new(&vmo)? };
        let refault_count_ptr: *const AtomicU64 = storage
            .get_object::<AtomicU64>(REFAULT_OFFSET)
            .map_err(|_| zx::Status::INVALID_ARGS)?;
        let fault_count_ptr: *const AtomicU64 =
            storage.get_object::<AtomicU64>(FAULT_OFFSET).map_err(|_| zx::Status::INVALID_ARGS)?;
        Ok(PageFaultCounter { vmo, _storage: storage, refault_count_ptr, fault_count_ptr })
    }

    /// Increments the page refault counter by `count` pages (pages resupplied after eviction).
    pub fn increment_refaults(&self, count: u64, order: Ordering) {
        // SAFETY: `self.refault_count_ptr` is non-null per construction, and valid as long as
        // `_storage` is valid.
        unsafe { &*self.refault_count_ptr }.fetch_add(count, order);
    }

    /// Reads the page refault counter (in pages).
    pub fn read_refaults(&self, order: Ordering) -> u64 {
        // SAFETY: `self.refault_count_ptr` is non-null per construction, and valid as long as
        // `_storage` is valid.
        unsafe { &*self.refault_count_ptr }.load(order)
    }

    /// Increments the total page fault counter by `count` pages (total pages supplied by the
    /// userspace pager, including readahead).
    pub fn increment_faults(&self, count: u64, order: Ordering) {
        // SAFETY: `self.fault_count_ptr` is non-null per construction, and valid as long as
        // `_storage` is valid.
        unsafe { &*self.fault_count_ptr }.fetch_add(count, order);
    }

    /// Reads the total page fault counter (in pages supplied).
    pub fn read_faults(&self, order: Ordering) -> u64 {
        // SAFETY: `self.fault_count_ptr` is non-null per construction, and valid as long as
        // `_storage` is valid.
        unsafe { &*self.fault_count_ptr }.load(order)
    }

    /// Returns a read-only handle for the backing VMO.
    pub fn readonly_vmo(&self) -> Result<zx::Vmo, zx::Status> {
        self.vmo.duplicate_handle(Rights::BASIC | Rights::READ | Rights::MAP | Rights::GET_PROPERTY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_page_fault_counter() {
        let counter = PageFaultCounter::new().unwrap();

        let ro_vmo = counter.readonly_vmo().unwrap();
        let ro_counter = PageFaultCounter::from_vmo_readonly(ro_vmo).unwrap();

        counter.increment_refaults(100, Ordering::SeqCst);
        counter.increment_faults(250, Ordering::SeqCst);
        assert_eq!(ro_counter.read_refaults(Ordering::SeqCst), 100);
        assert_eq!(ro_counter.read_faults(Ordering::SeqCst), 250);

        counter.increment_refaults(100, Ordering::SeqCst);
        counter.increment_faults(50, Ordering::SeqCst);
        assert_eq!(ro_counter.read_refaults(Ordering::SeqCst), 200);
        assert_eq!(ro_counter.read_faults(Ordering::SeqCst), 300);
    }
}
