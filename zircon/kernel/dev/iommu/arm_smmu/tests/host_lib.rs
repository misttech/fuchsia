// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Host test crate root for the ARM SMMU Rust driver port.

#![no_std]
#![allow(dead_code)]

#[path = "../mod.rs"]
pub mod arm_smmu;

use zx_status::Status;

use crate::arm_smmu::common::{
    ArmSmmuMode, DeviceAspace, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, PAGE_SIZE, PageCache,
    PinnedVmoSource, TlbInvalidator, get_smmu_mode, validate_smmu_mode_string,
};

struct SmokePinnedVmo {
    offset: u64,
    size: u64,
    base_pa: u64,
}

impl PinnedVmoSource for SmokePinnedVmo {
    fn offset(&self) -> u64 {
        self.offset
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn lookup_contiguous(&self, offset: u64, size: u64) -> Result<u64, Status> {
        if size != PAGE_SIZE || offset < self.offset || (offset - self.offset) >= self.size {
            return Err(Status::INVALID_ARGS);
        }
        Ok(self.base_pa + (offset - self.offset))
    }
}

struct SmokeInvalidator {
    count: usize,
    last_base: u64,
    last_size: u64,
}

impl TlbInvalidator for SmokeInvalidator {
    fn invalidate(&mut self, base: u64, size: u64) {
        self.count += 1;
        self.last_base = base;
        self.last_size = size;
    }
}

/// Executes a host smoke test exercising
/// `common/{smmu_mode,page_cache,translation_table,device_aspace}` and verifying that all `SmmuEnv`
/// FFI hooks link and run against `smmu_env_ffi.cc` and `PmmMock`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_arm_smmu_common_smoke_test() -> bool {
    if get_smmu_mode("disabled, 0x1234 = passthru, 0x5678 = enforced", Some(0x1234))
        != Some(ArmSmmuMode::Passthru)
    {
        return false;
    }
    if !validate_smmu_mode_string("disabled, 0x1234 = passthru")
        || validate_smmu_mode_string("disabled,")
    {
        return false;
    }

    // Exercise PageCache LIFO ordering and trimming.
    let mut cache = PageCache::new();
    let Ok(p1) = cache.get_page() else {
        return false;
    };
    let Ok(p2) = cache.get_page() else {
        return false;
    };
    // SAFETY: `p1` and `p2` were just checked out from `cache`, are unlinked, and remain zeroed
    // and cache-cleaned.
    unsafe {
        cache.return_page(p1);
        cache.return_page(p2);
    }
    let Ok(pop1) = cache.get_page() else {
        return false;
    };
    let Ok(pop2) = cache.get_page() else {
        return false;
    };
    if pop1 != p2 || pop2 != p1 {
        return false;
    }
    // SAFETY: `pop1` and `pop2` were just checked out from `cache`, are unlinked, and remain
    // zeroed and cache-cleaned.
    unsafe {
        cache.return_page(pop1);
        cache.return_page(pop2);
    }
    cache.trim(0);

    // Exercise DeviceAspace map/unmap straddling all 4 translation table levels.
    let aspace_start = 1u64 << 20;
    let aspace_end = 1u64 << 48;
    let Ok(mut aspace) = DeviceAspace::create(aspace_start, aspace_end - aspace_start, 0) else {
        return false;
    };
    let vmo = SmokePinnedVmo { offset: 0, size: 2 * PAGE_SIZE, base_pa: 0x1000 };
    let mut inval = SmokeInvalidator { count: 0, last_base: 0, last_size: 0 };
    let target_loc = (1u64 << 39) - PAGE_SIZE;

    let Ok(alloc) = aspace.map(
        &vmo,
        IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE,
        &mut inval,
        Some(target_loc),
    ) else {
        return false;
    };
    if alloc.base() != target_loc || alloc.size() != 2 * PAGE_SIZE || inval.count != 1 {
        return false;
    }

    aspace.unmap(alloc, &mut inval);
    if inval.count != 2 || inval.last_base != target_loc || inval.last_size != 2 * PAGE_SIZE {
        return false;
    }

    aspace.free_translation_tables(&mut inval);
    inval.count == 3 && inval.last_base == 0 && inval.last_size == 0
}
