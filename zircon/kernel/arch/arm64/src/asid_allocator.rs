// Copyright 2020 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Address Space ID (ASID) allocator for ARM64 user and guest address spaces.

use super::aspace_bindings::{
    MMU_ARM64_FIRST_USER_ASID, MMU_ARM64_MAX_USER_ASID_8, MMU_ARM64_MAX_USER_ASID_16,
};
use arm64_feature_bindings::arm64_asid_width;
use core::pin::Pin;
use debug::ltracef;
use id_allocator::IdAllocator;
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data};
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

/// Total number of IDs in the 16-bit ASID space (`[0, 65536)`).
const MAX_ASIDS: usize = (MMU_ARM64_MAX_USER_ASID_16 as usize) + 1;
const BITMAP_WORDS: usize = MAX_ASIDS / (usize::BITS as usize);

type BackingAsidAllocator =
    IdAllocator<u16, MAX_ASIDS, { MMU_ARM64_FIRST_USER_ASID as usize }, BITMAP_WORDS>;

/// Class to automate allocating ASIDs from the range of usable ASIDs outside the kernel.
///
/// NOTE: stores as much space for 16-bit ASIDs, but will fall back to limiting to 8-bit ASIDs
/// given hardware support.
#[pin_data]
pub(super) struct AsidAllocator {
    #[pin]
    allocator: BackingAsidAllocator,
    #[cfg(ktest)]
    max_user_asid: u16,
}

impl AsidAllocator {
    /// Initializes an [`AsidAllocator`] in place without constructing the 8 KiB backing bitmap on
    /// the caller's stack.
    pub(super) fn new(asid_width_override: arm64_asid_width) -> impl PinInit<Self, Status> {
        // Check whether or not the CPU only supports 8 bits, which is fairly exceptional; most
        // support the full 16-bit ASID space.
        let asid_width = if asid_width_override == arm64_asid_width::UNKNOWN {
            // SAFETY: Only the boot CPU's `arm64_feature_init` writes `arm64_mmu_features`, before
            // any other CPU starts and before `arm64_mmu_early_init` runs, so no write races with
            // this read.
            unsafe { super::feature::arm64_mmu_features.asid_width }
        } else {
            asid_width_override
        };
        debug_assert_ne!(asid_width, arm64_asid_width::UNKNOWN);

        #[cfg(ktest)]
        let max_user_asid = if asid_width == arm64_asid_width::ASID_8 {
            MMU_ARM64_MAX_USER_ASID_8
        } else {
            MMU_ARM64_MAX_USER_ASID_16
        };

        pin_init::pin_init!(Self {
            allocator <- BackingAsidAllocator::init(),
            #[cfg(ktest)]
            max_user_asid,
        } ? Status)
        .pin_chain(move |this| {
            if asid_width == arm64_asid_width::ASID_8 {
                this.allocator.reset(MMU_ARM64_MAX_USER_ASID_8 + 1)?;
            }
            Ok(())
        })
    }

    pub(super) fn alloc(&self) -> Result<u16, Status> {
        // Map `IdAllocator`'s `NO_RESOURCES` exhaustion status to `NO_MEMORY`.
        let new_asid = self.allocator.try_alloc().map_err(|_| Status::NO_MEMORY)?;
        ltracef!("new asid {new_asid:#x}\n");
        Ok(new_asid)
    }

    pub(super) fn free(&self, asid: u16) -> Result<(), Status> {
        ltracef!("free asid {asid:#x}\n");
        self.allocator.free(asid)
    }

    #[cfg(ktest)]
    fn max_user_asid(&self) -> u16 {
        self.max_user_asid
    }
}

/// Constructed once by [`asid_allocator_init`] during early MMU init; the allocator's internal
/// `KMutex` serializes every subsequent operation.
static GLOBAL_ASID_ALLOCATOR: LazyInit<AsidAllocator> = LazyInit::uninit();

/// Initializes the global ASID allocator.
///
/// # Safety
///
/// Must be called at most once on the boot CPU during early MMU init (`arm64_mmu_early_init`),
/// after `arm64_feature_init` has populated `arm64_mmu_features` and before any concurrent calls to
/// [`asid_alloc`] or [`asid_free`].
pub(super) unsafe fn asid_allocator_init() {
    // SAFETY: Caller guarantees single-threaded boot-time initialization before any reader or
    // concurrent writer can touch `GLOBAL_ASID_ALLOCATOR`.
    unsafe {
        Pin::static_ref(&GLOBAL_ASID_ALLOCATOR)
            .init_pin(AsidAllocator::new(arm64_asid_width::UNKNOWN))
            .expect("ASID allocator init");
    }
}

/// Allocates a unique user/guest ASID.
pub(super) fn asid_alloc() -> Result<u16, Status> {
    GLOBAL_ASID_ALLOCATOR.alloc()
}

/// Frees a previously allocated user/guest ASID.
pub(super) fn asid_free(asid: u16) -> Result<(), Status> {
    GLOBAL_ASID_ALLOCATOR.free(asid)
}

#[cfg(ktest)]
/// Tests for asid allocator
#[unittest::suite(name = "asid")]
mod tests {
    use unittest::{assert_eq, assert_ge, assert_le, assert_ok, assert_true, subtest, unwrap_ok};

    fn asid_allocator_test_inner(asid_width: arm64_asid_width) -> bool {
        let case = subtest!(|width: arm64_asid_width| {
            let aa = unwrap_ok!(kalloc::Box::try_pin_init(AsidAllocator::new(width)));

            // Test that it computed the correct ASID width.
            let max_asid = if width == arm64_asid_width::ASID_8 {
                MMU_ARM64_MAX_USER_ASID_8
            } else {
                MMU_ARM64_MAX_USER_ASID_16
            };
            assert_eq!(aa.max_user_asid(), max_asid);

            // Run the test twice to make sure it clears back to a default state.
            for _ in 0..2 {
                // Use up all the ASIDs.
                for _ in MMU_ARM64_FIRST_USER_ASID..=max_asid {
                    let asid = unwrap_ok!(aa.alloc());
                    assert_ge!(asid, MMU_ARM64_FIRST_USER_ASID);
                    assert_le!(asid, max_asid);
                }

                // Expect the next one to fail.
                assert_true!(aa.alloc() == Err(Status::NO_MEMORY));

                // Free them all.
                for i in MMU_ARM64_FIRST_USER_ASID..=max_asid {
                    assert_ok!(aa.free(i));
                }
            }
        });
        case(asid_width)
    }

    #[doc = "8 bit asid allocator test"]
    #[test]
    fn asid_allocator_test_8bit() {
        assert_true!(asid_allocator_test_inner(arm64_asid_width::ASID_8));
    }

    #[doc = "16 bit asid allocator test"]
    #[test]
    fn asid_allocator_test_16bit() {
        assert_true!(asid_allocator_test_inner(arm64_asid_width::ASID_16));
    }
}
