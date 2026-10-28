// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Common constants, permission flags, and operating mode enumerations for ARM SMMU drivers.

#[cfg(is_kernel)]
pub use crate::object::{
    IOMMU_FLAG_PERM_EXECUTE, IOMMU_FLAG_PERM_READ, IOMMU_FLAG_PERM_WRITE, QueryAddressResult,
};

/// Permission flag allowing device read access to a mapped region.
#[cfg(not(is_kernel))]
pub const IOMMU_FLAG_PERM_READ: u32 = 1 << 0;

/// Permission flag allowing device write access to a mapped region.
#[cfg(not(is_kernel))]
pub const IOMMU_FLAG_PERM_WRITE: u32 = 1 << 1;

/// Permission flag allowing device instruction-fetch/execute access to a mapped region.
#[cfg(not(is_kernel))]
pub const IOMMU_FLAG_PERM_EXECUTE: u32 = 1 << 2;

/// Bitmask of all valid IOMMU permission flags.
pub const ALL_PERM_FLAGS: u32 =
    IOMMU_FLAG_PERM_READ | IOMMU_FLAG_PERM_WRITE | IOMMU_FLAG_PERM_EXECUTE;

/// The sentinel value used to indicate that a physical address is invalid.
///
/// IOMMUs, just like CPU MMUs, work in memory units of pages which are at least
/// 4KB in length and 4KB aligned. Page sizes can go up from there (16KB, 64KB,
/// etc) but are always "page aligned" implying that a valid page address always
/// has the lowest N bits cleared. This address is all 1s, meaning that it will
/// never be a valid physical page address for any system unless the page size
/// was a single byte (which just does not happen).
pub const INVALID_PADDR: u64 = u64::MAX;

/// We currently assume and only support 4k pages.
pub const PAGE_SHIFT: u32 = 12;

/// Size of a 4 KiB translation granule in bytes (`u64`).
pub const PAGE_SIZE: u64 = 1 << PAGE_SHIFT;

/// Size of a 4 KiB translation granule in bytes (`usize`).
pub const PAGE_SIZE_USIZE: usize = 1 << PAGE_SHIFT;

/// Low-bit mask for a 4 KiB translation granule (`0xfff`).
pub const PAGE_MASK: u64 = PAGE_SIZE - 1;

/// When we configure for translation, we use a 48-bit device address space (`T0SZ = 16`).
pub const DEFAULT_TCR_T0SZ: u32 = 16;

/// Size in bytes of the default 48-bit TTBR0 device virtual address space (`1 << 48`).
pub const DEFAULT_TTBR0_ADDR_SPACE_SZ: u64 = 1u64 << (64 - DEFAULT_TCR_T0SZ);

/// Returns `true` if `val` is an exact multiple of [`PAGE_SIZE`].
#[inline]
pub const fn is_page_rounded(val: u64) -> bool {
    (val & PAGE_MASK) == 0
}

/// Rounds `val` up to the next multiple of [`PAGE_SIZE`] (wrapping on overflow).
#[inline]
pub const fn round_up_page_size(val: u64) -> u64 {
    val.wrapping_add(PAGE_MASK) & !PAGE_MASK
}

/// A strongly typed enum-style-bool which determines whether a request for a PMT
/// mapping needs to be contiguous in device virtual address space.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RequireContiguousMapping {
    /// The mapping does not need to be contiguous in device virtual address space.
    No = 0,
    /// The mapping must be contiguous in device virtual address space.
    Yes = 1,
}

/// Result of querying a device virtual address range from a pinned memory token.
#[cfg(not(is_kernel))]
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueryAddressResult {
    /// Base device virtual address (IOVA or physical address in bypass mode).
    pub device_vaddr: u64,
    /// Contiguous byte length starting at `device_vaddr`.
    pub size: usize,
}

zr::static_assert!(core::mem::size_of::<QueryAddressResult>() == 16);
zr::static_assert!(core::mem::align_of::<QueryAddressResult>() == 8);

/// Operating mode for an ARM SMMU instance (`kernel.arm-smmu-mode`).
#[repr(u32)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum ArmSmmuMode {
    /// SMMU hardware is disabled and bypassed globally (`CR0.CLIENTPD = 1`).
    #[default]
    Disabled = 0,
    /// SMMU hardware is enabled, but user-created BTIs operate in Stage-1 bypass mode.
    Passthru = 1,
    /// SMMU hardware is enabled and user-created BTIs enforce Stage-1 page-table translation.
    Enforced = 2,
}
