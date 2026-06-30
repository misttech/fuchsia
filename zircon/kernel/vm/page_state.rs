// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use kernel::relaxed_atomic::RelaxedAtomicI64;

/// Defines the state of a VM page (`vm_page_t`).
///
/// Be sure to keep this enum in sync with the definition of `vm_page_t`.
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum VmPageState {
    Free = 0,
    Alloc,
    Object,
    Wired,
    Heap,
    /// Allocated to serve arch-specific mmu purposes.
    Mmu,
    /// Allocated for platform-specific iommu structures.
    Iommu,
    Ipc,
    Cache,
    Slab,
    Zram,
    FreeLoaned,

    Count,
}

impl VmPageState {
    /// Returns the index of `self` as a `usize`.
    #[inline]
    pub const fn index(self) -> usize {
        self as usize
    }
}

/// Counts of VM pages by state.
#[repr(C)]
#[derive(Debug, Default)]
pub struct VmPageCounts {
    /// See comment in `percpu::vm_page_counts` for why we used a `RelaxedAtomic`.
    pub by_state: [RelaxedAtomicI64; (VmPageState::Count).index()],
}

/// Returns a string description of `state`.
#[inline]
pub const fn page_state_to_string(state: VmPageState) -> &'static str {
    match state {
        VmPageState::Free => "free",
        VmPageState::Alloc => "alloc",
        VmPageState::Object => "object",
        VmPageState::Wired => "wired",
        VmPageState::Heap => "heap",
        VmPageState::Mmu => "mmu",
        VmPageState::Ipc => "ipc",
        VmPageState::Cache => "cache",
        VmPageState::Slab => "slab",
        VmPageState::Zram => "zram",
        _ => "unknown",
    }
}

impl core::fmt::Display for VmPageState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(page_state_to_string(*self))
    }
}

const _: () = assert!(core::mem::size_of::<VmPageState>() == 1);
const _: () = assert!(core::mem::align_of::<VmPageState>() == 1);
const _: () = assert!(core::mem::size_of::<VmPageCounts>() == 12 * 8);
const _: () = assert!(core::mem::align_of::<VmPageCounts>() == 8);
const _: () = assert!(core::mem::offset_of!(VmPageCounts, by_state) == 0);
