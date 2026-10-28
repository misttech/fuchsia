// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Shared Stage-1 VMSAv8-64 translation table, page cache, device address space,
//! mode-string parser, and environment abstractions used by both SMMUv2 and SMMUv3.

pub mod constants;
pub mod device_aspace;
pub mod env;
pub mod page_cache;
pub mod smmu_mode;
pub mod translation_table;
pub mod vmsav8_64;

pub use constants::{
    ALL_PERM_FLAGS, ArmSmmuMode, DEFAULT_TCR_T0SZ, DEFAULT_TTBR0_ADDR_SPACE_SZ, INVALID_PADDR,
    IOMMU_FLAG_PERM_EXECUTE, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, PAGE_MASK, PAGE_SHIFT,
    PAGE_SIZE, PAGE_SIZE_USIZE, QueryAddressResult, RequireContiguousMapping, is_page_rounded,
    round_up_page_size,
};
#[cfg(not(is_kernel))]
pub use device_aspace::TlbInvalOp;
pub use device_aspace::{Allocation, DEFAULT_MAX_PAGE_CACHE_ENTRIES, DeviceAspace, TlbInvalidator};
#[cfg(not(is_kernel))]
pub use env::CppPinnedVmObjectRef;
pub use env::{PinnedVmoSource, SmmuEnv, SmmuPage};
pub use page_cache::PageCache;
pub use smmu_mode::{
    get_smmu_mode, get_smmu_mode_bytes, validate_smmu_mode_bytes, validate_smmu_mode_string,
};
#[cfg(any(ktest, not(is_kernel)))]
pub use smmu_mode::{get_smmu_mode_cstr, validate_smmu_mode_cstr};
pub use vmsav8_64::{
    ENTRIES_PER_PAGE, PAGE_ENTRY_ACCESS_FLAG_BIT, PAGE_ENTRY_BIT, PAGE_ENTRY_DBM_BIT, PERM_LUT,
    TABLE_ENTRY_BIT, VALID_ADDR_MASK, VALID_ENTRY_BIT, decode_page_entry_perms,
    get_page_entry_perms, get_table_entry_paddr, is_block_entry, is_table_entry, is_valid_addr,
    is_valid_entry, make_page_entry, make_perms, make_table_entry, write_pte,
};
