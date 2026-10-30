// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMUv2 Pinned Memory Token (`SmmuPmt`) implementation.

use core::pin::Pin;

use fbl::{
    DoublyLinkedListContainable, DoublyLinkedListNode, Recyclable, RefPtr, pin_make_ref_counted,
};
use ksync::{KCell, KMutex, LockToken, PhantomMutex};
use pin_init::{pin_data, pinned_drop};
use zx_status::Status;

use crate::arm_smmu::common::{
    Allocation, INVALID_PADDR, PAGE_SHIFT, PAGE_SIZE, PinnedVmoSource, QueryAddressResult,
    SmmuPinnedVmo, is_page_rounded, round_up_page_size,
};
use crate::arm_smmu::v2::BtiMode;
use crate::arm_smmu::v2::smmu_bti::{SmmuBti, SmmuBtiPmtLockClass, SmmuBtiPmtLockGuard};

/// Lifecycle state of an [`SmmuPmt`] instance.
#[repr(u32)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SmmuPmtState {
    /// PMT has been created, but not successfully mapped and added to its owner BTI.
    Initial = 0,
    /// PMT has been created, mapped (or is in the process of mapping), and added to its owner BTI.
    Active = 1,
    /// PMT has been explicitly unpinned. Its pinned VMO is released and it is not on any BTI
    /// lists.
    Released = 2,
    /// Unclosed PMT whose dispatcher handle count dropped to zero without being unpinned.
    ///
    /// Since we can enforce BTI access at a hardware level, if a PMT hits zero handles without an
    /// explicit unpin, we can still immediately return the pages to the global page pool. So, the
    /// pinned VMO is released, and to keep bookkeeping consistent, we still record the PMT state
    /// as having been quarantined.
    Quarantined = 3,
}

/// An SMMUv2 Pinned Memory Token representing a pinned VMO region mapped into a BTI.
///
/// The lifecycle of an [`SmmuPmt`] can be complicated due to the requirement to support the
/// pattern where a user obtains a handle to a PMT via a `zx_bti_pin` syscall, and then destroys
/// the last handle to the PMT without first deliberately unpinning the memory in the PMT via a
/// call to `zx_pmt_unpin`.
///
/// Refer to the comments in [`SmmuBti`] (`v2/smmu_bti.rs`) for a detailed lifecycle state diagram.
///
/// ## External Lock Projection (`SmmuBti::pmt_lock`)
///
/// Mutable state (`pinned_vmo`, `state`, `map_location`) is protected by the owning `SmmuBti`'s
/// `pmt_lock` (`SmmuBtiPmtLockClass`). Because `#[ksync::guarded]` requires a `#[mutex(...)]`
/// field on the struct to generate token-checked field projections (`guard_pmt_lock` /
/// `guard_pmt_lock_mut`), `pmt_lock` is a zero-sized `KMutex<PhantomMutex>` tagged with the shared
/// `SmmuBtiPmtLockClass`. Callers lock `owner.lock_pmt_lock()` and pass its `LockToken` into
/// `pmt.guard_pmt_lock(token)` / `pmt.guard_pmt_lock_mut(token)`. The token proves that a
/// `pmt_lock` is held, but not which BTI's: a zero-sized lock cannot be checked against its
/// guard, so these calls are `unsafe`, and each caller states why the token is of the PMT's own
/// owner.
#[ksync::guarded]
#[fbl::ref_counted]
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct SmmuPmt {
    #[pin]
    pub(crate) list_node: DoublyLinkedListNode<SmmuPmt>,
    owner: RefPtr<SmmuBti>,
    bti_mode: BtiMode,
    size: u64,
    #[mutex(SmmuBtiPmtLockClass)]
    pub(crate) pmt_lock: KMutex<PhantomMutex>,
    #[guarded_by(pmt_lock)]
    pub(crate) pinned_vmo: SmmuPinnedVmo,
    #[guarded_by(pmt_lock)]
    pub(crate) state: SmmuPmtState,
    /// Optional device address space allocation. While `SmmuPmt` instances always have this field,
    /// it is only `Some` when a translation-mode mapping is active (or about to be released on
    /// an aborted `SmmuBti::map`).
    #[guarded_by(pmt_lock)]
    pub(crate) map_location: Option<Allocation>,
}

// Manual implementation because `#[pin_data]` copies field attributes onto its internal
// projection struct literal, where derive helper attributes like `#[dll_node]` are rejected.
impl DoublyLinkedListContainable<Self> for SmmuPmt {
    #[inline]
    fn get_node(&self) -> &DoublyLinkedListNode<Self> {
        &self.list_node
    }
}

// SAFETY: `SmmuPmt` state is synchronized via `SmmuBti::pmt_lock` and atomic `RefCounted`.
unsafe impl Send for SmmuPmt {}
// SAFETY: `SmmuPmt` state is synchronized via `SmmuBti::pmt_lock` and atomic `RefCounted`.
unsafe impl Sync for SmmuPmt {}

impl SmmuPmt {
    /// Creates a new [`SmmuPmt`] in the [`SmmuPmtState::Initial`] state while holding
    /// `owner.pmt_lock`.
    ///
    /// If allocation of the [`SmmuPmt`] fails, any `map_location` is immediately unmapped via
    /// `owner.release_mapping_locked(pmt_guard, map_location)` before returning `None`.
    ///
    /// # Safety
    ///
    /// `pmt_guard` must hold `owner.pmt_lock`.
    pub(crate) unsafe fn create(
        owner: &SmmuBti,
        pmt_guard: &mut Pin<&mut SmmuBtiPmtLockGuard<'_>>,
        pinned_vmo: SmmuPinnedVmo,
        bti_mode: BtiMode,
        map_location: Option<Allocation>,
    ) -> Option<RefPtr<Self>> {
        debug_assert!(bti_mode == BtiMode::Bypass || bti_mode == BtiMode::Translation);
        debug_assert!((bti_mode == BtiMode::Translation) == map_location.is_some());

        let size = pinned_vmo.size();

        let res = pin_make_ref_counted!(Self {
            list_node: DoublyLinkedListNode::new(),
            owner: RefPtr::from_ref(owner),
            bti_mode,
            size,
            pmt_lock: KMutex::new(PhantomMutex),
            pinned_vmo: KCell::new(SmmuPinnedVmo::empty()),
            state: KCell::new(SmmuPmtState::Initial),
            map_location: KCell::new(None),
        });

        match res {
            Ok(pmt) => {
                {
                    // SAFETY: `pmt` was just created with `owner` as its owner, and the caller
                    // promises that `pmt_guard` holds `owner.pmt_lock`.
                    let mut inner =
                        unsafe { pmt.guard_pmt_lock_mut(pmt_guard.as_mut().token_mut()) };
                    *inner.pinned_vmo_mut() = pinned_vmo;
                    *inner.map_location_mut() = map_location;
                }
                Some(pmt)
            }
            Err(_) => {
                // Failed to allocate the SmmuPmt instance. Tell our owning BTI to release any
                // mapping we may have failed to transfer to the new SmmuPmt instance.
                owner.release_mapping_locked(pmt_guard, map_location);
                None
            }
        }
    }

    /// Queries the device virtual address and contiguous size for `query_offset` and `query_size`.
    pub fn query_address(
        &self,
        query_offset: u64,
        query_size: usize,
    ) -> Result<QueryAddressResult, Status> {
        // Basic parameter validation. The offset into the VMO we are querying must be page aligned,
        // and the size may not be zero.
        if !is_page_rounded(query_offset) || query_size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // Grab our owner's PMT lock and check to be sure we are in the proper state to be querying
        // addresses. It is important that we check to make sure that we are in the `Active` state
        // as well as checking that our `pinned_vmo` state holds a valid VMO, because it is our
        // guarantee that our `map_location` is valid when we are operating in `Translation` mode.
        //
        // If (someday) a fault happens which causes the underlying VMO to become un-pinned (it
        // would need to be deferred to a thread, as we cannot unpin at interrupt time), the worst
        // case is that we manage to satisfy a request to pin/map only to have the BTI itself enter
        // the fault state and invalidate the PMT we were in the process of creating.
        ksync::lock!(let owner_guard = self.owner.lock_pmt_lock());
        // SAFETY: `owner_guard` holds `self.owner.pmt_lock`, which is the lock guarding `self`.
        let inner = unsafe { self.guard_pmt_lock(owner_guard.token()) };
        if *inner.state() != SmmuPmtState::Active || !inner.pinned_vmo().has_vmo() {
            return Err(Status::BAD_STATE);
        }

        // If our BTI is operating in translation mode when we pinned and mapped the VMO into the
        // BTI's address space, then all accesses via the device are going to seem contiguous from
        // the device's perspective. All we need to do is bounds check the request and offset our
        // request by our mapped location's base address.
        if self.bti_mode == BtiMode::Translation {
            let Some(loc) = inner.map_location().as_ref() else {
                return Err(Status::BAD_STATE);
            };
            // Make sure the query fits inside of the mapping in our device's address space.
            if query_offset >= loc.size() || (query_size as u64) > (loc.size() - query_offset) {
                return Err(Status::OUT_OF_RANGE);
            }
            return Ok(QueryAddressResult {
                device_vaddr: loc.base() + query_offset,
                size: query_size,
            });
        }

        // Looks like our BTI must have been operating in bypass mode when we pinned this memory.
        // We don't have a device virtual address mapping for the VMO, so just bounce the request
        // through to the `SmmuPinnedVmo` instance.
        //
        // The `SmmuPinnedVmo` demands that we ask it only about regions which have been properly
        // bound to the original pin location, and which are page aligned in both offset and size.
        // Since `query_offset` is relative to our pin aperture, we need to offset it by the
        // `pinned_vmo`'s offset into the underlying VMO. We have already checked to be sure that
        // `query_offset` is page aligned, and we know that `pinned_vmo.offset()` must be as well,
        // so the sum must be too.
        //
        // Make sure that we have rounded `query_size` up to the nearest page boundary as well so
        // that we pass all of the checks of `lookup_contiguous`. Be careful when checking the
        // return code from `lookup_contiguous`. The user might either be asking for verification
        // that the entire region is contiguous (following the pattern in the `Bti::pin` syscall
        // handling for `ZX_BTI_CONTIGUOUS` requests), or they might be iterating a scatter-gather
        // list a page at a time, and they are simply passing the remaining length of their query
        // (`PinnedMemoryTokenDispatcher::EncodeAddrs`).
        //
        // Unfortunately, `SmmuPinnedVmo::lookup_contiguous` is only going to return to us a single
        // page address if the full region from `[offset, offset + size)` is contiguous. Otherwise
        // it will return `Status::NOT_FOUND` when it hits the first discontinuity. In that case,
        // retry the operation for a single page so that we return the physical address of the
        // first page of the range to satisfy the user's request.
        debug_assert_eq!(self.bti_mode, BtiMode::Bypass);
        let query_offset = query_offset.wrapping_add(inner.pinned_vmo().offset());
        let mut query_size_u64 = round_up_page_size(query_size as u64);

        for attempt in 0..2 {
            match inner.pinned_vmo().lookup_contiguous(query_offset, query_size_u64) {
                Ok(paddr) => {
                    debug_assert_ne!(paddr, INVALID_PADDR);
                    return Ok(QueryAddressResult {
                        device_vaddr: paddr,
                        size: query_size_u64 as usize,
                    });
                }
                Err(status) => {
                    // Tolerate errors other than INVALID_ARGS or OUT_OF_RANGE only if this is the
                    // second (and final) attempt.
                    if status == Status::INVALID_ARGS
                        || status == Status::OUT_OF_RANGE
                        || attempt != 0
                    {
                        return Err(status);
                    }
                }
            }

            // We shouldn't be here after our first attempt. The second attempt must always either
            // succeed or fail. If this is our first attempt, reduce our query size to just one
            // page.
            debug_assert_eq!(attempt, 0);
            query_size_u64 = PAGE_SIZE;
        }

        // We should never get here. If we succeeded on our first or second attempt, we would have
        // already returned, and if we received _any_ errors on our second attempt, we should have
        // already bailed out.
        unreachable!("Second attempt in query_address always returns");
    }

    /// Explicitly unpins and releases the memory held by this PMT.
    pub fn release_pinned_memory(&self) {
        // SAFETY: `self.owner` owns `self`.
        unsafe { self.owner.on_pmt_unpin(self) };
    }

    /// Invoked when the last user handle to the `PinnedMemoryTokenDispatcher` is closed.
    pub fn on_dispatcher_zero_handles(&self) {
        // SAFETY: `self.owner` owns `self`.
        unsafe { self.owner.on_pmt_zero_handles(self) };
    }

    /// Returns a reference to the owning [`SmmuBti`].
    #[inline]
    pub fn owner(&self) -> &RefPtr<SmmuBti> {
        &self.owner
    }

    /// Returns `true` if this PMT is currently linked in `owner.active_pmt_list`.
    #[inline]
    pub fn in_container(&self) -> bool {
        self.list_node.in_container()
    }

    /// Returns the size in bytes of the pinned VMO region.
    #[inline]
    pub fn size(&self) -> u64 {
        self.size
    }

    /// Returns the number of pinned pages while holding `owner.pmt_lock`.
    #[inline]
    pub(crate) fn pages(&self, _token: &LockToken<'_, SmmuBtiPmtLockClass>) -> u64 {
        self.size >> PAGE_SHIFT
    }

    /// Returns the current [`SmmuPmtState`] while holding `owner.pmt_lock`.
    ///
    /// # Safety
    ///
    /// `token` must be of the owning BTI's `pmt_lock`.
    #[inline]
    pub(crate) unsafe fn state(&self, token: &LockToken<'_, SmmuBtiPmtLockClass>) -> SmmuPmtState {
        // SAFETY: The caller promises that `token` is of `self.owner.pmt_lock`.
        *unsafe { self.guard_pmt_lock(token) }.state()
    }
}

#[pinned_drop]
impl PinnedDrop for SmmuPmt {
    fn drop(self: Pin<&mut Self>) {
        // By the time that our PMT destructs, it must not be in any container (e.g. our owner's
        // `active_pmt_list`), and its state must be one of `Initial`, `Released`, or `Quarantined`
        // (we should never destruct while `Active`).
        //
        // The only legitimate reason for us to still be holding a pinned VMO is because we took
        // ownership of it while in the `Initial` state during `SmmuBti::map`, but our BTI entered
        // the fault state before the PMT could be added to `active_pmt_list`. It is okay if this
        // happens: we have never been exposed to any users, and the pinned memory will be returned
        // as our `SmmuPinnedVmo` drops.
        //
        // Otherwise, we made it into the `active_pmt_list`, and the only way out of the list
        // (aside from shutting down the entire SMMU in tests) is via `on_pmt_unpin` or
        // `on_pmt_zero_handles`, which will put us into the `Released` or `Quarantined` state
        // (respectively) and take `pinned_vmo` and `map_location`.
        //
        // No locks are taken here. `drop` may run wherever the last reference happens to be
        // released (including with our owner's `pmt_lock` held), and it has exclusive access to
        // `self`, so the guarded fields are read directly.
        //
        // SAFETY: We are in `PinnedDrop::drop`, so nothing else can reference `self`. None of the
        // fields are moved.
        let this = unsafe { self.get_unchecked_mut() };
        debug_assert!(!this.list_node.in_container());
        let state = *this.state.get_inner_mut();
        debug_assert!(
            state == SmmuPmtState::Initial
                || state == SmmuPmtState::Released
                || state == SmmuPmtState::Quarantined
        );

        // Temporary porting note: the C++ `~SmmuPmt` asserted `vmo == nullptr || state !=
        // kInitial`, the opposite of what its own comment (and this one) describes, and the
        // `iommu::Pmt` base destructor asserted that the VMO was gone. Both fired in debug builds
        // when a BTI faulted while a pin was in progress.
        debug_assert!(!this.pinned_vmo.get_inner_mut().has_vmo() || state == SmmuPmtState::Initial);

        // Temporary porting note: the C++ `~SmmuPmt` released a mapping which was still attached,
        // relying on its only caller (the abort path of `SmmuBti::Map`) to be holding `pmt_lock_`.
        // Here that path releases the mapping itself before dropping the PMT, so a mapping must
        // never reach this point. Returning it would need `pmt_lock`, and letting it drop would
        // free the address range while leaving the device able to reach memory which is about to
        // be unpinned, so treat it as fatal.
        assert!(
            this.map_location.get_inner_mut().is_none(),
            "SmmuPmt dropped with a mapping still attached (state {state:?})"
        );
    }
}
