// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::relaxed_atomic::RelaxedAtomicI64;
pub use page_bindings as bindings;

/// Defines the state of a VM page (`vm_page_t`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[repr(transparent)]
pub struct VmPageState(pub bindings::vm_page_state);

impl VmPageState {
    pub const FREE: Self = Self(bindings::vm_page_state::FREE);
    pub const ALLOC: Self = Self(bindings::vm_page_state::ALLOC);
    pub const OBJECT: Self = Self(bindings::vm_page_state::OBJECT);
    pub const WIRED: Self = Self(bindings::vm_page_state::WIRED);
    pub const HEAP: Self = Self(bindings::vm_page_state::HEAP);
    pub const MMU: Self = Self(bindings::vm_page_state::MMU);
    pub const IOMMU: Self = Self(bindings::vm_page_state::IOMMU);
    pub const IPC: Self = Self(bindings::vm_page_state::IPC);
    pub const CACHE: Self = Self(bindings::vm_page_state::CACHE);
    pub const SLAB: Self = Self(bindings::vm_page_state::SLAB);
    pub const ZRAM: Self = Self(bindings::vm_page_state::ZRAM);
    pub const FREE_LOANED: Self = Self(bindings::vm_page_state::FREE_LOANED);

    pub const COUNT: usize = bindings::vm_page_state::COUNT_ as usize;

    pub const ALL: [Self; Self::COUNT] = [
        Self::FREE,
        Self::ALLOC,
        Self::OBJECT,
        Self::WIRED,
        Self::HEAP,
        Self::MMU,
        Self::IOMMU,
        Self::IPC,
        Self::CACHE,
        Self::SLAB,
        Self::ZRAM,
        Self::FREE_LOANED,
    ];

    /// Returns the index of `self` as a `usize`.
    #[inline]
    pub const fn index(self) -> usize {
        self.0 as usize
    }

    #[inline]
    pub const fn as_raw(self) -> u8 {
        self.0 as u8
    }
}

/// Counts of VM pages by state.
#[repr(C)]
#[derive(Debug, Default)]
pub struct VmPageCounts {
    /// See comment in `percpu::vm_page_counts` for why we used a `RelaxedAtomic`.
    pub by_state: [RelaxedAtomicI64; VmPageState::COUNT],
}

/// Returns a string description of `state`.
#[inline]
pub const fn page_state_to_string(state: VmPageState) -> &'static str {
    match state.0 {
        bindings::vm_page_state::FREE => "free",
        bindings::vm_page_state::ALLOC => "alloc",
        bindings::vm_page_state::OBJECT => "object",
        bindings::vm_page_state::WIRED => "wired",
        bindings::vm_page_state::HEAP => "heap",
        bindings::vm_page_state::MMU => "mmu",
        bindings::vm_page_state::IOMMU => "iommu",
        bindings::vm_page_state::IPC => "ipc",
        bindings::vm_page_state::CACHE => "cache",
        bindings::vm_page_state::SLAB => "slab",
        bindings::vm_page_state::ZRAM => "zram",
        bindings::vm_page_state::FREE_LOANED => "free_loaned",
        _ => "unknown",
    }
}

impl core::fmt::Display for VmPageState {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(page_state_to_string(*self))
    }
}

zr::static_assert!(core::mem::size_of::<VmPageState>() == 1);
zr::static_assert!(core::mem::align_of::<VmPageState>() == 1);
zr::static_assert!(core::mem::size_of::<VmPageCounts>() == 12 * 8);
zr::static_assert!(core::mem::align_of::<VmPageCounts>() == 8);
zr::static_assert!(core::mem::offset_of!(VmPageCounts, by_state) == 0);
zr::static_assert!({
    let mut i = 0;
    while i < VmPageState::COUNT {
        assert!(VmPageState::ALL[i].index() == i);
        i += 1;
    }
    true
});
