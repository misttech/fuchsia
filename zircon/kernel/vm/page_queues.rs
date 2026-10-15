// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::kernel::event::{AutounsignalEvent, Event};
use crate::kernel::relaxed_atomic::{RelaxedAtomicBool, RelaxedAtomicU32, RelaxedAtomicU64};
use crate::kernel::thread::{Thread, ThreadPtr};
use crate::platform_rs::timer::DurationMono;
use crate::vm::compressor::VmCompressor;
use crate::vm::page::{VmPage, VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::physical_page_borrowing_config::PhysicalPageBorrowingConfig;
use crate::vm::pmm;
use crate::vm::vm::vm_ktrace_duration;
use crate::vm::vm_cow_pages::{EvictionAction, VmCowPages, VmCowReclaimType};
use core::cell::UnsafeCell;
use core::ffi::{CStr, c_void};
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{MaybeUninit, offset_of};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering};
use debug::kernel_oops;
use fbl::{DoublyLinkedListContainable, RefPtr};
use kprint::kprintln;
use ksync::{KMutex, LockToken, RawCriticalMutex, guarded};
use page_bindings::{vm_page_state, vm_page_t};
use page_queues_bindings as bindings;
use pin_init::{PinInit, pin_data};

define_kcounter!(PQ_LRU_PAGES_EVICTED, "pq.lru.pages_evicted", Sum);
define_kcounter!(PQ_LRU_PAGES_COMPRESSED, "pq.lru.pages_compressed", Sum);
define_kcounter!(PQ_LRU_PAGES_DISCARDED, "pq.lru.pages_discarded", Sum);
define_kcounter!(PQ_LRU_SWEEPING_SKIPPED_UNLOAN, "pq.lru.sweeping_skipped_unloan", Sum);
define_kcounter!(PQ_ACCESSED_NORMAL, "pq.accessed.normal", Sum);
define_kcounter!(PQ_ACCESSED_NORMAL_SAME_QUEUE, "pq.accessed.normal_same_queue", Sum);
define_kcounter!(PQ_ACCESSED_ISOLATE, "pq.accessed.isolate", Sum);
define_kcounter!(PQ_ACCESSED_NOT_RECLAIM, "pq.accessed.not_reclaim", Sum);

/// Used to identify the reason that aging is triggered, mostly for debugging and informational
/// purposes.
#[repr(i32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum AgeReason {
    /// Aging occurred due to the maximum timeout being reached before any other reason could
    /// trigger.
    Timeout,
    /// The allowable ratio of active versus inactive pages was exceeded.
    ActiveRatio,
    /// An explicit call to RotatePagerBackedQueues caused aging. This would typically occur due to
    /// test code or via the kernel debug console.
    Manual,
}
zr::static_assert!(AgeReason::Timeout as i32 == bindings::PageQueues_AgeReason::Timeout as i32);
zr::static_assert!(
    AgeReason::ActiveRatio as i32 == bindings::PageQueues_AgeReason::ActiveRatio as i32
);
zr::static_assert!(AgeReason::Manual as i32 == bindings::PageQueues_AgeReason::Manual as i32);
zr::static_assert!(size_of::<AgeReason>() == size_of::<bindings::PageQueues_AgeReason>());

/// Describes any action to take when processing the LRU queue. This is applied to pages that would
/// otherwise have to be moved from the old LRU queue into the isolate queue.
#[repr(i32)]
#[derive(Debug, Copy, Clone, Hash, PartialEq, Eq)]
pub enum LruAction {
    None,
    EvictOnly,
    CompressOnly,
    EvictAndCompress,
}
zr::static_assert!(LruAction::None as i32 == bindings::PageQueues_LruAction::None as i32);
zr::static_assert!(LruAction::EvictOnly as i32 == bindings::PageQueues_LruAction::EvictOnly as i32);
zr::static_assert!(
    LruAction::CompressOnly as i32 == bindings::PageQueues_LruAction::CompressOnly as i32
);
zr::static_assert!(
    LruAction::EvictAndCompress as i32 == bindings::PageQueues_LruAction::EvictAndCompress as i32
);
zr::static_assert!(size_of::<LruAction>() == size_of::<bindings::PageQueues_LruAction>());

impl LruAction {
    fn from_raw(raw: i32) -> Self {
        match raw {
            x if x == Self::None as i32 => Self::None,
            x if x == Self::EvictOnly as i32 => Self::EvictOnly,
            x if x == Self::CompressOnly as i32 => Self::CompressOnly,
            x if x == Self::EvictAndCompress as i32 => Self::EvictAndCompress,
            _ => panic!("Invalid LruAction: {raw}"),
        }
    }
}

/// Helper struct to group queue length counts returned by [`PageQueues::queue_counts`].
#[repr(C)]
#[derive(Default, Debug, Clone, PartialEq, Eq)]
pub struct Counts {
    pub reclaim: [usize; NUM_RECLAIM],
    pub reclaim_isolate: usize,
    pub pager_backed_dirty: usize,
    pub anonymous: usize,
    pub wired: usize,
    pub anonymous_zero_fork: usize,
    pub failed_reclaim: usize,
    pub high_priority: usize,
}
zr::static_assert!(offset_of!(Counts, reclaim) == offset_of!(bindings::PageQueues_Counts, reclaim));
zr::static_assert!(
    offset_of!(Counts, reclaim_isolate) == offset_of!(bindings::PageQueues_Counts, reclaim_isolate)
);
zr::static_assert!(
    offset_of!(Counts, pager_backed_dirty)
        == offset_of!(bindings::PageQueues_Counts, pager_backed_dirty)
);
zr::static_assert!(
    offset_of!(Counts, anonymous) == offset_of!(bindings::PageQueues_Counts, anonymous)
);
zr::static_assert!(offset_of!(Counts, wired) == offset_of!(bindings::PageQueues_Counts, wired));
zr::static_assert!(
    offset_of!(Counts, anonymous_zero_fork)
        == offset_of!(bindings::PageQueues_Counts, anonymous_zero_fork)
);
zr::static_assert!(
    offset_of!(Counts, failed_reclaim) == offset_of!(bindings::PageQueues_Counts, failed_reclaim)
);
zr::static_assert!(
    offset_of!(Counts, high_priority) == offset_of!(bindings::PageQueues_Counts, high_priority)
);
zr::static_assert!(size_of::<Counts>() == size_of::<bindings::PageQueues_Counts>());

/// Helper struct to group reclaimable queue length counts returned by
/// [`PageQueues::get_reclaim_queue_counts`].
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct ReclaimCounts {
    pub total: usize,
    pub newest: usize,
    pub oldest: usize,
}
zr::static_assert!(
    offset_of!(ReclaimCounts, total) == offset_of!(bindings::PageQueues_ReclaimCounts, total)
);
zr::static_assert!(
    offset_of!(ReclaimCounts, newest) == offset_of!(bindings::PageQueues_ReclaimCounts, newest)
);
zr::static_assert!(
    offset_of!(ReclaimCounts, oldest) == offset_of!(bindings::PageQueues_ReclaimCounts, oldest)
);
zr::static_assert!(size_of::<ReclaimCounts>() == size_of::<bindings::PageQueues_ReclaimCounts>());

/// Helper struct to group active and inactive page counts returned by
/// [`PageQueues::get_active_inactive_counts`].
#[repr(C)]
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq)]
pub struct ActiveInactiveCounts {
    /// Pages that would normally be available for eviction, but are presently considered active
    /// and so will not be evicted.
    pub active: usize,
    /// Pages that are available for eviction due to not presently being considered active.
    pub inactive: usize,
}
zr::static_assert!(
    offset_of!(ActiveInactiveCounts, active)
        == offset_of!(bindings::PageQueues_ActiveInactiveCounts, active)
);
zr::static_assert!(
    offset_of!(ActiveInactiveCounts, inactive)
        == offset_of!(bindings::PageQueues_ActiveInactiveCounts, inactive)
);
zr::static_assert!(
    size_of::<ActiveInactiveCounts>() == size_of::<bindings::PageQueues_ActiveInactiveCounts>()
);

/// Used to represent and return page backlink information acquired whilst holding the page queue
/// lock. As a VMO may not destruct while it has pages in it, the cow RefPtr will always be valid,
/// although the page and offset contained here are not synchronized and must be separately
/// validated before use. This can be done by acquiring the returned vmo's lock and then validating
/// that the page is still contained at the offset.
#[derive(Clone)]
pub struct VmoBacklink {
    pub cow: Option<RefPtr<VmCowPages>>,
    pub page: VmPagePtr,
    pub offset: u64,
}

impl core::fmt::Debug for VmoBacklink {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("VmoBacklink")
            .field("cow", &self.cow.as_ref().map(|c| c.as_raw()))
            .field("page", &self.page)
            .field("offset", &self.offset)
            .finish()
    }
}

/// Specifies the indices for both the page_queues and the page_queue_counts
#[repr(transparent)]
#[derive(Default, Debug, Clone, Copy, Eq, PartialEq, Ord, PartialOrd)]
struct PageQueue(u8);

impl PageQueue {
    const NONE: Self = Self(0);
    const ANONYMOUS: Self = Self(1);
    const WIRED: Self = Self(2);
    const HIGH_PRIORITY: Self = Self(3);
    const ANONYMOUS_ZERO_FORK: Self = Self(4);
    const PAGER_BACKED_DIRTY: Self = Self(5);
    const FAILED_RECLAIM: Self = Self(6);
    const RECLAIM_ISOLATE: Self = Self(7);
    const RECLAIM_BASE: Self = Self(8);
    const RECLAIM_LAST: Self = Self(Self::RECLAIM_BASE.0 + NUM_RECLAIM as u8 - 1);
    const NUM_QUEUES: Self = Self(Self::RECLAIM_LAST.0 + 1);
}

// Ensure that the reclaim queue counts are always at the end.
zr::static_assert!(PageQueue::RECLAIM_LAST.0 + 1 == PageQueue::NUM_QUEUES.0);

/// The number of reclamation queues is slightly arbitrary, but to be useful you want at least 3
/// representing
///  * Very new pages that you probably don't want to evict as doing so probably implies you are in
///    swap death
///  * Slightly old pages that could be evicted if needed
///  * Very old pages that you'd be happy to evict
///
/// With two active queues 8 page queues are used so that there is some fidelity of information
/// in the inactive queues. Additional queues have reduced value as sufficiently old pages
/// quickly become equivalently unlikely to be used in the future.
pub const NUM_RECLAIM: usize = 8;
zr::static_assert!(NUM_RECLAIM == bindings::PageQueues_kNumReclaim);

/// Two active queues are used to allow for better fidelity of active information. This prevents
/// a race between aging once and needing to collect/harvest age information.
pub const NUM_ACTIVE_QUEUES: usize = 2;
zr::static_assert!(NUM_ACTIVE_QUEUES == bindings::PageQueues_kNumActiveQueues);

/// The amount of pages that will have to move around the queues before the active/inactive
/// ratio is re-checked. This therefore represents how much error the active ratio aging process
/// might have, or how delayed the MRU generation might be. In the worst case once the active
/// ratio is triggered this value is how much page data needs to then change queues before the
/// aging process happens.
const MB: usize = 1024 * 1024;
pub const ACTIVE_INACTIVE_ERROR_MARGIN: usize = (2 * MB) / page::SIZE;
zr::static_assert!(ACTIVE_INACTIVE_ERROR_MARGIN == bindings::PageQueues_kActiveInactiveErrorMargin);

// Needs to be at least one non-active queue
zr::static_assert!(NUM_RECLAIM > NUM_ACTIVE_QUEUES);

/// In addition to active and inactive, we want to consider some of the queues as 'oldest' to
/// provide an additional way to limit eviction. Presently the processing of the LRU queue to
/// make room for aging is not integrated with the Evictor, and so will not trigger eviction,
/// therefore to have a non-zero number of pages ever appear in an oldest queue for eviction the
/// last two queues are considered the oldest.
pub const NUM_OLDEST_QUEUES: usize = 2;
zr::static_assert!(NUM_OLDEST_QUEUES == bindings::PageQueues_kNumOldestQueues);
zr::static_assert!(NUM_OLDEST_QUEUES + NUM_ACTIVE_QUEUES <= NUM_RECLAIM);

/// Number of different isolate queues that are available. Different isolate queues allow for
/// separating isolate pages into different buckets such that more nuanced choices on what page
/// to reclaim can be made.
///
/// We use 2 queues to separate "Don't Need" pages (high reclamation priority, index 0) from
/// standard aged pages (standard reclamation priority, index 1).
pub const ISOLATE_QUEUE_DONT_NEED: usize = 0;
zr::static_assert!(ISOLATE_QUEUE_DONT_NEED == bindings::PageQueues_kIsolateQueueDontNeed);
pub const ISOLATE_QUEUE_STANDARD: usize = 1;
zr::static_assert!(ISOLATE_QUEUE_STANDARD == bindings::PageQueues_kIsolateQueueStandard);
pub const NUM_ISOLATE_QUEUES: usize = 2;
zr::static_assert!(NUM_ISOLATE_QUEUES == bindings::PageQueues_kNumIsolateQueues);

zr::static_assert!(ISOLATE_QUEUE_DONT_NEED < NUM_ISOLATE_QUEUES);
zr::static_assert!(ISOLATE_QUEUE_STANDARD < NUM_ISOLATE_QUEUES);
zr::static_assert!(ISOLATE_QUEUE_DONT_NEED < ISOLATE_QUEUE_STANDARD);

pub const DEFAULT_MIN_MRU_ROTATE_TIME: DurationMono = DurationMono::from_seconds(5);
zr::static_assert!(
    DEFAULT_MIN_MRU_ROTATE_TIME.into_nanos() == bindings::PageQueues_kDefaultMinMruRotateTime
);
pub const DEFAULT_MAX_MRU_ROTATE_TIME: DurationMono = DurationMono::from_seconds(5);
zr::static_assert!(
    DEFAULT_MAX_MRU_ROTATE_TIME.into_nanos() == bindings::PageQueues_kDefaultMaxMruRotateTime
);

/// This is presently an arbitrary constant, since the min and max mru rotate time are currently
/// fixed at the same value, meaning that the active ratio can not presently trigger, or
/// prevent, aging.
pub const DEFAULT_ACTIVE_RATIO_MULTIPLIER: u64 = 0;
zr::static_assert!(
    DEFAULT_ACTIVE_RATIO_MULTIPLIER == bindings::PageQueues_kDefaultActiveRatioMultiplier
);

/// When holding the PageQueue lock, and performing an operation on an arbitrary number of pages,
/// the "operation batch size" controls the number of pages for which the lock will be held before
/// checking for contention and potentially releasing the lock if contended, allowing other
/// operations to proceed.
const OP_BATCH_SIZE: usize = 64;

/// Removes `page` from its intrusive doubly-linked list node in O(1) time without knowing which
/// list it belongs to, matching C++ `internal_erase`.
///
/// # Safety
///
/// Caller must guarantee that the `list_lock` guarding the containing list is held and `page` is
/// currently in an intrusive container.
#[inline]
unsafe fn remove_page_from_list_node(page: &VmPage) {
    // SAFETY: Caller guarantees `list_lock` is held and `page` is in an untracked
    // `VmPageDoublyLinkedList`.
    unsafe {
        let _ = fbl::remove_from_container::<VmPage, fbl::DefaultObjectTag, NonNull<VmPage>>(page);
    }
}

// The None is needed since to know if a page can be reclaimed by the current LruAction a RefPtr
// to the VMO must first be created. If the page shouldn't be reclaimed the RefPtr must not be
// dropped till outside the lock, in case it's the last ref. The None action provides a way to
// retain these RefPtrs and have them dropped outside the lock.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum ListAction {
    None,
    ReplaceWithLoaned,
    Reclaim,
}

const MAX_DEFERRED_COUNT: usize = 16;

// Used to represent page backlink information acquired whilst holding the page queue lock, paired
// with the deferred `ListAction` to perform once the lock is dropped. As a VMO may not destruct
// while it has pages in it, the cow RefPtr will always be valid, although the page and offset
// contained here are not synchronized and must be separately validated before use. This can be done
// by acquiring the returned vmo's lock and then validating that the page is still contained at the
// offset.
struct LruIsolateItem {
    cow: RefPtr<VmCowPages>,
    page: VmPagePtr,
    offset: u64,
    action: ListAction,
}

// Helper class for building an isolate list for deferred processing when acting on the LRU queues.
// Pages are added while the page queues lock is held, and processed once the lock is dropped.
// Statically sized with the maximum number of items it might need to hold and it is an error to
// attempt to add more than this many items, as flush() cannot automatically be called due to
// incompatible locking requirements between flushing and adding items.
struct LruIsolate {
    // Cache of the PageQueues LruAction for checking what to do with different reclaimable pages.
    lru_action: LruAction,
    // List of pages and the actions to perform on them.
    // Invariant: elements in `0..self.count` are initialized `LruIsolateItem`s, and elements in
    // `self.count..MAX_DEFERRED_COUNT` are uninitialized.
    items: [MaybeUninit<LruIsolateItem>; MAX_DEFERRED_COUNT],
    // Number of initialized items in `items`.
    count: usize,
}

impl LruIsolate {
    const fn new() -> Self {
        Self {
            lru_action: LruAction::None,
            items: [const { MaybeUninit::uninit() }; MAX_DEFERRED_COUNT],
            count: 0,
        }
    }

    // Sets the LRU action, this allows the object construction to happen without the page queues
    // lock, where as setting the LruAction can be done within it.
    fn set_lru_action(&mut self, lru_action: LruAction) {
        self.lru_action = lru_action;
    }

    // Checks if there is space for additional pages to be added. If `true` the `add_*` methods
    // must not be called.
    #[inline]
    fn is_full(&self) -> bool {
        self.count >= MAX_DEFERRED_COUNT
    }

    /// # Safety
    ///
    /// Caller must ensure `!self.is_full()`, `page` is owned by `cow` (in the `OBJECT` state) and
    /// currently assigned to `PageQueues`, and `_token` proves the `list_lock` is held.
    #[inline]
    unsafe fn add_internal(
        &mut self,
        cow: Option<RefPtr<VmCowPages>>,
        page: VmPagePtr,
        action: ListAction,
        _token: &LockToken<'_, PageQueuesListLockClass>,
    ) {
        debug_assert!(cow.is_some());
        debug_assert!(!self.is_full());
        if let Some(cow) = cow {
            // SAFETY: Caller guarantees `page` is a valid `VmPagePtr` in the `OBJECT` state under
            // `list_lock`.
            let offset = unsafe { page.get_page_offset() };
            self.items[self.count].write(LruIsolateItem { cow, page, offset, action });
            self.count += 1;
        }
    }

    /// Adds a page to be potentially replaced with a loaned page.
    ///
    /// # Safety
    ///
    /// Caller must ensure `!self.is_full()`, `page` is owned by a VMO (in the `OBJECT` state) and
    /// currently assigned to a reclaim queue of `PageQueues`, and `token` proves the `list_lock`
    /// is held.
    #[inline]
    unsafe fn add_loan_replacement(
        &mut self,
        page: VmPagePtr,
        token: &LockToken<'_, PageQueuesListLockClass>,
    ) {
        // SAFETY: Caller guarantees `page` is a valid `VmPagePtr` attached to a VM object.
        let raw_page = unsafe { page.as_ref() };
        debug_assert!(!raw_page.is_loaned());
        // SAFETY: `raw_page` is in the `OBJECT` state under `list_lock`.
        let cow_ptr = unsafe { raw_page.get_object() };
        debug_assert!(!cow_ptr.is_null());
        // If the VMO does not support borrowing then skip enqueuing the RefPtr, since the
        // replace_page_with_loaned can never succeed anyway.
        // SAFETY: `cow_ptr` is a non-null, aligned `VmCowPages` pointer kept alive by `page`'s
        // membership in `PageQueues` under `list_lock` (a VMO cannot be destroyed while it has
        // pages in `PageQueues`).
        if !unsafe { VmCowPages::from_raw_ref(cow_ptr.cast()) }.can_borrow() {
            return;
        }
        // SAFETY: `cow_ptr` is a valid `VmCowPages` with a positive reference count kept alive by
        // `page` under `list_lock`.
        let cow_ref = unsafe { VmCowPages::upgrade_from_raw(cow_ptr.cast()) };
        debug_assert!(cow_ref.is_some());
        // SAFETY: Caller guarantees `!self.is_full()` and `page` is in the `OBJECT` state under
        // `list_lock`.
        unsafe {
            self.add_internal(cow_ref, page, ListAction::ReplaceWithLoaned, token);
        }
    }

    /// Add a page to be reclaimed. Actual reclamation will only be done if the `set_lru_action` is
    /// compatible with the page and its VMO owner.
    ///
    /// # Safety
    ///
    /// Caller must ensure `!self.is_full()`, `page` is owned by a VMO (in the `OBJECT` state) and
    /// currently assigned to a reclaim queue of `PageQueues`, and `token` proves the `list_lock`
    /// is held.
    #[inline]
    unsafe fn add_reclaimable(
        &mut self,
        page: VmPagePtr,
        token: &LockToken<'_, PageQueuesListLockClass>,
    ) {
        if self.lru_action == LruAction::None {
            return;
        }
        // SAFETY: Caller guarantees `page` is a valid `VmPagePtr` attached to a VM object.
        let raw_page = unsafe { page.as_ref() };
        // SAFETY: `raw_page` is in the `OBJECT` state under `list_lock`.
        let cow_ptr = unsafe { raw_page.get_object() };
        debug_assert!(!cow_ptr.is_null());
        // Need to get the cow refptr before we can check if our lru action is appropriate for this
        // page.
        // SAFETY: `cow_ptr` is a non-null, aligned `VmCowPages` pointer with a positive reference
        // count kept alive by `page` under `list_lock`.
        let cow_ref = unsafe { VmCowPages::upgrade_from_raw(cow_ptr.cast()) };
        debug_assert!(cow_ref.is_some());
        if self.lru_action == LruAction::EvictAndCompress
            || cow_ref.as_ref().is_some_and(|cow| {
                (cow.can_evict() || cow.is_discardable())
                    == (self.lru_action == LruAction::EvictOnly)
            })
        {
            // SAFETY: Caller guarantees `!self.is_full()` and `page` is in the `OBJECT` state
            // under `list_lock`.
            unsafe {
                self.add_internal(cow_ref, page, ListAction::Reclaim, token);
            }
        } else {
            // Must not let the cow refptr get dropped till after the lock, so even if not
            // reclaiming must keep this entry.
            // SAFETY: Caller guarantees `!self.is_full()` and `page` is in the `OBJECT` state
            // under `list_lock`.
            unsafe {
                self.add_internal(cow_ref, page, ListAction::None, token);
            }
        }
    }

    // Performs any pending operations on the stored pages.
    // Requires PageQueues lock NOT be held
    fn flush(&mut self) {
        let count = core::mem::replace(&mut self.count, 0);
        if count == 0 {
            return;
        }

        // Compression state will be lazily instantiated if needed, and then used for any remaining
        // pages in the list. Any items prior to the first Reclaim item are processed without
        // holding the compressor.
        let first_reclaim = self.items[..count]
            .iter()
            // SAFETY: By `LruIsolate`'s invariant, all slots in `0..count` were initialized by
            // `add_internal` and have not yet been consumed.
            .position(|item| unsafe { item.assume_init_ref() }.action == ListAction::Reclaim)
            .unwrap_or(count);
        if first_reclaim > 0 {
            // SAFETY: `0..first_reclaim` is a subrange of `0..count`, and these initialized slots
            // have not yet been consumed.
            unsafe {
                self.flush_range(0..first_reclaim, None);
            }
        }

        // Attempt to acquire any compressor that might exist, unless only evicting. Note that if
        // LruAction::None we would not have enqueued any Reclaim pages, so we can just check for
        // EvictOnly.
        if first_reclaim < count {
            if self.lru_action != LruAction::EvictOnly
                && let Some(comp) = pmm::get_page_compression()
            {
                pin_init::stack_pin_init!(let compressor_guard = comp.acquire_compressor());
                // SAFETY: `first_reclaim..count` is a subrange of `0..count` disjoint from
                // `0..first_reclaim`, so these initialized slots have not yet been consumed.
                unsafe {
                    self.flush_range(first_reclaim..count, Some(compressor_guard.get()));
                }
            } else {
                // SAFETY: `first_reclaim..count` is a subrange of `0..count` disjoint from
                // `0..first_reclaim`, so these initialized slots have not yet been consumed.
                unsafe {
                    self.flush_range(first_reclaim..count, None);
                }
            }
        }
    }

    /// Performs the pending operations for the items in `range`, consuming and dropping them.
    ///
    /// Requires that no spinlocks or VMO locks are held by the caller.
    ///
    /// # Safety
    ///
    /// Caller must ensure every slot in `self.items[range]` is currently initialized and that
    /// `flush_range` is called at most once for each initialized slot before it is re-initialized.
    unsafe fn flush_range(
        &mut self,
        range: core::ops::Range<usize>,
        compressor: Option<&VmCompressor>,
    ) {
        for slot in &mut self.items[range] {
            // SAFETY: Caller guarantees every slot in `range` is initialized and read only once.
            let item = unsafe { slot.assume_init_read() };
            if item.action == ListAction::ReplaceWithLoaned {
                // Only replace with a loaned page if background borrowing/sweeping is still
                // active. We avoid calling replace_page_with_loaned if active unloans are in
                // progress to prevent lock contention on paged_vmo_lock and redundant
                // borrowing/reclaiming work.
                // Note that checking is_borrowing_active() during the initial sweep logic in
                // process_lru_queue is not sufficient because an unloan can start in the window
                // after process_lru_queue evaluates the flag but before we flush the deferred
                // list.
                if PhysicalPageBorrowingConfig::get().is_borrowing_active() {
                    // We ignore the return value because the page may have moved, become pinned,
                    // we may not have any free loaned pages any more, or the VmCowPages may not be
                    // able to borrow.
                    let _ = item.cow.replace_page_with_loaned(item.page, item.offset);
                }
            } else if item.action == ListAction::Reclaim {
                // If using a compressor, make sure it is Armed between reclamations.
                if let Some(c) = compressor
                    && c.arm().is_err()
                {
                    // Continue processing as we might still be able to evict and we need to clear
                    // all the refptrs as well.
                    continue;
                }
                // SAFETY: `item.cow` is a live `RefPtr<VmCowPages>`, `item.page` points to a valid
                // PMM `VmPage`, and `reclaim_page` acquires `item.cow`'s lock to validate whether
                // `page` is still at `offset`.
                let reclaimed = unsafe {
                    item.cow.reclaim_page(
                        item.page,
                        item.offset,
                        EvictionAction::FollowHint,
                        compressor,
                    )
                };
                if let Ok(success) = reclaimed {
                    let num_pages = success.num_pages;
                    let num_loaned_pages = success.num_loaned_pages;

                    if num_pages > 0 || num_loaned_pages > 0 {
                        match success.type_ {
                            VmCowReclaimType::Evict => {
                                PQ_LRU_PAGES_EVICTED.add((num_pages + num_loaned_pages) as i64);
                            }
                            VmCowReclaimType::Discard => {
                                debug_assert_eq!(num_loaned_pages, 0);
                                PQ_LRU_PAGES_DISCARDED.add(num_pages as i64);
                            }
                            VmCowReclaimType::Compress => {
                                debug_assert_eq!(num_loaned_pages, 0);
                                PQ_LRU_PAGES_COMPRESSED.add(num_pages as i64);
                            }
                        }
                    }
                }
            }
        }
    }
}

impl Drop for LruIsolate {
    fn drop(&mut self) {
        self.flush();
    }
}

/// Allocated pages that are part of the cow pages in a VmObjectPaged can be placed in a page queue.
/// The page queues provide a way to
///  * Classify and group pages across VMO boundaries
///  * Retrieve the VMO that a page is contained in (via a back reference stored in the vm_page_t)
///
/// Once a page has been placed in a page queue its queue_node becomes owned by the page queue and
/// must not be used until the page has been [`PageQueues::remove`]'d. It is not sufficient to call
/// list_delete on the queue_node yourself as this operation is not atomic and needs to be performed
/// whilst holding the [`PageQueues`] `list_lock`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PageQueues {
    /// The `list_lock` is used to protect the linked lists queues as these cannot be implemented
    /// with atomics. A few related members are also protected with this lock, such as the isolate
    /// cursor. The purpose of this separate spinlock, compared to the general `lock`, is so that
    /// latency sensitive operations, such as adding / removing pages, that only need to modify the
    /// list, can happen without false contention with other page queues operations.
    #[mutex]
    list_lock: KMutex<RawCriticalMutex>,

    /// General lock used to protect all logic and members that are not part of critical latency
    /// sensitive operations.
    /// Where both locks need to be acquired, `lock` must be acquired prior to the `list_lock`.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    /// Externally supplied event that we should signal anytime aging occurs.
    #[guarded_by(lock)]
    aging_event: *mut Event,

    /// Records whether or not the active aging via the mru thread should be disabled or not.
    #[guarded_by(lock)]
    aging_disabled: bool,

    /// Time at which the `mru_gen` was last incremented.
    // TODO(https://fxbug.dev/549881466): This should be a InstantMono (which is just an i64), but
    // Rust does not yet support arbitrary atomic types like this, even when an
    // #[repr(transparent)]
    last_age_time: AtomicI64,

    /// Reason the last aging event happened, this is purely for informational/debugging purposes.
    /// Initialized to Timeout as a somewhat arbitrary choice.
    #[guarded_by(lock)]
    last_age_reason: AgeReason,

    /// Tracks whether the active ratio has been tripped and should contribute as an aging trigger.
    /// This is stored as a boolean so that it is sticky in the advent of a race with additional
    /// modifications to the page queues. Were this not sticky then, in the absence of a debounce
    /// threshold, we could repeatedly trigger the active ratio on and off, causing the aging
    /// thread to repeatedly wake up, miss the trigger, and do nothing.
    #[guarded_by(lock)]
    active_ratio_triggered: bool,

    /// Used to signal the mru thread that it should wake up and check if the mru generation needs
    /// incrementing. This must be signaled if `active_ratio_triggered` transitions false->true or
    /// if the `lru_gen` is incremented. Over signalling is safe, just less efficient. Only the mru
    /// thread is permitted to wait on this.
    #[pin]
    mru_event: AutounsignalEvent,

    /// Used to signal the lru thread that it should wake up and check if the lru queue needs
    /// processing. This must be signaled if the `mru_gen` is modified such that the lru queue
    /// needs processing. Over signalling is safe, just less efficient. Only the lru thread is
    /// permitted to wait on this.
    #[pin]
    lru_event: AutounsignalEvent,

    /// What to do with pages when processing the LRU queue.
    #[guarded_by(lock)]
    lru_action: LruAction,

    /// The page queues are placed into an array, indexed by page queue, for consistency and
    /// uniformity of access. This does mean that the list for PageQueueNone does not actually have
    /// any pages in it, and should always be empty.
    /// The reclaimable queues are the more complicated as, unlike the other categories, pages can
    /// be in one of the queues, and can move around. The reclaimable queues themselves store pages
    /// that are roughly grouped by their last access time. The relationship is not precise as
    /// pages are not moved between queues unless it becomes strictly necessary. This is in
    /// contrast to the queue counts that are always up to date.
    ///
    /// What this means is that the `VmPage::page_queue` index is always up to do date, and the
    /// `page_queue_counts` represent an accurate count of pages with that `VmPage::page_queue`
    /// index, but counting the pages actually in the linked list may not yield the correct number.
    ///
    /// New reclaimable pages are always placed into the queue associated with the MRU generation.
    /// If they get accessed the `VmPage::page_queue` gets updated along with the counts. At some
    /// point the LRU queue will get processed and this will cause pages to get relocated to their
    /// correct list.
    ///
    /// Consider the following example:
    ///
    /// ```text
    ///  LRU  MRU            LRU  MRU            LRU   MRU            LRU   MRU        MRU  LRU
    ///    |  |                |  |                |     |              |     |            |  |
    ///    |  |    Insert A    |  |    Age         |     |  Touch A     |     |  Age       |  |
    ///    V  v    Queue=2     v  v    Queue=2     v     v  Queue=3     v     v  Queue=3   v  v
    /// [][ ][ ][] -------> [][ ][a][] -------> [][ ][a][ ] -------> [][ ][a][ ] -------> [ ][ ][a][]
    /// ```
    ///
    /// At this point page A, in its `VmPage`, has its queue marked as 3, and the
    /// `page_queue_counts` are {0,0,1,0}, but the page itself remains in the linked list for queue
    /// 2. If the LRU queue is then processed to increment it we would do.
    ///
    /// ```text
    ///  MRU  LRU             MRU  LRU            MRU    LRU
    ///    |  |                 |    |              |      |
    ///    |  |       Move LRU  |    |    Move LRU  |      |
    ///    V  v       Queue=3   v    v    Queue=3   v      v
    ///   [ ][ ][a][] -------> [ ][][a][] -------> [][ ][][a]
    /// ```
    ///
    /// In the second processing of the LRU queue it gets noticed that the page, based on
    /// `VmPage::page_queue`, is in the wrong queue and gets moved into the correct one.
    ///
    /// For specifics on how LRU and MRU generations map to LRU and MRU queues, see comments on
    /// `lru_gen` and `mru_gen`.
    #[guarded_by(list_lock)]
    #[pin]
    page_queues: [VmPageDoublyLinkedList; PageQueue::NUM_QUEUES.0 as usize],

    /// When a page is in the PageQueueReclaimIsolate state, instead of being in the `page_queues`
    /// list it is in, potentially one of several different, `isolate_queues` lists. This is just
    /// an implementation simplification as there's no need to 'save' the memory of the unused
    /// list_node_t in the other `page_queues` array. Note that PageQueueReclaimIsolate state is
    /// not a queue index, i.e. a page will have the PageQueueReclaimIsolate state irrespective
    /// of which `isolate_queues` list it is in.
    /// Pages in the PageQueueReclaimIsolate state are always exactly in the `isolate_queues` list,
    /// and similarly any page in the `isolate_queues` list is exactly in the
    /// PageQueueReclaimIsolate state. In this way the `isolate_queues` are considered reclaimable,
    /// and are part of active/inactive tracking, but do not support the
    /// [`PageQueues::mark_accessed`] fastpath.
    #[guarded_by(list_lock)]
    #[pin]
    isolate_queues: [VmPageDoublyLinkedList; NUM_ISOLATE_QUEUES],

    /// The generation counts are monotonic increasing counters and used to represent the effective
    /// age of the oldest and newest reclaimable queues. The page queues themselves are treated as
    /// a fixed size circular buffer that the generations map onto. This means all pages in the
    /// system have an age somewhere in `[lru_gen, mru_gen]` and so the lru and mru generations
    /// cannot drift apart by more than [`PageQueues::NUM_RECLAIM`], otherwise there would not
    /// be enough queues.
    /// A pages age being between `[lru_gen, mru_gen]` is not an invariant as
    /// [`PageQueues::mark_accessed`] can race and mark pages as being in an invalid queue. This
    /// race will get noticed when the LRU queue is processed and the page will get updated at that
    /// point to have a valid queue. Importantly, whilst pages can think they are in a queue that
    /// is invalid, only valid linked lists in the `page_queues` will ever have pages in them.
    /// This invariant is easy to enforce as the `page_queues` are updated under a lock.
    /// These are atomic so they can be safely read without the lock held, however they are always
    /// modified with the lock hold.
    lru_gen: AtomicU64,
    mru_gen: AtomicU64,

    /// Tracks the counts of pages in each queue in O(1) time complexity. As pages are moved
    /// between queues, the corresponding source and destination counts are decremented and
    /// incremented, respectively.
    ///
    /// The first entry of the array is left special: it logically represents pages not in any
    /// queue. For simplicity, it is initialized to zero rather than the total number of pages in
    /// the system. Consequently, the value of this entry is a negative number with absolute value
    /// equal to the total number of pages in all queues. This approach avoids unnecessary branches
    /// when updating counts.
    page_queue_counts: [AtomicUsize; PageQueue::NUM_QUEUES.0 as usize],

    /// Count for how many pages have moved queue without us recalculating the active ratio. This
    /// is a [`RelaxedAtomic`] to allow for completely skipping lock acquisition in
    /// [`PageQueues::mark_accessed`], except when the ratio actually needs to be recalculated.
    lazy_active_ratio_aging_skips: RelaxedAtomicU64,

    /// Tracks the number of consecutive LRU queue processing iterations that skipped sweeping due
    /// to active unloans.
    consecutive_skipped_sweeps: RelaxedAtomicU32,

    /// Track the mru and lru threads and have a signalling mechanism to shut them down.
    shutdown_threads: AtomicBool,
    #[guarded_by(lock)]
    mru_thread: *mut Thread,
    #[guarded_by(lock)]
    lru_thread: *mut Thread,

    /// Debug compressor is only available when debug asserts are also enabled. This ensures it can
    /// never have an impact on production builds.
    ///
    /// As the #[guarded] macro does not support #[cfg] members in #[guarded_by] annotates this is
    /// manually wrapped in an UnsafeCell and its access is controlled by list_lock.
    #[cfg(debug_assertions)]
    debug_compressor: UnsafeCell<Option<kalloc::Box<super::debug_compressor::DebugCompressor>>>,

    /// Queue rotation parameters. These are not locked as they are only read by the mru thread,
    /// and are set before the mru thread is started.
    min_mru_rotate_time: UnsafeCell<DurationMono>,
    max_mru_rotate_time: UnsafeCell<DurationMono>,

    /// Determines if anonymous zero page forks are placed in the zero fork queue or in the
    /// reclaimable queue.
    zero_fork_is_reclaimable: RelaxedAtomicBool,

    /// Determines if anonymous pages are placed in the reclaimable queues, or in their own non
    /// aging anonymous queues.
    anonymous_is_reclaimable: RelaxedAtomicBool,

    /// Current active ratio multiplier.
    #[guarded_by(lock)]
    active_ratio_multiplier: i64,

    phantom: PhantomData<PhantomPinned>,
}

// Compile-time layout assertions against the C++ PageQueues type via bindgen.
zr::static_assert!(
    core::mem::size_of::<PageQueues>() == core::mem::size_of::<bindings::PageQueues>()
);
zr::static_assert!(
    core::mem::align_of::<PageQueues>() == core::mem::align_of::<bindings::PageQueues>()
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, list_lock)
        == core::mem::offset_of!(bindings::PageQueues, list_lock_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, lock) == core::mem::offset_of!(bindings::PageQueues, lock_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, aging_event)
        == core::mem::offset_of!(bindings::PageQueues, aging_event_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, aging_disabled)
        == core::mem::offset_of!(bindings::PageQueues, aging_disabled_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, last_age_time)
        == core::mem::offset_of!(bindings::PageQueues, last_age_time_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, last_age_reason)
        == core::mem::offset_of!(bindings::PageQueues, last_age_reason_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, active_ratio_triggered)
        == core::mem::offset_of!(bindings::PageQueues, active_ratio_triggered_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, mru_event)
        == core::mem::offset_of!(bindings::PageQueues, mru_event_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, lru_event)
        == core::mem::offset_of!(bindings::PageQueues, lru_event_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, lru_action)
        == core::mem::offset_of!(bindings::PageQueues, lru_action_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, page_queues)
        == core::mem::offset_of!(bindings::PageQueues, page_queues_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, isolate_queues)
        == core::mem::offset_of!(bindings::PageQueues, isolate_queues_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, lru_gen)
        == core::mem::offset_of!(bindings::PageQueues, lru_gen_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, mru_gen)
        == core::mem::offset_of!(bindings::PageQueues, mru_gen_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, page_queue_counts)
        == core::mem::offset_of!(bindings::PageQueues, page_queue_counts_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, lazy_active_ratio_aging_skips)
        == core::mem::offset_of!(bindings::PageQueues, lazy_active_ratio_aging_skips_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, consecutive_skipped_sweeps)
        == core::mem::offset_of!(bindings::PageQueues, consecutive_skipped_sweeps_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, shutdown_threads)
        == core::mem::offset_of!(bindings::PageQueues, shutdown_threads_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, mru_thread)
        == core::mem::offset_of!(bindings::PageQueues, mru_thread_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, lru_thread)
        == core::mem::offset_of!(bindings::PageQueues, lru_thread_)
);
#[cfg(debug_assertions)]
zr::static_assert!(
    core::mem::offset_of!(PageQueues, debug_compressor)
        == core::mem::offset_of!(bindings::PageQueues, debug_compressor_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, min_mru_rotate_time)
        == core::mem::offset_of!(bindings::PageQueues, min_mru_rotate_time_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, max_mru_rotate_time)
        == core::mem::offset_of!(bindings::PageQueues, max_mru_rotate_time_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, zero_fork_is_reclaimable)
        == core::mem::offset_of!(bindings::PageQueues, zero_fork_is_reclaimable_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, anonymous_is_reclaimable)
        == core::mem::offset_of!(bindings::PageQueues, anonymous_is_reclaimable_)
);
zr::static_assert!(
    core::mem::offset_of!(PageQueues, active_ratio_multiplier)
        == core::mem::offset_of!(bindings::PageQueues, active_ratio_multiplier_)
);

// SAFETY: `PageQueues` is internally synchronized via its own `lock` and `list_lock`, and the
// pointers it holds only reference objects that outlive it.
unsafe impl Send for PageQueues {}
// SAFETY: `PageQueues` methods operate on shared references `&self` concurrently across threads,
// with all mutable state protected by its internal locks or held in atomics.
unsafe impl Sync for PageQueues {}

#[pin_init::pinned_drop]
impl PinnedDrop for PageQueues {
    fn drop(self: Pin<&mut Self>) {
        // The remainder of the C++ destructor, i.e. tearing down the locks, the events and the
        // queues themselves, is performed by the drop glue of the individual members.
        // SAFETY: `self` is a live, pinned `PageQueues` for the duration of this call.
        unsafe { bindings::cpp_page_queues_stop_threads(self.as_raw()) };

        // SAFETY: We hold an exclusive reference to `self` and, having just stopped the mru and lru
        // threads, there can be no other references to the queues, so the `list_lock` does not need
        // to be acquired to inspect them.
        unsafe {
            let token = LockToken::new();
            for queue in self.page_queues.get(&token) {
                debug_assert!(queue.is_empty());
            }
        }
        for (i, count) in self.page_queue_counts.iter().enumerate() {
            let count = count.load(Ordering::Relaxed);
            debug_assert_eq!(count, 0, "i={i} count={count}");
        }
    }
}

impl PageQueues {
    pub const NUM_RECLAIM: usize = NUM_RECLAIM;
    pub const NUM_ACTIVE_QUEUES: usize = NUM_ACTIVE_QUEUES;
    pub const ACTIVE_INACTIVE_ERROR_MARGIN: usize = ACTIVE_INACTIVE_ERROR_MARGIN;
    pub const NUM_OLDEST_QUEUES: usize = NUM_OLDEST_QUEUES;
    pub const ISOLATE_QUEUE_DONT_NEED: usize = ISOLATE_QUEUE_DONT_NEED;
    pub const ISOLATE_QUEUE_STANDARD: usize = ISOLATE_QUEUE_STANDARD;
    pub const NUM_ISOLATE_QUEUES: usize = NUM_ISOLATE_QUEUES;
    pub const DEFAULT_MIN_MRU_ROTATE_TIME: DurationMono = DEFAULT_MIN_MRU_ROTATE_TIME;
    pub const DEFAULT_MAX_MRU_ROTATE_TIME: DurationMono = DEFAULT_MAX_MRU_ROTATE_TIME;
    pub const DEFAULT_ACTIVE_RATIO_MULTIPLIER: u64 = DEFAULT_ACTIVE_RATIO_MULTIPLIER;
    pub const OP_BATCH_SIZE: usize = OP_BATCH_SIZE;

    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        zr::pin_init_ffi!(bindings::cpp_page_queues_init)
    }

    /// Domain-specific conversion: returns raw pointer for `PageQueues`.
    pub fn as_raw(&self) -> *mut bindings::PageQueues {
        (self as *const Self).cast_mut().cast()
    }

    // Converts free running generation to reclaim queue.
    #[inline]
    fn gen_to_queue(generation: u64) -> PageQueue {
        PageQueue(PageQueue::RECLAIM_BASE.0 + (generation % NUM_RECLAIM as u64) as u8)
    }

    // Validates that a given reclaim queue is valid in the inclusive range `[lru, mru]`. Both `lru`
    // and `mru` must be reclaimable queues.
    #[inline]
    fn queue_is_valid(queue: PageQueue, lru: PageQueue, mru: PageQueue) -> bool {
        debug_assert!(queue.0 >= PageQueue::RECLAIM_BASE.0);
        if lru.0 <= mru.0 {
            queue.0 >= lru.0 && queue.0 <= mru.0
        } else {
            queue.0 >= lru.0 || queue.0 <= mru.0
        }
    }

    // Returns whether this queue is reclaimable, and hence can be active or inactive. If this
    // returns false then it is guaranteed that both |queue_is_active| and |queue_is_inactive| would
    // return false.
    #[inline]
    fn queue_is_reclaim(queue: PageQueue) -> bool {
        // We check against the the Isolate queue and not the base queue so that accessing a page
        // can move it from the Isolate list into the LRU queues. To keep this case
        // efficient we require that the Isoalte queue be directly before the LRU queues.
        const _: () = assert!(PageQueue::RECLAIM_ISOLATE.0 + 1 == PageQueue::RECLAIM_BASE.0);

        // Ensure that the Dirty queue comes before the smallest queue that would return true for
        // this function. This function is used for computing active/inactive sets for the
        // purpose of eviction, and dirty pages cannot be evicted. The Dirty queue also
        // needs to come before the Isolate queue so that MarkAccessed does not try to move
        // the page to the MRU queue on access.
        const _: () = assert!(PageQueue::PAGER_BACKED_DIRTY.0 < PageQueue::RECLAIM_ISOLATE.0);
        queue.0 >= PageQueue::RECLAIM_ISOLATE.0
    }

    // Calculates the age of a queue against a given mru, with 0 meaning page_queue==mru.
    // This is only meaningful to call on reclaimable queues.
    #[inline]
    fn queue_age(page_queue: PageQueue, mru: PageQueue) -> u64 {
        debug_assert!(page_queue.0 >= PageQueue::RECLAIM_BASE.0);
        if page_queue.0 <= mru.0 {
            (mru.0 - page_queue.0) as u64
        } else {
            (NUM_RECLAIM as u32 + mru.0 as u32 - page_queue.0 as u32) as u64
        }
    }

    // Returns whether the given page queue would be considered active against a given mru.
    // This is valid to call on any page queue, not just reclaimable ones, and as such this
    // returning false does not imply the queue is inactive.
    #[inline]
    fn queue_is_active(page_queue: PageQueue, mru: PageQueue) -> bool {
        if page_queue.0 < PageQueue::RECLAIM_BASE.0 {
            return false;
        }
        Self::queue_age(page_queue, mru) < NUM_ACTIVE_QUEUES as u64
    }

    // Returns whether the given page queue would be considered inactive against a given mru.
    // This is valid to call on any page queue, not just reclaimable ones, and as such this
    // returning false does not imply the queue is active.
    #[inline]
    fn queue_is_inactive(page_queue: PageQueue, mru: PageQueue) -> bool {
        // The Isolate queue does not have an age, and so we cannot call queue_age on it, but it
        // should definitely be considered part of the inactive set.
        if page_queue == PageQueue::RECLAIM_ISOLATE {
            return true;
        }
        if page_queue.0 < PageQueue::RECLAIM_BASE.0 {
            return false;
        }
        Self::queue_age(page_queue, mru) >= NUM_ACTIVE_QUEUES as u64
    }

    #[inline]
    fn mru_gen_to_queue(&self) -> PageQueue {
        Self::gen_to_queue(self.mru_gen.load(Ordering::Relaxed))
    }

    #[inline]
    fn lru_gen_to_queue(&self) -> PageQueue {
        Self::gen_to_queue(self.lru_gen.load(Ordering::Relaxed))
    }

    // Returns whether or not it is permissible to increase the mru generation, or if the lru
    // generation would need incrementing first. This is marked as requiring lock as, even though
    // the annotation is not exercised, the return value of this method is only useful /
    // non-racy if the mru/lru generations cannot change, which requires holding the lock to
    // guarantee.
    #[inline]
    fn can_increment_mru_gen_locked(&self, _token: &LockToken<'_, PageQueuesLockClass>) -> bool {
        self.mru_gen.load(Ordering::Relaxed) - self.lru_gen.load(Ordering::Relaxed)
            < NUM_RECLAIM as u64 - 1
    }

    // Similar to |can_increment_mru_gen_locked|, but for the lru.
    #[inline]
    fn can_increment_lru_gen_locked(&self, _token: &LockToken<'_, PageQueuesLockClass>) -> bool {
        self.mru_gen.load(Ordering::Relaxed) - self.lru_gen.load(Ordering::Relaxed)
            > NUM_ACTIVE_QUEUES as u64
    }

    // Records that |pages| have potentially changed queue impacting the active/inactive ratio, and
    // returns |true| if checking the active ratio can be skipped.
    #[inline]
    fn record_active_ratio_skips(&self, pages: usize) -> bool {
        // Add the pages to the skip count and check if our specific addition caused the count to
        // cross the threshold. This prevents a thundering herd of threads all noticing once
        // the count passes the threshold.
        let old_count = self.lazy_active_ratio_aging_skips.fetch_add(pages as u64);
        if old_count < ACTIVE_INACTIVE_ERROR_MARGIN as u64
            && old_count + (pages as u64) >= ACTIVE_INACTIVE_ERROR_MARGIN as u64
        {
            // Reset the skips counter to zero. This possibly loses some counts, but as the active
            // ratio has not yet been checked, this is fine.
            self.lazy_active_ratio_aging_skips.store(0);
            return false;
        }
        true
    }

    // Potentially calls |check_active_ratio_aging_locked| based on the
    // ACTIVE_INACTIVE_ERROR_MARGIN. |pages| indicates how many pages might have changed queue, and
    // hence how much the ratio could have changed by.
    #[inline]
    fn maybe_check_active_ratio_aging(&self, pages: usize) {
        if !self.record_active_ratio_skips(pages) {
            #[cold]
            fn cold_path(pq: &PageQueues) {
                ksync::lock!(let mut guard = pq.lock_lock());
                pq.check_active_ratio_aging_locked(&mut guard);
            }
            cold_path(self);
        }
    }

    // Checks if the active ratio has exceeded the threshold to cause aging, and if so signals the
    // event.
    fn check_active_ratio_aging_locked(&self, guard: &mut Pin<&mut PageQueuesLockGuard<'_>>) {
        if *guard.as_mut().fields().active_ratio_triggered {
            // Already triggered, nothing more to do.
            return;
        }
        if self.is_active_ratio_triggering_aging(guard) {
            *guard.as_mut().fields_mut().active_ratio_triggered = true;
            self.mru_event.signal();
        }
    }

    // Helper method that calculates whether the current active ratio would trigger aging.
    fn is_active_ratio_triggering_aging(&self, guard: &Pin<&mut PageQueuesLockGuard<'_>>) -> bool {
        let counts = self.get_active_inactive_counts();
        counts.active * (*guard.fields().active_ratio_multiplier as usize) > counts.inactive
    }

    // Helpers for adding and removing to the queues. All of the public Set/Move/Remove operations
    // are convenience wrappers around these.

    /// # Safety
    ///
    /// Caller must guarantee that `page` is owned by `cow` (in the `OBJECT` state) and not
    /// currently assigned to a `PageQueue`.
    #[inline]
    unsafe fn set_queue_backlink_locked_list(
        &self,
        page: VmPagePtr,
        cow: &VmCowPages,
        page_offset: u64,
        queue: PageQueue,
        list_guard: &mut Pin<&mut PageQueuesListLockGuard<'_>>,
    ) {
        debug_assert!(queue != PageQueue::RECLAIM_ISOLATE);
        let fields = list_guard.as_mut().fields_mut();
        // SAFETY: Caller guarantees `page` is attached to a VM object and not currently in a
        // container, `fields.page_queues` is pinned in memory, and `list_lock` is held.
        unsafe {
            let raw = page.as_ref();
            debug_assert_eq!(raw.state().0, vm_page_state::OBJECT);
            debug_assert!(!raw.is_free());
            debug_assert!(!raw.get_node().in_container());
            debug_assert!(raw.get_object().is_null());
            debug_assert_eq!(raw.get_page_offset(), 0);

            raw.set_object(cow.as_raw().cast());
            raw.set_page_offset(page_offset);

            let queue_ref = raw.get_page_queue_ref();
            debug_assert_eq!(queue_ref.load(Ordering::Relaxed), PageQueue::NONE.0);
            queue_ref.store(queue.0, Ordering::Relaxed);

            let page_queues = fields.page_queues.get_unchecked_mut();
            Pin::new_unchecked(&mut page_queues[queue.0 as usize])
                .push_front_raw(page.as_non_null());
        }
        self.page_queue_counts[queue.0 as usize].fetch_add(1, Ordering::Relaxed);
    }

    /// # Safety
    ///
    /// Caller must guarantee that `page` is owned by a VMO and currently assigned to this
    /// `PageQueue`. `_token` proves the `list_lock` is held.
    #[inline]
    unsafe fn remove_locked_list(
        &self,
        page: VmPagePtr,
        _token: &LockToken<'_, PageQueuesListLockClass>,
    ) {
        // Directly exchange the old gen.
        // SAFETY: Caller guarantees `page` is attached to a VM object, currently in a queue of
        // `self`, and `list_lock` is held.
        unsafe {
            let raw = page.as_ref();
            let old_queue = raw.get_page_queue_ref().swap(PageQueue::NONE.0, Ordering::Relaxed);
            debug_assert_ne!(old_queue, PageQueue::NONE.0);
            self.page_queue_counts[old_queue as usize].fetch_sub(1, Ordering::Relaxed);
            raw.set_object(core::ptr::null_mut());
            raw.set_page_offset(0);
            remove_page_from_list_node(raw);
        }
    }

    /// # Safety
    ///
    /// Caller must guarantee that `page` is owned by a VMO and currently assigned to this
    /// `PageQueue`.
    #[inline]
    unsafe fn move_to_queue_locked_list(
        &self,
        page: VmPagePtr,
        queue: PageQueue,
        list_guard: &mut Pin<&mut PageQueuesListLockGuard<'_>>,
    ) {
        debug_assert!(queue != PageQueue::RECLAIM_ISOLATE);
        // SAFETY: Caller guarantees `page` is attached to a VM object and currently in a queue of
        // `self`, `fields.page_queues` is pinned in memory, and `list_lock` is held.
        unsafe {
            let raw = page.as_ref();
            debug_assert_eq!(raw.state().0, vm_page_state::OBJECT);
            debug_assert!(!raw.is_free());
            debug_assert!(raw.get_node().in_container());
            debug_assert!(!raw.get_object().is_null());

            let old_queue = raw.get_page_queue_ref().swap(queue.0, Ordering::Relaxed);
            debug_assert_ne!(old_queue, PageQueue::NONE.0);

            remove_page_from_list_node(raw);
            let fields = list_guard.as_mut().fields_mut();
            let page_queues = fields.page_queues.get_unchecked_mut();
            Pin::new_unchecked(&mut page_queues[queue.0 as usize])
                .push_front_raw(page.as_non_null());
            self.page_queue_counts
                .get_unchecked(old_queue as usize)
                .fetch_sub(1, Ordering::Relaxed);
            self.page_queue_counts[queue.0 as usize].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// # Safety
    ///
    /// Caller must guarantee that `page` is owned by a VMO and currently assigned to this
    /// `PageQueue`.
    #[inline]
    unsafe fn move_to_isolate_locked_list(
        &self,
        page: VmPagePtr,
        isolate_queue_index: usize,
        list_guard: &mut Pin<&mut PageQueuesListLockGuard<'_>>,
    ) {
        debug_assert!(isolate_queue_index < NUM_ISOLATE_QUEUES);
        // SAFETY: Caller guarantees `page` is attached to a VM object and currently in a queue of
        // `self`, `fields.isolate_queues` is pinned in memory, and `list_lock` is held.
        unsafe {
            let raw = page.as_ref();
            debug_assert_eq!(raw.state().0, vm_page_state::OBJECT);
            debug_assert!(!raw.is_free());
            debug_assert!(raw.get_node().in_container());
            debug_assert!(!raw.get_object().is_null());

            let old_queue =
                raw.get_page_queue_ref().swap(PageQueue::RECLAIM_ISOLATE.0, Ordering::Relaxed);
            debug_assert_ne!(old_queue, PageQueue::NONE.0);

            remove_page_from_list_node(raw);
            let fields = list_guard.as_mut().fields_mut();
            let isolate_queues = fields.isolate_queues.get_unchecked_mut();
            Pin::new_unchecked(&mut isolate_queues[isolate_queue_index])
                .push_back_raw(page.as_non_null());
            self.page_queue_counts
                .get_unchecked(old_queue as usize)
                .fetch_sub(1, Ordering::Relaxed);
            self.page_queue_counts[PageQueue::RECLAIM_ISOLATE.0 as usize]
                .fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Internal helper for MarkAccessed that handles the case where the page might be in the
    /// Isolate queue. Pages in the isolate queue must actually be moved out of the list, and this
    /// requires taking the lock.
    /// # Safety
    ///
    /// Caller must guarantee that `page` is owned by a VMO for the duration of the operation.
    #[cold]
    #[inline(never)]
    unsafe fn mark_accessed_maybe_isolate(&self, page: VmPagePtr) {
        PQ_ACCESSED_ISOLATE.add(1);
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is attached to a VM object and `list_lock` is held.
            let queue_ref = unsafe { page.as_ref().get_page_queue_ref() };
            let old_gen = queue_ref.load(Ordering::Relaxed);
            // With the lock held check if the page is in a reclaim queue. Although it can move
            // between different reclaim queues, it cannot go from reclaim->non-reclaim without the
            // lock.
            if !Self::queue_is_reclaim(PageQueue(old_gen)) {
                PQ_ACCESSED_NOT_RECLAIM.add(1);
                return;
            }
            // SAFETY: `page` was verified to be in a reclaim queue of `self` under `list_lock`.
            unsafe { self.move_to_queue_locked_list(page, self.mru_gen_to_queue(), &mut guard) };
        }
        self.maybe_check_active_ratio_aging(1);
    }

    // Helper that checks if iterations is at a multiple of the OP_BATCH_SIZE, and if so whether or
    // not the lock is presently contested and hence should be yielded.
    #[inline]
    fn batch_op_should_drop_lock(&self, iterations: usize) -> bool {
        if iterations.is_multiple_of(OP_BATCH_SIZE) {
            return self.list_lock.raw_mutex().is_contested();
        }
        false
    }

    // This processes the LRU queue with the goal of increasing the lru_gen to the target_gen. It
    // achieves this by walking all the pages in the queue and doing one of the following:
    // 1. For pages that have a newest accessed time and are in the wrong queue, are moved into the
    // correct queue.
    // 2. For pages that are in the correct queue, they are moved to the Isolate queue.
    // An optional limit for the number of pages to Isolate can be provided and if reached this
    // will return early even if target_gen has not been reached.
    fn process_lru_queue(&self, target_gen: u64, isolate: Option<usize>) {
        // This assertion is <=, and not strictly <, since to evict a some queue X, the target must
        // be X+1. Hence to preserve NUM_ACTIVE_QUEUES, we can allow target_gen to become equal to
        // the first active queue, as this will process all the non-active queues. Although we
        // might refresh our value for the mru_queue, since the mru_gen is monotonic increasing, if
        // this assert passes once it should continue to be true.
        assert!(
            target_gen <= self.mru_gen.load(Ordering::Relaxed) - (NUM_ACTIVE_QUEUES as u64 - 1)
        );

        // Calculate a truly worst case loop iteration count based on every page being in the LRU
        // queue and needing to iterate the LRU multiple steps to the target_gen. Instead of
        // reading the LRU and comparing the target_gen, just add a buffer of the maximum number of
        // page queues.
        let active_inactive = self.get_active_inactive_counts();
        let max_lru_iterations =
            (active_inactive.active + active_inactive.inactive + NUM_RECLAIM) as u64;
        // Loop iteration counting is just for diagnostic purposes.
        let mut loop_iterations: u64 = 0;

        // Pages in this list might be reclaimed or replaced with a loaned page, depending on the
        // action specified in deferred_action. Each of these actions must be done outside the
        // lock, so we accumulate pages and then act after lock is released. The number of items
        // collected per batch is limited to avoid excessive stack usage.
        // The deferred_list is declared here as it is expensive to construct/destruct and we would
        // like to reuse it between iterations.
        let mut deferred_list = LruIsolate::new();

        // Only accumulate pages to try to replace with loaned pages if loaned pages are available
        // and we're allowed to borrow at this code location. We check is_borrowing_active()
        // instead of is_borrowing_on_mru_enabled() to temporarily suspend sweeping if an unloan is
        // currently in progress, avoiding lock contention on the borrowing VMOs' paged_vmo_lock
        // between the LRU thread and the unloaning thread.
        let has_loaned_pages = pmm::node().count_loaned_free_pages() != 0;
        let borrowing_config = PhysicalPageBorrowingConfig::get();
        let do_sweeping = has_loaned_pages && borrowing_config.is_borrowing_active();
        if do_sweeping {
            // Sweeping is active, reset the skipped sweeps counter.
            self.consecutive_skipped_sweeps.store(0);
        } else if borrowing_config.is_borrowing_on_mru_enabled() && has_loaned_pages {
            core::hint::cold_path();
            // Sweeping is enabled and there are loaned free pages to sweep, but we skipped
            // sweeping due to active unloans.
            PQ_LRU_SWEEPING_SKIPPED_UNLOAN.add(1);
            let count = self.consecutive_skipped_sweeps.fetch_add(1) + 1;
            // Log a diagnostic warning if sweeping is skipped for a large number of consecutive
            // iterations.
            if count >= 1000 && count.is_multiple_of(1000) {
                kprintln!(
                    "[pq]: WARNING: LRU page sweeping has been skipped for {count} consecutive \
                     iterations due to active unloans"
                );
            }
        } else {
            // Sweeping is not applicable at all, reset the skipped sweeps counter.
            self.consecutive_skipped_sweeps.store(0);
        }

        let mut isolate_remaining = isolate.unwrap_or(usize::MAX);

        vm_ktrace_duration!(2, "ProcessLruQueue");
        while isolate_remaining > 0 {
            if loop_iterations == max_lru_iterations {
                kernel_oops!(
                    "[pq]: WARNING: process_lru_queue exceeded expected max LRU loop iterations \
                     {}\n",
                    max_lru_iterations
                );
            }
            loop_iterations += 1;

            deferred_list.flush();
            let mut post = false;
            {
                // Need to hold the general lock, in addition to the list lock, so that the lru_gen
                // does not change while we are working.
                ksync::lock!(let guard = self.lock_lock());
                // Fill in the lru action now that the lock is held.
                deferred_list.set_lru_action(*guard.fields().lru_action);

                ksync::lock!(let mut list_guard = self.lock_list_lock());
                let lru = self.lru_gen.load(Ordering::Relaxed);
                if lru >= target_gen {
                    break;
                }
                let mru_queue = self.mru_gen_to_queue();
                let lru_queue = Self::gen_to_queue(lru);

                let mut iterations: usize = 0;
                while !list_guard.fields().page_queues[lru_queue.0 as usize].is_empty()
                    && !deferred_list.is_full()
                    && isolate_remaining > 0
                {
                    // Newer pages accumulate at the head of the list, while older pages are at the
                    // tail. Pages are removed from the tail to ensure FIFO processing (oldest
                    // first).
                    let page_raw = {
                        let fields = list_guard.as_mut().fields_mut();
                        // SAFETY: `fields.page_queues` is pinned in place inside `PageQueues`; we
                        // only mutate the target `VmPageDoublyLinkedList` in place without moving
                        // the array or list head. `pop_back` returns `Some` because `!is_empty()`
                        // was verified above while holding `list_lock`.
                        unsafe {
                            let page_queues = fields.page_queues.get_unchecked_mut();
                            Pin::new_unchecked(&mut page_queues[lru_queue.0 as usize])
                                .pop_back()
                                .unwrap()
                        }
                    };
                    // SAFETY: `page_raw` is a valid non-null pointer to a `VmPage` in the PMM
                    // arena popped from `page_queues[lru_queue]`, and all mutated fields use
                    // interior mutability (`UnsafeCell`).
                    let page_ref = unsafe { page_raw.as_ref() };
                    let page = VmPagePtr::new(page_raw);
                    // SAFETY: `page_ref` is in the OBJECT state while in a reclaim queue under
                    // `list_lock`.
                    let raw_queue =
                        unsafe { page_ref.get_page_queue_ref() }.load(Ordering::Relaxed);
                    let page_queue = PageQueue(raw_queue);
                    debug_assert!(page_queue.0 >= PageQueue::RECLAIM_BASE.0);

                    // If the queue stored in the page does not match then we want to move it to
                    // its correct queue with the caveat that its queue could be invalid. The queue
                    // would be invalid if mark_accessed had raced. Should this happen we know that
                    // the page is actually *very* old, and so we will fall back to the case of
                    // forcibly changing its age to the new lru gen.
                    if page_queue != lru_queue
                        && Self::queue_is_valid(page_queue, lru_queue, mru_queue)
                    {
                        // Pages are added to the head because they are newer pages.
                        // SAFETY: `fields.page_queues` is pinned in place inside `PageQueues` and
                        // mutated in place without moving any list head. `page_raw` was just
                        // popped from `lru_queue` (so it is a valid `NonNull<VmPage>` not in any
                        // container) and remains valid for the duration of its residency in
                        // `page_queues` under `list_lock`.
                        unsafe {
                            let fields = list_guard.as_mut().fields_mut();
                            let page_queues = fields.page_queues.get_unchecked_mut();
                            Pin::new_unchecked(&mut page_queues[page_queue.0 as usize])
                                .push_front_raw(page_raw);
                        }

                        if do_sweeping
                            && !page_ref.is_loaned()
                            && Self::queue_is_active(page_queue, mru_queue)
                        {
                            // SAFETY: `!deferred_list.is_full()` holds from the loop condition,
                            // and `page` is in a reclaim queue in the `OBJECT` state under
                            // `list_lock`.
                            unsafe {
                                deferred_list.add_loan_replacement(page, list_guard.token());
                            }
                        }
                    } else {
                        // Force it the isolate list, don't care about races. If we happened to
                        // access it at the same time then too bad.
                        // Aged pages go to the standard priority isolate queue.
                        // SAFETY: `page_ref` is in the OBJECT state under `list_lock`.
                        let old_queue = PageQueue(
                            unsafe { page_ref.get_page_queue_ref() }
                                .swap(PageQueue::RECLAIM_ISOLATE.0, Ordering::SeqCst),
                        );
                        debug_assert!(old_queue.0 >= PageQueue::RECLAIM_BASE.0);

                        self.page_queue_counts[old_queue.0 as usize]
                            .fetch_sub(1, Ordering::Relaxed);
                        self.page_queue_counts[PageQueue::RECLAIM_ISOLATE.0 as usize]
                            .fetch_add(1, Ordering::Relaxed);
                        // peek_isolate peeks at the head. Pages are added to the tail of the
                        // isolate queue to preserve relative age (older pages at the head, newer
                        // at the tail).
                        // SAFETY: `fields.isolate_queues` is pinned in place inside `PageQueues`
                        // and mutated in place without moving any list head. `page_raw` was just
                        // popped from `lru_queue` (so it is a valid `NonNull<VmPage>` not in any
                        // container) and remains valid for the duration of its residency in
                        // `isolate_queues` under `list_lock`.
                        unsafe {
                            let fields = list_guard.as_mut().fields_mut();
                            let isolate_queues = fields.isolate_queues.get_unchecked_mut();
                            Pin::new_unchecked(&mut isolate_queues[ISOLATE_QUEUE_STANDARD])
                                .push_back_raw(page_raw);
                        }
                        // SAFETY: `!deferred_list.is_full()` holds from the loop condition, and
                        // `page` is in the isolate queue in the `OBJECT` state under `list_lock`.
                        unsafe {
                            deferred_list.add_reclaimable(page, list_guard.token());
                        }
                        isolate_remaining -= 1;
                    }
                    iterations += 1;
                    if self.batch_op_should_drop_lock(iterations) {
                        break;
                    }
                }
                if list_guard.fields().page_queues[lru_queue.0 as usize].is_empty() {
                    // Note that we held the lock the entire time, and lru_gen is always modified
                    // with the lock held, so this should always precisely set lru_gen to lru + 1.
                    let prev = self.lru_gen.fetch_add(1, Ordering::Relaxed);
                    debug_assert_eq!(prev, lru);
                    post = true;
                }
            }
            if post {
                // The lru gen was changed and the MruThread might be waiting for space to
                // increment the mru gen, so kick the MruThread.
                self.mru_event.signal();
            }
        }
    }

    /// Helper for DebugPageIs* methods that checks if `page` is in any reclaim queue and passes
    /// `validator` on its owning `VmCowPages`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    unsafe fn debug_page_is_specific_reclaim<F>(
        &self,
        page: VmPagePtr,
        validator: F,
    ) -> Option<usize>
    where
        F: FnOnce(&VmCowPages) -> bool,
    {
        let cow_pages: Option<RefPtr<VmCowPages>>;
        let result;
        {
            ksync::lock!(let _guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is a valid VmPagePtr.
            let raw = unsafe { page.as_ref() };
            // SAFETY: `raw` is in the OBJECT state under `list_lock`.
            let q = PageQueue(unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed));
            if q.0 < PageQueue::RECLAIM_BASE.0 || q.0 > PageQueue::RECLAIM_LAST.0 {
                return None;
            }
            result = Self::queue_age(q, self.mru_gen_to_queue()) as usize;
            // SAFETY: `raw` is in a reclaim queue under `list_lock`.
            let cow = unsafe { raw.get_object() };
            debug_assert!(!cow.is_null());
            // SAFETY: `cow` is valid and list_lock is held.
            cow_pages = unsafe { VmCowPages::upgrade_from_raw(cow.cast()) };
            debug_assert!(cow_pages.is_some());
        }
        if validator(&cow_pages.unwrap()) { Some(result) } else { None }
    }

    /// Helper for DebugPageIs* methods that checks if `page` is in `queue` and passes `validator`
    /// on its owning `VmCowPages`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    unsafe fn debug_page_is_specific_queue<F>(
        &self,
        page: VmPagePtr,
        queue: PageQueue,
        validator: F,
    ) -> bool
    where
        F: FnOnce(&VmCowPages) -> bool,
    {
        let cow_pages;
        {
            ksync::lock!(let _guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is a valid VmPagePtr.
            let raw = unsafe { page.as_ref() };
            // SAFETY: `raw` is in the OBJECT state under `list_lock`.
            let q = PageQueue(unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed));
            if q != queue {
                return false;
            }
            // SAFETY: `raw` is in `queue` under `list_lock`.
            let cow = unsafe { raw.get_object() };
            debug_assert!(!cow.is_null());
            // SAFETY: `cow` is valid and list_lock is held.
            cow_pages = unsafe { VmCowPages::upgrade_from_raw(cow.cast()) };
            debug_assert!(cow_pages.is_some());
        }
        validator(&cow_pages.unwrap())
    }

    // All Set operations places a page, which must not currently be in a page queue, into the
    // specified queue. The backlink information of |object| and |page_offset| must be specified and
    // valid. If the page is either removed from the referenced object, or moved to a different
    // offset, the backlink information must be updated either by calling ChangeObjectOffsetLocked,
    // or removing the page completely from the queues.

    /// Places `page` into the wired queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, but not yet assigned to a PageQueue.
    #[inline]
    pub unsafe fn set_wired(&self, page: VmPagePtr, cow: &VmCowPages, offset: u64) {
        ksync::lock!(let mut guard = self.lock_list_lock());
        // SAFETY: Caller guarantees `page` is owned by `cow` and not yet assigned to a PageQueue.
        unsafe {
            self.set_queue_backlink_locked_list(page, cow, offset, PageQueue::WIRED, &mut guard);
        }
    }

    /// Places `page` into the anonymous queue.
    ///
    /// `skip_reclaim` controls whether reclaiming the page should be forcibly skipped regardless
    /// of whether anonymous pages are considered reclaimable in general.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, but not yet assigned to a PageQueue.
    #[inline]
    pub unsafe fn set_anonymous(
        &self,
        page: VmPagePtr,
        cow: &VmCowPages,
        offset: u64,
        skip_reclaim: bool,
    ) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is owned by `cow` and not yet assigned to a
            // PageQueue.
            unsafe {
                self.set_queue_backlink_locked_list(
                    page,
                    cow,
                    offset,
                    if self.anonymous_is_reclaimable.load() && !skip_reclaim {
                        self.mru_gen_to_queue()
                    } else {
                        PageQueue::ANONYMOUS
                    },
                    &mut guard,
                );
            }
            #[cfg(debug_assertions)]
            {
                // SAFETY: `list_lock` is held.
                let maybe_dc = unsafe { &*self.debug_compressor.get() };
                if let Some(dc) = maybe_dc.as_ref() {
                    dc.add(page, cow, offset);
                }
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Places `page` into the general reclaimable queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, but not yet assigned to a PageQueue.
    #[inline]
    pub unsafe fn set_reclaim(&self, page: VmPagePtr, cow: &VmCowPages, offset: u64) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is owned by `cow` and not yet assigned to a
            // PageQueue.
            unsafe {
                self.set_queue_backlink_locked_list(
                    page,
                    cow,
                    offset,
                    self.mru_gen_to_queue(),
                    &mut guard,
                );
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Places `page` into the pager backed dirty queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, but not yet assigned to a PageQueue.
    #[inline]
    pub unsafe fn set_pager_backed_dirty(&self, page: VmPagePtr, cow: &VmCowPages, offset: u64) {
        ksync::lock!(let mut guard = self.lock_list_lock());
        // SAFETY: Caller guarantees `page` is owned by `cow` and not yet assigned to a PageQueue.
        unsafe {
            self.set_queue_backlink_locked_list(
                page,
                cow,
                offset,
                PageQueue::PAGER_BACKED_DIRTY,
                &mut guard,
            );
        }
    }

    /// Places `page` into the anonymous zero fork queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, but not yet assigned to a PageQueue.
    #[inline]
    pub unsafe fn set_anonymous_zero_fork(&self, page: VmPagePtr, cow: &VmCowPages, offset: u64) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is owned by `cow` and not yet assigned to a
            // PageQueue.
            unsafe {
                self.set_queue_backlink_locked_list(
                    page,
                    cow,
                    offset,
                    if self.zero_fork_is_reclaimable.load() {
                        self.mru_gen_to_queue()
                    } else {
                        PageQueue::ANONYMOUS_ZERO_FORK
                    },
                    &mut guard,
                );
            }
            #[cfg(debug_assertions)]
            {
                // SAFETY: `list_lock` is held.
                let maybe_dc = unsafe { &*self.debug_compressor.get() };
                if let Some(dc) = maybe_dc.as_ref() {
                    dc.add(page, cow, offset);
                }
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Places `page` into the high priority queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, but not yet assigned to a PageQueue.
    #[inline]
    pub unsafe fn set_high_priority(&self, page: VmPagePtr, cow: &VmCowPages, offset: u64) {
        ksync::lock!(let mut guard = self.lock_list_lock());
        // SAFETY: Caller guarantees `page` is owned by `cow` and not yet assigned to a PageQueue.
        unsafe {
            self.set_queue_backlink_locked_list(
                page,
                cow,
                offset,
                PageQueue::HIGH_PRIORITY,
                &mut guard,
            );
        }
    }

    // All Move operations change the queue that a page is considered to be in, but do not change
    // the object or offset backlink information. The page must currently be in a valid page
    // queue.

    /// Moves `page` to the wired queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_to_wired(&self, page: VmPagePtr) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is valid and in a page queue, and `list_lock` is
            // held.
            unsafe { self.move_to_queue_locked_list(page, PageQueue::WIRED, &mut guard) };
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Moves `page` to the anonymous queue.
    ///
    /// `skip_reclaim` controls whether reclaiming the page should be forcibly skipped regardless
    /// of whether anonymous pages are considered reclaimable in general.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_to_anonymous(&self, page: VmPagePtr, skip_reclaim: bool) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is valid and in a page queue, and `list_lock` is
            // held.
            unsafe {
                self.move_to_queue_locked_list(
                    page,
                    if self.anonymous_is_reclaimable.load() && !skip_reclaim {
                        self.mru_gen_to_queue()
                    } else {
                        PageQueue::ANONYMOUS
                    },
                    &mut guard,
                );
                #[cfg(debug_assertions)]
                if let Some(dc) = (*self.debug_compressor.get()).as_ref() {
                    dc.add(
                        page,
                        &*(page.as_ref().get_object() as *const VmCowPages),
                        page.as_ref().get_page_offset(),
                    );
                }
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Moves `page` to the general reclaimable queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_to_reclaim(&self, page: VmPagePtr) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is valid and in a page queue, and `list_lock` is
            // held.
            unsafe {
                self.move_to_queue_locked_list(page, self.mru_gen_to_queue(), &mut guard);
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Moves `page` to the "Don't Need" reclaim queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_to_reclaim_dont_need(&self, page: VmPagePtr) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is valid and in a page queue, and `list_lock` is
            // held.
            unsafe {
                self.move_to_isolate_locked_list(page, ISOLATE_QUEUE_DONT_NEED, &mut guard);
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Moves `page` to the pager backed dirty queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_to_pager_backed_dirty(&self, page: VmPagePtr) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is valid and in a page queue, and `list_lock` is
            // held.
            unsafe {
                self.move_to_queue_locked_list(page, PageQueue::PAGER_BACKED_DIRTY, &mut guard);
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Moves `page` to the high priority queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_to_high_priority(&self, page: VmPagePtr) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is valid and in a page queue, and `list_lock` is
            // held.
            unsafe {
                self.move_to_queue_locked_list(page, PageQueue::HIGH_PRIORITY, &mut guard);
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// If a page is presently in the anonymous (or reclaim queue, depending if anonymous pages are
    /// reclaimable) moves it to the appropriate anonymous zero fork queue (exact queue depends on
    /// whether zero forks are reclaimable or not). If the page is not in the anonymous queue then
    /// it is not modified.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn move_anonymous_to_anonymous_zero_fork(&self, page: VmPagePtr) {
        // First perform a common case short-circuit where the page is already in the anonymous
        // queue and both the anonymous and zero fork queues are the same reclaimable queue. In
        // this case the page is already in the correct queue, and nothing needs to be done.
        // SAFETY: Caller guarantees `page` is in the OBJECT state and resides in a page queue.
        if self.zero_fork_is_reclaimable.load()
            && self.anonymous_is_reclaimable.load()
            && Self::queue_is_reclaim(PageQueue(
                unsafe { page.as_ref().get_page_queue_ref() }.load(Ordering::Relaxed),
            ))
        {
            return;
        }
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // First check if the page is presently in whatever counts as the anonymous queue. If
            // it isn't, then we don't move it.
            // SAFETY: Caller guarantees `page` is in the OBJECT state and resides in a page queue,
            // and `list_lock` is held.
            unsafe {
                let queue = PageQueue(page.as_ref().get_page_queue_ref().load(Ordering::Relaxed));
                if self.anonymous_is_reclaimable.load() && !Self::queue_is_reclaim(queue) {
                    return;
                }
                if !self.anonymous_is_reclaimable.load() && queue != PageQueue::ANONYMOUS {
                    return;
                }
                // In the anonymous queue, move to whatever counts as the anonymous zero fork queue.
                self.move_to_queue_locked_list(
                    page,
                    if self.zero_fork_is_reclaimable.load() {
                        self.mru_gen_to_queue()
                    } else {
                        PageQueue::ANONYMOUS_ZERO_FORK
                    },
                    &mut guard,
                );
                #[cfg(debug_assertions)]
                if let Some(dc) = (*self.debug_compressor.get()).as_ref() {
                    dc.add(
                        page,
                        &*(page.as_ref().get_object() as *const VmCowPages),
                        page.as_ref().get_page_offset(),
                    );
                }
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Indicates that page has failed a compression attempted, and moves it to a separate queue to
    /// prevent it from being considered part of the reclaim set, which makes it neither active nor
    /// inactive. The specified page must be in the page queues, but if not presently in a reclaim
    /// queue this method will do nothing.
    /// TODO(https://fxbug.dev/42138396): Determine whether/how pages are moved back into the
    /// reclaim pool and either further generalize this to support pager backed, or specialize
    /// FailedReclaim to be explicitly only anonymous.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO, and assigned to this PageQueue.
    #[inline]
    pub unsafe fn compress_failed(&self, page: VmPagePtr) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            // Move the page if its currently in some kind of reclaimable queue.
            // SAFETY: Caller guarantees `page` is in the OBJECT state and resides in a page queue,
            // and `list_lock` is held.
            unsafe {
                if Self::queue_is_reclaim(PageQueue(
                    page.as_ref().get_page_queue_ref().load(Ordering::Relaxed),
                )) {
                    self.move_to_queue_locked_list(page, PageQueue::FAILED_RECLAIM, &mut guard);
                }
            }
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Changes the backlink information for a page and should only be called by the page owner
    /// under its lock (that is the VMO lock). The page must currently be in a valid page queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO (and that VMOs lock is held), and
    /// assigned to this PageQueue.
    #[inline]
    pub unsafe fn change_object_offset(
        &self,
        page: VmPagePtr,
        object: &VmCowPages,
        page_offset: u64,
    ) {
        ksync::lock!(let guard = self.lock_list_lock());
        // SAFETY: Caller guarantees `page` is owned by `object` (with its lock held) and `guard`
        // holds `list_lock`.
        unsafe { self.change_object_offset_locked_list(page, object, page_offset, guard.token()) };
    }

    /// Batched version of `change_object_offset`.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `pages` are owned by a VMO, assigned to this PageQueue, and that
    /// `pages` and `offsets` have matching lengths.
    #[inline]
    pub unsafe fn change_object_offset_array(
        &self,
        pages: &[VmPagePtr],
        object: &VmCowPages,
        offsets: &[u64],
    ) {
        debug_assert_eq!(pages.len(), offsets.len());
        let count = pages.len();
        let mut i = 0;
        while i < count {
            ksync::lock!(let guard = self.lock_list_lock());
            // Use a do/while structure for the inner loop to ensure we at least make some progress
            // before checking again for a lock drop.
            loop {
                // SAFETY: Caller guarantees `pages[i]` is owned by `object` and `guard` holds
                // `list_lock`.
                unsafe {
                    self.change_object_offset_locked_list(
                        pages[i],
                        object,
                        offsets[i],
                        guard.token(),
                    )
                };
                i += 1;
                if i >= count || self.batch_op_should_drop_lock(i) {
                    break;
                }
            }
        }
    }

    /// Externally locked variant of `change_object_offset` that can be used for more efficient
    /// batch operations. In addition to the annotated lock, the VMO lock of the owner is also
    /// required to be held.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO (and its lock is held) and assigned to
    /// this PageQueue.`_token` proves the page queues `list_lock` is held.
    #[inline]
    pub unsafe fn change_object_offset_locked_list(
        &self,
        page: VmPagePtr,
        object: &VmCowPages,
        page_offset: u64,
        _token: &LockToken<'_, PageQueuesListLockClass>,
    ) {
        // SAFETY: Caller guarantees `page` is in the OBJECT state, currently resides in a valid
        // page queue, and both the VMO lock and `list_lock` are held.
        unsafe {
            let raw = page.as_ref();
            debug_assert_eq!(raw.state().0, vm_page_state::OBJECT);
            debug_assert!(!raw.is_free());
            debug_assert!(raw.get_node().in_container());
            debug_assert!(!raw.get_object().is_null());
            raw.set_object(object as *const VmCowPages as *mut core::ffi::c_void);
            raw.set_page_offset(page_offset);
        }
    }

    /// Removes the page from any page list and returns ownership of the queue_node.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    #[inline]
    pub unsafe fn remove(&self, page: VmPagePtr) {
        {
            ksync::lock!(let guard = self.lock_list_lock());
            // SAFETY: Caller guarantees `page` is owned by a VMO and assigned to this PageQueue,
            // and `guard` holds `list_lock`.
            unsafe { self.remove_locked_list(page, guard.token()) };
        }
        self.maybe_check_active_ratio_aging(1);
    }

    /// Batched version of `remove` that also places all the pages in the specified list.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `pages` are owned by a VMO and assigned to this PageQueue.
    #[inline]
    pub unsafe fn remove_array_into_list(
        &self,
        pages: &[VmPagePtr],
        mut out_list: Pin<&mut VmPageDoublyLinkedList>,
    ) {
        let count = pages.len();
        let mut i = 0;
        while i < count {
            ksync::lock!(let guard = self.lock_list_lock());
            // Use a do/while structure for the inner loop to ensure we at least make some progress
            // before checking again for a lock drop.
            loop {
                // SAFETY: Caller guarantees `pages[i]` is assigned to this PageQueue, `guard`
                // holds `list_lock`, and `pages[i]` is removed from queues before being pushed
                // into `out_list`.
                unsafe {
                    self.remove_locked_list(pages[i], guard.token());
                    out_list.as_mut().push_back_raw(pages[i].as_non_null());
                }
                i += 1;
                if i >= count || self.batch_op_should_drop_lock(i) {
                    break;
                }
            }
        }
        self.maybe_check_active_ratio_aging(count);
    }

    /// Tells the page queue this page has been accessed, and it should have its position in the
    /// queues updated.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that `page` is owned by a VMO for the duration of the operation.
    #[inline]
    pub unsafe fn mark_accessed(&self, page: VmPagePtr) {
        PQ_ACCESSED_NORMAL.add(1);
        // SAFETY: Caller guarantees `page` is owned by a VMO (in OBJECT state) for the duration of
        // this call.
        let queue_ref = unsafe { page.as_ref().get_page_queue_ref() };
        let mut old_gen = queue_ref.load(Ordering::Relaxed);
        let target_queue = self.mru_gen_to_queue();
        if old_gen == target_queue.0 {
            PQ_ACCESSED_NORMAL_SAME_QUEUE.add(1);
            return;
        }
        loop {
            // If we ever find old_gen to not be in the active/inactive range then this means the
            // page has either been racily removed from, or was never in, the reclaim queue. In
            // which case we can return as there's nothing to be marked accessed.
            if !Self::queue_is_reclaim(PageQueue(old_gen)) {
                PQ_ACCESSED_NOT_RECLAIM.add(1);
                return;
            }
            // The Isolate queue is a reclaim queue, but we cannot just change the queue in the
            // vm_page_t and must actually move it between the lists.
            if old_gen == PageQueue::RECLAIM_ISOLATE.0 {
                // SAFETY: Caller guarantees `page` is owned by a VMO for the duration of the
                // operation.
                unsafe { self.mark_accessed_maybe_isolate(page) };
                return;
            }
            // Between loading the mru_gen and finally storing it in the queue_ref it's possible for
            // our calculated target_queue to become invalid. This is extremely unlikely as it would
            // require us to stall for long enough for the lru_gen to pass this point, but if it
            // does happen then ProcessLruQueues will notice our queue is invalid and correct our
            // age to be that of lru_gen.
            if let Err(actual) = queue_ref.compare_exchange_weak(
                old_gen,
                target_queue.0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                old_gen = actual;
            } else {
                break;
            }
        }
        // Use get_unchecked to query the page_queue_counts when using old_gen, since the compiler
        // does not know that it is within the bounds and will otherwise emit a needless bounds
        // check. target_queue.0 is statically known to be within the bounds, so regular indexing
        // does not emit the bounds check.
        // SAFETY: `old_gen` is a valid page queue index (`< NUM_PAGE_QUEUES`).
        unsafe {
            self.page_queue_counts.get_unchecked(old_gen as usize).fetch_sub(1, Ordering::Relaxed);
        }
        self.page_queue_counts[target_queue.0 as usize].fetch_add(1, Ordering::Relaxed);
        self.maybe_check_active_ratio_aging(1);
    }

    /// Provides access to the underlying lock, allowing _Locked variants to be called. Use of this
    /// is highly discouraged as the underlying lock is a CriticalMutex which disables
    /// preemption. Preferably *Array variations should be used, but this provides a higher
    /// performance mechanism when needed.
    pub fn get_lock(&self) -> &KMutex<PageQueuesListLockClass, RawCriticalMutex> {
        let lock = unsafe { bindings::cpp_page_queues_get_lock(self.as_raw()) };
        // SAFETY: The returned lock pointer is `&self.list_lock_`, valid for the lifetime of
        // `self`.
        unsafe { &*lock.cast::<KMutex<PageQueuesListLockClass, RawCriticalMutex>>() }
    }

    /// Returns a string representation of the given `AgeReason`.
    pub fn string_from_age_reason(reason: AgeReason) -> &'static CStr {
        let ptr = unsafe { bindings::cpp_page_queues_string_from_age_reason(reason as i32) };
        // SAFETY: `cpp_page_queues_string_from_age_reason` returns a pointer to a static C string
        // literal.
        unsafe { CStr::from_ptr(ptr) }
    }

    /// Performs a manually requested aging event. This ignores usual aging triggers / restrictions
    /// and waits, if necessary, for aging to be possible and then performs it.
    /// Only for tests and debugging.
    pub fn rotate_reclaim_queues(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_rotate_reclaim_queues(self.as_raw()) }
    }

    /// Moves a page from from the anonymous zero fork queue into the anonymous queue and returns
    /// the backlink information. If the zero fork queue is empty then a nullopt is returned,
    /// otherwise if it has_value the vmo field may be null to indicate that the vmo is running
    /// its destructor (see VmoBacklink for more details).
    pub fn pop_anonymous_zero_fork(&self) -> Option<VmoBacklink> {
        let mut out = MaybeUninit::<bindings::PageQueuesVmoBacklink>::uninit();
        // SAFETY: `self.as_raw()` is a valid pointer and `out` is valid for writing.
        let has_value = unsafe {
            bindings::cpp_page_queues_pop_anonymous_zero_fork(self.as_raw(), out.as_mut_ptr())
        };
        if has_value {
            let out = unsafe { out.assume_init() };
            let page = unsafe { VmPagePtr::from_ffi(out.page).expect("null page provided") };
            let cow = unsafe { VmCowPages::from_raw(out.cow.cast()) };
            Some(VmoBacklink { cow, page, offset: out.offset })
        } else {
            None
        }
    }

    /// Looks at the isolate queues and returns backlink information of the first page found. If the
    /// isolate queue is empty then LRU queues up to |lowest_queue| epochs from the most recent will
    /// be processed to attempt to fill the isolate list. If no page was found a nullopt is
    /// returned, otherwise if it has_value the vmo field may be null to indicate that the vmo
    /// is running its destructor (see VmoBacklink for more details). If a page is returned its
    /// location in the reclaim queue is not modified.
    pub fn peek_isolate(&self, lowest_queue: usize) -> Option<VmoBacklink> {
        let mut out = MaybeUninit::<bindings::PageQueuesVmoBacklink>::uninit();
        // SAFETY: `self.as_raw()` is a valid pointer and `out` is valid for writing.
        let has_value = unsafe {
            bindings::cpp_page_queues_peek_isolate(self.as_raw(), lowest_queue, out.as_mut_ptr())
        };
        if has_value {
            let out = unsafe { out.assume_init() };
            let page = unsafe { VmPagePtr::from_ffi(out.page).expect("null page provided") };
            let cow = unsafe { VmCowPages::from_raw(out.cow.cast()) };
            Some(VmoBacklink { cow, page, offset: out.offset })
        } else {
            None
        }
    }

    /// Can be called while the |page| is known to be in the loaned state. This method checks if it
    /// is in the page queues, and if so returns a reference to the cow pages that owns it.
    /// The page must be 'owned' by the caller, in so far as the page->state() is guaranteed to not
    /// be changing.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is valid and in the loaned state with state not changing.
    pub unsafe fn get_cow_for_loaned_page(&self, page: VmPagePtr) -> Option<VmoBacklink> {
        let mut out = MaybeUninit::<bindings::PageQueuesVmoBacklink>::uninit();
        // SAFETY: `self.as_raw()` is valid, and caller guarantees preconditions on `page`.
        let has_value = unsafe {
            bindings::cpp_page_queues_get_cow_for_loaned_page(
                self.as_raw(),
                page.as_ffi(),
                out.as_mut_ptr(),
            )
        };
        if has_value {
            let out = unsafe { out.assume_init() };
            let page = unsafe { VmPagePtr::from_ffi(out.page).expect("null page provided") };
            let cow = unsafe { VmCowPages::from_raw(out.cow.cast()) };
            Some(VmoBacklink { cow, page, offset: out.offset })
        } else {
            None
        }
    }

    /// Returns just the reclaim queue counts. Called from the zx_object_get_info() syscall.
    pub fn get_reclaim_queue_counts(&self) -> ReclaimCounts {
        let mut counts = ReclaimCounts::default();

        // Grab the lock to prevent LRU processing, this lets us get a slightly less racy snapshot
        // of the queue counts, although we may still double count pages that move after we
        // count them. Specifically any parallel callers of MarkAccessed could move a page
        // and change the counts, causing us to either double count or miss count that page.
        // As these counts are not load bearing we accept the very small chance of
        // potentially being off a few pages.
        ksync::lock!(let _guard = self.lock_list_lock());
        let lru = self.lru_gen.load(Ordering::Relaxed);
        let mru = self.mru_gen.load(Ordering::Relaxed);

        counts.total = 0;
        for index in lru..=mru {
            let count = self.page_queue_counts[Self::gen_to_queue(index).0 as usize]
                .load(Ordering::Relaxed);
            // Distance to the MRU, and not the LRU, determines the bucket the count goes into. This
            // is to match the logic in PeekPagerBacked, which is also based on distance
            // to MRU.
            if index > mru - NUM_ACTIVE_QUEUES as u64 {
                counts.newest += count;
            } else if index <= mru - (NUM_RECLAIM as u64 - NUM_OLDEST_QUEUES as u64) {
                counts.oldest += count;
            }
            counts.total += count;
        }
        // Account the Isolate queue length under |oldest|, since (Isolate + oldest LRU) pages are
        // eligible for reclamation first. |oldest| is meant to track pages eligible for eviction
        // first.
        let inactive_count =
            self.page_queue_counts[PageQueue::RECLAIM_ISOLATE.0 as usize].load(Ordering::Relaxed);
        counts.oldest += inactive_count;
        counts.total += inactive_count;
        counts
    }

    /// Returns the length of all the queues. Lookups of the lengths are not synchronized and are
    /// only cohesive with respect to concurrent actions if the caller is otherwise guaranteeing
    /// that QueueOperations are not happening in parallel.
    pub fn queue_counts(&self) -> Counts {
        let mut counts = Counts::default();

        // Grab the lock to prevent LRU processing, this lets us get a slightly less racy snapshot
        // of the queue counts. We may still double count pages that move after we count
        // them.
        ksync::lock!(let _guard = self.lock_list_lock());
        let lru = self.lru_gen.load(Ordering::Relaxed);
        let mru = self.mru_gen.load(Ordering::Relaxed);

        for index in lru..=mru {
            counts.reclaim[(mru - index) as usize] = self.page_queue_counts
                [Self::gen_to_queue(index).0 as usize]
                .load(Ordering::Relaxed);
        }
        counts.reclaim_isolate =
            self.page_queue_counts[PageQueue::RECLAIM_ISOLATE.0 as usize].load(Ordering::Relaxed);
        counts.pager_backed_dirty = self.page_queue_counts
            [PageQueue::PAGER_BACKED_DIRTY.0 as usize]
            .load(Ordering::Relaxed);
        counts.anonymous =
            self.page_queue_counts[PageQueue::ANONYMOUS.0 as usize].load(Ordering::Relaxed);
        counts.wired = self.page_queue_counts[PageQueue::WIRED.0 as usize].load(Ordering::Relaxed);
        counts.anonymous_zero_fork = self.page_queue_counts
            [PageQueue::ANONYMOUS_ZERO_FORK.0 as usize]
            .load(Ordering::Relaxed);
        counts.failed_reclaim =
            self.page_queue_counts[PageQueue::FAILED_RECLAIM.0 as usize].load(Ordering::Relaxed);
        counts.high_priority =
            self.page_queue_counts[PageQueue::HIGH_PRIORITY.0 as usize].load(Ordering::Relaxed);
        counts
    }

    /// Retrieves the current number of active and inactive pages across the queues.
    pub fn get_active_inactive_counts(&self) -> ActiveInactiveCounts {
        let mut active_count: usize = 0;
        let mut inactive_count: usize = 0;
        let mru = self.mru_gen_to_queue();
        for queue in 0..PageQueue::NUM_QUEUES.0 {
            let count = self.page_queue_counts[queue as usize].load(Ordering::Relaxed);
            if Self::queue_is_active(PageQueue(queue), mru) {
                active_count += count;
            }
            if Self::queue_is_inactive(PageQueue(queue), mru) {
                inactive_count += count;
            }
        }
        ActiveInactiveCounts { active: active_count, inactive: inactive_count }
    }

    /// Dumps debug information about the page queues.
    pub fn dump(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_dump(self.as_raw()) }
    }

    /// Returns a global count of all pages compressed at the point of LRU change. This is a global
    /// method and will include stats from every `PageQueues` that has been instantiated.
    pub fn get_lru_pages_compressed() -> u64 {
        PQ_LRU_PAGES_COMPRESSED.sum_across_all_cpus() as u64
    }

    /// Enables reclamation of anonymous pages by causing them to be placed into the reclaimable
    /// queue instead of the dedicated anonymous queue. The |zero_forks| parameter controls
    /// whether the anonymous zero forks should also go into the general reclaimable queue or
    /// not. Any pages already placed into the anonymous queues will be moved over, and there is
    /// no way to disable this once enabled.
    pub fn enable_anonymous_reclaim(&self, zero_forks: bool) {
        {
            ksync::lock!(let mut guard = self.lock_list_lock());
            self.anonymous_is_reclaimable.store(true);
            self.zero_fork_is_reclaimable.store(zero_forks);

            let mru_queue = self.mru_gen_to_queue();

            // Migrate any existing pages into the reclaimable queues.
            while !guard.fields().page_queues[PageQueue::ANONYMOUS.0 as usize].is_empty() {
                let page = VmPagePtr::new(NonNull::from(
                    guard.fields().page_queues[PageQueue::ANONYMOUS.0 as usize].front().unwrap(),
                ));
                // SAFETY: `page` is at the front of `page_queues[ANONYMOUS]` under `list_lock`.
                unsafe { self.move_to_queue_locked_list(page, mru_queue, &mut guard) };
            }
            while zero_forks
                && !guard.fields().page_queues[PageQueue::ANONYMOUS_ZERO_FORK.0 as usize].is_empty()
            {
                let page = VmPagePtr::new(NonNull::from(
                    guard.fields().page_queues[PageQueue::ANONYMOUS_ZERO_FORK.0 as usize]
                        .front()
                        .unwrap(),
                ));
                // SAFETY: `page` is at the front of `page_queues[ANONYMOUS_ZERO_FORK]` under
                // `list_lock`.
                unsafe { self.move_to_queue_locked_list(page, mru_queue, &mut guard) };
            }
        }
        ksync::lock!(let mut guard = self.lock_lock());
        self.check_active_ratio_aging_locked(&mut guard);
    }

    /// Returns whether or not the reclaim queues only include pager backed pages or not.
    #[inline]
    pub fn reclaim_is_only_pager_backed(&self) -> bool {
        !self.anonymous_is_reclaimable.load()
    }

    /// Returns true if the page is in an isolate queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    #[inline]
    pub unsafe fn is_page_reclaimable(page: VmPagePtr) -> bool {
        // SAFETY: Caller guarantees `page` is attached to a VM object.
        let raw = unsafe { page.as_ref() };
        // SAFETY: `raw` is in the OBJECT state.
        unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed) == PageQueue::RECLAIM_ISOLATE.0
    }

    // These query functions are marked debug as it is generally a racy way to determine a pages
    // state and these are exposed for the purpose of writing tests or asserts against the
    // PageQueues.

    /// Checks if a page is in a reclaim queue.
    ///
    /// This returns an optional value that, if the page is in a reclaim queue, will contain the
    /// index of the queue that the page was in.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_reclaim(&self, page: VmPagePtr) -> Option<usize> {
        // SAFETY: validity of page asserted by caller.
        unsafe { self.debug_page_is_specific_reclaim(page, |_| true) }
    }

    /// Checks if a page is in the reclaim isolate queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_reclaim_isolate(&self, page: VmPagePtr) -> bool {
        // SAFETY: validity of page asserted by caller.
        unsafe {
            self.debug_page_is_specific_queue(page, PageQueue::RECLAIM_ISOLATE, |cow| {
                cow.can_evict()
            })
        }
    }

    /// Checks if a page is in the pager backed dirty queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_pager_backed_dirty(&self, page: VmPagePtr) -> bool {
        // SAFETY: Caller guarantees `page` is attached to a VM object.
        let raw = unsafe { page.as_ref() };
        // SAFETY: `raw` is in the OBJECT state.
        unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed)
            == PageQueue::PAGER_BACKED_DIRTY.0
    }

    /// Checks if a page is in the anonymous queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_anonymous(&self, page: VmPagePtr) -> bool {
        if self.reclaim_is_only_pager_backed() {
            // SAFETY: Caller guarantees `page` is attached to a VM object.
            let raw = unsafe { page.as_ref() };
            // SAFETY: `raw` is in the OBJECT state.
            return unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed)
                == PageQueue::ANONYMOUS.0;
        }
        // SAFETY: validity of page asserted by caller.
        unsafe { self.debug_page_is_specific_reclaim(page, |cow| !cow.can_evict()).is_some() }
    }

    /// Checks if a page is in the wired queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_wired(&self, page: VmPagePtr) -> bool {
        // SAFETY: Caller guarantees `page` is attached to a VM object.
        let raw = unsafe { page.as_ref() };
        // SAFETY: `raw` is in the OBJECT state.
        unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed) == PageQueue::WIRED.0
    }

    /// Checks if a page is in the high priority queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_high_priority(&self, page: VmPagePtr) -> bool {
        // SAFETY: Caller guarantees `page` is attached to a VM object.
        let raw = unsafe { page.as_ref() };
        // SAFETY: `raw` is in the OBJECT state.
        unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed) == PageQueue::HIGH_PRIORITY.0
    }

    /// Checks if a page is in the anonymous zero fork queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_anonymous_zero_fork(&self, page: VmPagePtr) -> bool {
        if self.reclaim_is_only_pager_backed() {
            // SAFETY: Caller guarantees `page` is attached to a VM object.
            let raw = unsafe { page.as_ref() };
            // SAFETY: `raw` is in the OBJECT state.
            return unsafe { raw.get_page_queue_ref() }.load(Ordering::Relaxed)
                == PageQueue::ANONYMOUS_ZERO_FORK.0;
        }
        // SAFETY: validity of page asserted by caller.
        unsafe { self.debug_page_is_specific_reclaim(page, |cow| !cow.can_evict()).is_some() }
    }

    /// Checks if a page is in any anonymous queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is owned by a VMO and assigned to this PageQueue.
    pub unsafe fn debug_page_is_any_anonymous(&self, page: VmPagePtr) -> bool {
        // SAFETY: Caller guarantees `page` is owned by a VMO and assigned to this PageQueue.
        unsafe {
            self.debug_page_is_anonymous(page) || self.debug_page_is_anonymous_zero_fork(page)
        }
    }

    // These methods are public so that the scanner can call. Once the scanner is an object that can
    // be friended, and not a collection of anonymous functions, these can be made private.

    /// Creates any threads for queue management. This needs to be done separately to construction
    /// as there is a recursive dependency where creating threads will need to manipulate pages,
    /// which will call back into the page queues.
    /// Delaying thread creation is fine as these threads are purely for aging and eviction
    /// management, which is not needed during early kernel boot.
    /// Failure to start the threads may cause operations such as RotatePagerBackedQueues to block
    /// indefinitely as they might attempt to offload work to a nonexistent thread. This issue is
    /// only relevant for unittests that may wish to avoid starting the threads for some tests.
    /// It is the responsibility of the caller to only call this once, otherwise it will panic.
    pub fn start_threads(
        &self,
        min_mru_rotate_time: DurationMono,
        max_mru_rotate_time: DurationMono,
    ) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe {
            bindings::cpp_page_queues_start_threads(
                self.as_raw(),
                min_mru_rotate_time.into_nanos(),
                max_mru_rotate_time.into_nanos(),
            )
        }
    }

    /// Initializes and starts the debug compression, which attempts to immediately compress a
    /// random subset of pages added to the page queues. It is an error to call this if there is
    /// no compressor or if not running in debug mode.
    pub fn start_debug_compressor(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_start_debug_compressor(self.as_raw()) }
    }

    /// Sets the active ratio multiplier.
    pub fn set_active_ratio_multiplier(&self, multiplier: u32) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_set_active_ratio_multiplier(self.as_raw(), multiplier) }
    }

    /// Sets the LRU action.
    pub fn set_lru_action(&self, action: LruAction) {
        ksync::lock!(let mut guard = self.lock_lock());
        *guard.as_mut().fields_mut().lru_action = action;
    }

    /// Disables the active aging system.
    /// Controls to enable and disable the active aging system. These must be called alternately and
    /// not in parallel. That is, it is an error to call DisableAging twice without calling
    /// EnableAging in between. Similar for EnableAging.
    pub fn disable_aging(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_disable_aging(self.as_raw()) }
    }

    /// Enables the active aging system.
    /// Controls to enable and disable the active aging system. These must be called alternately and
    /// not in parallel. That is, it is an error to call DisableAging twice without calling
    /// EnableAging in between. Similar for EnableAging.
    pub fn enable_aging(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_enable_aging(self.as_raw()) }
    }

    /// Register an Event that will be signalled every time aging occurs. This can be used to know
    /// if if peek_reclaim might now return items (due to aging having occurred) where it had
    /// previously ceased.
    /// Only a single Event may be registered at a time and the Event is assumed to live as long as
    /// the PageQueues object. A None can be passed in to unregister an Event, otherwise it is
    /// an error to attempt to register over the top of an existing event.
    ///
    /// # Safety
    ///
    /// If an `Event` is provided, it is assumed to live as long as the `PageQueues` object.
    pub unsafe fn set_aging_event(&self, event: Option<NonNull<Event>>) {
        let event_ptr = event.map_or(core::ptr::null_mut(), |e| e.as_ptr()) as *mut bindings::Event;
        // SAFETY: `self.as_raw()` is valid, and caller guarantees event lifetime.
        unsafe { bindings::cpp_page_queues_set_aging_event(self.as_raw(), event_ptr) }
    }

    // Debug method to retrieve a reference to any lru thread. Intended for use
    // during tests / debugging and hence bypass the lock normally needed to read this member. It is
    // up to the caller to know if these objects are alive or not.
    pub fn debug_get_lru_thread(&self) -> Option<ThreadPtr> {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        let raw = unsafe { bindings::cpp_page_queues_debug_get_lru_thread(self.as_raw()) };
        // SAFETY: If non-null, `raw` points to a live kernel thread.
        unsafe { ThreadPtr::from_raw(raw.cast()) }
    }

    // Debug method to retrieve a reference to any mru thread. Intended for use
    // during tests / debugging and hence bypass the lock normally needed to read this member. It is
    // up to the caller to know if these objects are alive or not.
    pub fn debug_get_mru_thread(&self) -> Option<ThreadPtr> {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        let raw = unsafe { bindings::cpp_page_queues_debug_get_mru_thread(self.as_raw()) };
        // SAFETY: If non-null, `raw` points to a live kernel thread.
        unsafe { ThreadPtr::from_raw(raw.cast()) }
    }
}

/// Converts a raw pointer and count into a slice, returning an empty slice when `count == 0`.
///
/// # Safety
///
/// `ptr` must be non-null. If `count > 0`, `ptr` must be properly aligned and valid for reads of
/// `count` elements of type `T` for lifetime `'a`.
#[inline]
unsafe fn slice_from_raw_parts_or_empty<'a, T>(ptr: *const T, count: usize) -> &'a [T] {
    debug_assert!(!ptr.is_null());
    if count == 0 {
        &[]
    } else {
        // SAFETY: Caller guarantees `ptr` is valid for `count` elements when `count > 0`.
        unsafe { core::slice::from_raw_parts(ptr, count) }
    }
}

/// FFI wrapper for [`PageQueues::set_wired`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::set_wired`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_wired(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid pointers upholding `set_wired`
    // preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.set_wired(page_ptr, cow, page_offset);
    }
}

/// FFI wrapper for [`PageQueues::set_anonymous`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::set_anonymous`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_anonymous(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
    skip_reclaim: bool,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid pointers upholding `set_anonymous`
    // preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.set_anonymous(page_ptr, cow, page_offset, skip_reclaim);
    }
}

/// FFI wrapper for [`PageQueues::set_reclaim`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::set_reclaim`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_reclaim(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid pointers upholding `set_reclaim`
    // preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.set_reclaim(page_ptr, cow, page_offset);
    }
}

/// FFI wrapper for [`PageQueues::set_pager_backed_dirty`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::set_pager_backed_dirty`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_pager_backed_dirty(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid pointers upholding
    // `set_pager_backed_dirty` preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.set_pager_backed_dirty(page_ptr, cow, page_offset);
    }
}

/// FFI wrapper for [`PageQueues::set_anonymous_zero_fork`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::set_anonymous_zero_fork`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_anonymous_zero_fork(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid pointers upholding
    // `set_anonymous_zero_fork` preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.set_anonymous_zero_fork(page_ptr, cow, page_offset);
    }
}

/// FFI wrapper for [`PageQueues::set_high_priority`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::set_high_priority`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_high_priority(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid pointers upholding
    // `set_high_priority` preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.set_high_priority(page_ptr, cow, page_offset);
    }
}

/// FFI wrapper for [`PageQueues::move_to_wired`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of [`PageQueues::move_to_wired`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_to_wired(queues: &PageQueues, page: *mut vm_page_t) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_to_wired` preconditions.
    unsafe { queues.move_to_wired(page_ptr) };
}

/// FFI wrapper for [`PageQueues::move_to_anonymous`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of [`PageQueues::move_to_anonymous`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_to_anonymous(
    queues: &PageQueues,
    page: *mut vm_page_t,
    skip_reclaim: bool,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_to_anonymous` preconditions.
    unsafe { queues.move_to_anonymous(page_ptr, skip_reclaim) };
}

/// FFI wrapper for [`PageQueues::move_to_reclaim`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of [`PageQueues::move_to_reclaim`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_to_reclaim(
    queues: &PageQueues,
    page: *mut vm_page_t,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_to_reclaim` preconditions.
    unsafe { queues.move_to_reclaim(page_ptr) };
}

/// FFI wrapper for [`PageQueues::move_to_reclaim_dont_need`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of
/// [`PageQueues::move_to_reclaim_dont_need`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_to_reclaim_dont_need(
    queues: &PageQueues,
    page: *mut vm_page_t,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_to_reclaim_dont_need` preconditions.
    unsafe { queues.move_to_reclaim_dont_need(page_ptr) };
}

/// FFI wrapper for [`PageQueues::move_to_pager_backed_dirty`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of
/// [`PageQueues::move_to_pager_backed_dirty`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_to_pager_backed_dirty(
    queues: &PageQueues,
    page: *mut vm_page_t,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_to_pager_backed_dirty` preconditions.
    unsafe { queues.move_to_pager_backed_dirty(page_ptr) };
}

/// FFI wrapper for [`PageQueues::move_to_high_priority`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of
/// [`PageQueues::move_to_high_priority`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_to_high_priority(
    queues: &PageQueues,
    page: *mut vm_page_t,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_to_high_priority` preconditions.
    unsafe { queues.move_to_high_priority(page_ptr) };
}

/// FFI wrapper for [`PageQueues::move_anonymous_to_anonymous_zero_fork`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of
/// [`PageQueues::move_anonymous_to_anonymous_zero_fork`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_move_anonymous_to_anonymous_zero_fork(
    queues: &PageQueues,
    page: *mut vm_page_t,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `move_anonymous_to_anonymous_zero_fork` preconditions.
    unsafe { queues.move_anonymous_to_anonymous_zero_fork(page_ptr) };
}

/// FFI wrapper for [`PageQueues::compress_failed`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of [`PageQueues::compress_failed`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_compress_failed(
    queues: &PageQueues,
    page: *mut vm_page_t,
) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `compress_failed` preconditions.
    unsafe { queues.compress_failed(page_ptr) };
}

/// FFI wrapper for [`PageQueues::change_object_offset`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::change_object_offset`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_change_object_offset(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid non-null pointers upholding
    // `change_object_offset` preconditions.
    unsafe {
        let cow = &*(object as *const VmCowPages);
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.change_object_offset(page_ptr, cow, page_offset);
    }
}

/// FFI wrapper for [`PageQueues::change_object_offset_array`].
///
/// # Safety
///
/// `pages` and `offsets` must be valid for `count` elements and `cow` must be a valid
/// `VmCowPages` pointer meeting the preconditions of [`PageQueues::change_object_offset_array`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_change_object_offset_array(
    queues: &PageQueues,
    pages: *mut *mut vm_page_t,
    cow: *mut bindings::VmCowPages,
    offsets: *const u64,
    count: usize,
) {
    debug_assert!(!cow.is_null());
    // SAFETY: `VmPagePtr` is `#[repr(transparent)]` over `NonNull<VmPage>`, and caller
    // guarantees `cow`, `pages`, and `offsets` are valid non-null pointers upholding
    // `change_object_offset_array` preconditions.
    unsafe {
        let cow = cow as *const VmCowPages;
        debug_assert!(slice_from_raw_parts_or_empty(pages, count).iter().all(|x| !x.is_null()));
        let pages_slice = slice_from_raw_parts_or_empty(pages as *const VmPagePtr, count);
        let offsets_slice = slice_from_raw_parts_or_empty(offsets, count);
        queues.change_object_offset_array(pages_slice, &*cow, offsets_slice);
    }
}

/// FFI wrapper for [`PageQueues::change_object_offset_locked_list`].
///
/// # Safety
///
/// `page` and `object` must be valid pointers meeting the preconditions of
/// [`PageQueues::change_object_offset_locked_list`], and the page queues lock (`get_lock()`) must
/// be held by the caller.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_change_object_offset_locked_list(
    queues: &PageQueues,
    page: *mut vm_page_t,
    object: *mut bindings::VmCowPages,
    page_offset: u64,
) {
    debug_assert!(!object.is_null());
    // SAFETY: Caller guarantees `page` and `object` are valid non-null pointers, `list_lock`
    // (`get_lock()`) is held, and `change_object_offset_locked_list` preconditions are met.
    unsafe {
        let cow = object as *const VmCowPages;
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        let token = LockToken::new();
        queues.change_object_offset_locked_list(page_ptr, &*cow, page_offset, &token);
    }
}

/// FFI wrapper for [`PageQueues::remove`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of [`PageQueues::remove`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_remove(queues: &PageQueues, page: *mut vm_page_t) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer upholding `remove`
    // preconditions.
    unsafe {
        let page_ptr = VmPagePtr::from_ffi_unchecked(page);
        queues.remove(page_ptr);
    }
}

/// FFI wrapper for [`PageQueues::remove_array_into_list`].
///
/// # Safety
///
/// `pages` must be valid for `count` elements and `out_list` must be a valid
/// `VmPageDoublyLinkedList` pointer meeting the preconditions of
/// [`PageQueues::remove_array_into_list`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_remove_array_into_list(
    queues: &PageQueues,
    pages: *mut *mut vm_page_t,
    count: usize,
    out_list: *mut VmPageDoublyLinkedList,
) {
    debug_assert!(!out_list.is_null());
    // SAFETY: `VmPagePtr` is `#[repr(transparent)]` over `NonNull<VmPage>`, and caller
    // guarantees `pages` and `out_list` are valid non-null pointers upholding
    // `remove_array_into_list` preconditions.
    unsafe {
        debug_assert!(slice_from_raw_parts_or_empty(pages, count).iter().all(|x| !x.is_null()));
        let pages_slice = slice_from_raw_parts_or_empty(pages as *const VmPagePtr, count);
        let list_ref = Pin::new_unchecked(&mut *out_list);
        queues.remove_array_into_list(pages_slice, list_ref);
    }
}
/// FFI wrapper for [`PageQueues::mark_accessed`].
///
/// # Safety
///
/// `page` must be a valid pointer meeting the preconditions of [`PageQueues::mark_accessed`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_mark_accessed(queues: &PageQueues, page: *mut vm_page_t) {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page) };
    // SAFETY: Caller upholds `mark_accessed` preconditions.
    unsafe { queues.mark_accessed(page_ptr) };
}

/// FFI wrapper for [`PageQueues::get_reclaim_queue_counts`].
///
/// # Safety
///
/// `out_counts` must point to a valid writable `ReclaimCounts`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_get_reclaim_queue_counts(
    queues: &PageQueues,
    out_counts: *mut c_void,
) {
    let counts = queues.get_reclaim_queue_counts();
    // SAFETY: Caller guarantees `out_counts` points to a valid `ReclaimCounts`.
    unsafe {
        *(out_counts as *mut ReclaimCounts) = counts;
    }
}

/// FFI wrapper for [`PageQueues::queue_counts`].
///
/// # Safety
///
/// `out_counts` must point to a valid writable `Counts`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_queue_counts(
    queues: &PageQueues,
    out_counts: *mut c_void,
) {
    let counts = queues.queue_counts();
    // SAFETY: Caller guarantees `out_counts` points to a valid `Counts`.
    unsafe {
        *(out_counts as *mut Counts) = counts;
    }
}

/// FFI wrapper for [`PageQueues::get_active_inactive_counts`].
///
/// # Safety
///
/// `out_counts` must point to a valid writable `ActiveInactiveCounts`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_get_active_inactive_counts(
    queues: &PageQueues,
    out_counts: *mut c_void,
) {
    let counts = queues.get_active_inactive_counts();
    // SAFETY: Caller guarantees `out_counts` points to a valid `ActiveInactiveCounts`.
    unsafe {
        *(out_counts as *mut ActiveInactiveCounts) = counts;
    }
}

/// FFI wrapper for [`PageQueues::get_lru_pages_compressed`].
///
/// # Safety
///
/// The caller must ensure kernel counters have been initialized.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_get_lru_pages_compressed() -> u64 {
    PageQueues::get_lru_pages_compressed()
}

/// FFI wrapper for [`PageQueues::enable_anonymous_reclaim`].
///
/// # Safety
///
/// `queues` must be a valid reference to an initialized `PageQueues`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_enable_anonymous_reclaim(
    queues: &PageQueues,
    zero_forks: bool,
) {
    queues.enable_anonymous_reclaim(zero_forks);
}

/// FFI wrapper for [`PageQueues::reclaim_is_only_pager_backed`].
///
/// # Safety
///
/// `queues` must be a valid reference to an initialized `PageQueues`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_reclaim_is_only_pager_backed(
    queues: &PageQueues,
) -> bool {
    queues.reclaim_is_only_pager_backed()
}

/// FFI wrapper for [`PageQueues::is_page_reclaimable`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer meeting the preconditions of
/// [`PageQueues::is_page_reclaimable`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_is_page_reclaimable(page: *const vm_page_t) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `is_page_reclaimable` preconditions.
    unsafe { PageQueues::is_page_reclaimable(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_reclaim`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer, and `out_queue` (if non-null) must be a valid
/// writable `usize` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_reclaim(
    queues: &PageQueues,
    page: *const vm_page_t,
    out_queue: *mut usize,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_reclaim` preconditions.
    let q_opt = unsafe { queues.debug_page_is_reclaim(page_ptr) };
    if let Some(q) = q_opt {
        if !out_queue.is_null() {
            // SAFETY: Caller guarantees `out_queue` is valid when non-null.
            unsafe { *out_queue = q };
        }
        true
    } else {
        false
    }
}

/// FFI wrapper for [`PageQueues::debug_page_is_reclaim_isolate`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_reclaim_isolate(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_reclaim_isolate` preconditions.
    unsafe { queues.debug_page_is_reclaim_isolate(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_pager_backed_dirty`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_pager_backed_dirty(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_pager_backed_dirty` preconditions.
    unsafe { queues.debug_page_is_pager_backed_dirty(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_anonymous`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_anonymous(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_anonymous` preconditions.
    unsafe { queues.debug_page_is_anonymous(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_anonymous_zero_fork`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_anonymous_zero_fork(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_anonymous_zero_fork` preconditions.
    unsafe { queues.debug_page_is_anonymous_zero_fork(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_any_anonymous`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_any_anonymous(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_any_anonymous` preconditions.
    unsafe { queues.debug_page_is_any_anonymous(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_wired`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_wired(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_wired` preconditions.
    unsafe { queues.debug_page_is_wired(page_ptr) }
}

/// FFI wrapper for [`PageQueues::debug_page_is_high_priority`].
///
/// # Safety
///
/// `page` must be a valid `vm_page_t` pointer.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_debug_page_is_high_priority(
    queues: &PageQueues,
    page: *const vm_page_t,
) -> bool {
    // SAFETY: Caller guarantees `page` is a valid `vm_page_t` pointer.
    let page_ptr = unsafe { VmPagePtr::from_ffi_unchecked(page.cast_mut()) };
    // SAFETY: Caller upholds `debug_page_is_high_priority` preconditions.
    unsafe { queues.debug_page_is_high_priority(page_ptr) }
}

/// FFI wrapper for [`PageQueues::set_lru_action`].
///
/// # Safety
///
/// The caller must ensure `queues` is a valid reference to an initialized `PageQueues`, `action`
/// is a valid `LruAction` discriminant, and the page queues lock is not held by the caller.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_set_lru_action(queues: &PageQueues, action: i32) {
    queues.set_lru_action(LruAction::from_raw(action));
}

// TODO(https://fxbug.dev/549881466): Temporary export for the C++ RotateReclaimQueues, LruThread
// and PeekIsolate, to be removed once those are ported to Rust.
/// FFI wrapper for [`PageQueues::process_lru_queue`]. An `isolate` of `usize::MAX` means no limit.
///
/// # Safety
///
/// The caller must ensure `queues` is a valid reference to an initialized `PageQueues`, and no
/// spinlocks or VMO locks are held by the caller.
// SAFETY: Symbol name is prefixed and unique across the kernel link unit.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_page_queues_process_lru_queue(
    queues: &PageQueues,
    target_gen: u64,
    isolate: usize,
) {
    queues.process_lru_queue(target_gen, (isolate != usize::MAX).then_some(isolate));
}

#[cfg(ktest)]
#[unittest::suite(name = "page_queues_rust")]
/// PageQueues unit tests.
mod tests {
    use super::{ActiveInactiveCounts, Counts, NUM_RECLAIM, PageQueues};
    use crate::platform_rs::timer::DurationMono;
    use crate::vm::page::{VmPage, VmPageObjectState, VmPagePtr, VmPageUnion};
    use crate::vm::page_state::VmPageState;
    use crate::vm::page_state::bindings::vm_page_state;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use crate::vm_unittests::test_helper::make_uncommitted_pager_vmo;
    use fbl::DoublyLinkedListContainable;
    use page::SIZE as PAGE_SIZE_USIZE;
    use unittest::{assert_true, expect_eq, expect_false, expect_true, unwrap_ok, unwrap_some};
    use zx_types::ZX_TIME_INFINITE;

    const PAGE_SIZE: u64 = PAGE_SIZE_USIZE as u64;

    fn initialize_test_page(page: &mut VmPage) -> VmPagePtr {
        debug_assert!(!page.get_node().in_container());
        // Pages are constructed in the FREE state
        debug_assert!(page.is_free());
        // SAFETY: We hold a mutable reference to page and so have exclusive ownership.
        unsafe {
            page.set_state(VmPageState(vm_page_state::OBJECT));
            *page.state_union.get() =
                VmPageUnion { object: core::mem::ManuallyDrop::new(VmPageObjectState::default()) };
            VmPagePtr::from_raw(core::ptr::from_ref(page).cast_mut()).unwrap()
        }
    }

    /// Tests adding and removing pages from page queues.
    #[test]
    fn pq_add_remove() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        // Pretend we have an allocated page
        let mut test_page = VmPage::default();
        let test_page_ptr = initialize_test_page(&mut test_page);

        // Need a VMO to claim our pages are in
        let vmo = unwrap_ok!(VmObjectPaged::create(0, 0, PAGE_SIZE));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Put the page in each queue and make sure it shows up
        // SAFETY: `test_page_ptr` is a valid test page and `cow` is valid.
        unsafe { pq.set_wired(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { wired: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in the wired queue.
        unsafe { pq.remove(test_page_ptr) };
        // SAFETY: `test_page_ptr` is an initialized test page in OBJECT state.
        expect_false!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        // SAFETY: `test_page_ptr` is an initialized test page in OBJECT state.
        expect_false!(unsafe { pq.debug_page_is_anonymous(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts::default());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_anonymous(test_page_ptr, &cow, 0, false) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_anonymous(test_page_ptr) });
        if pq.reclaim_is_only_pager_backed() {
            expect_true!(pq.queue_counts() == Counts { anonymous: 1, ..Default::default() });
        } else {
            let mut reclaim = [0; NUM_RECLAIM];
            reclaim[0] = 1;
            expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });
        }

        // SAFETY: `test_page_ptr` is in an anonymous queue.
        unsafe { pq.remove(test_page_ptr) };
        // SAFETY: `test_page_ptr` is an initialized test page in OBJECT state.
        expect_false!(unsafe { pq.debug_page_is_anonymous(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts::default());

        // Need a pager VMO to claim our page is in.
        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(1, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 1;
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });

        // SAFETY: `test_page_ptr` is in the reclaim queue.
        unsafe { pq.remove(test_page_ptr) };
        // SAFETY: `test_page_ptr` is an initialized test page in OBJECT state.
        expect_false!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        expect_true!(pq.queue_counts() == Counts::default());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_pager_backed_dirty(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { pager_backed_dirty: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in the pager backed dirty queue.
        unsafe { pq.remove(test_page_ptr) };
        // SAFETY: `test_page_ptr` is an initialized test page in OBJECT state.
        expect_false!(unsafe { pq.debug_page_is_pager_backed_dirty(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts::default());
    }

    /// Tests moving pages between different page queues.
    #[test]
    fn pq_move_queues() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        // Pretend we have an allocated page
        let mut test_page = VmPage::default();
        let test_page_ptr = initialize_test_page(&mut test_page);

        // Need a VMO to claim our pages are in
        let vmo = unwrap_ok!(VmObjectPaged::create(0, 0, PAGE_SIZE));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Move the page between queues.
        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_wired(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { wired: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_anonymous(test_page_ptr, false) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_false!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_anonymous(test_page_ptr) });
        if pq.reclaim_is_only_pager_backed() {
            expect_true!(pq.queue_counts() == Counts { anonymous: 1, ..Default::default() });
        } else {
            let mut reclaim = [0; NUM_RECLAIM];
            reclaim[0] = 1;
            expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });
        }
        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.remove(test_page_ptr) };

        // Now try some pager backed queues.
        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(1, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 1;
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_pager_backed_dirty(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { pager_backed_dirty: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_reclaim(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_reclaim_dont_need(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_false!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { reclaim_isolate: 1, ..Default::default() });

        // Verify that the DontNeed page is first in line for eviction.
        let backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        expect_true!(backlink.as_ref().is_some_and(|b| b.page == test_page_ptr));

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_wired(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_false!(unsafe { pq.debug_page_is_reclaim_isolate(test_page_ptr) });
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_false!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { wired: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in the wired queue.
        unsafe { pq.remove(test_page_ptr) };
        expect_true!(pq.queue_counts() == Counts::default());
    }

    /// Tests moving a page to the queue it is already in.
    #[test]
    fn pq_move_self_queue() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        // Pretend we have an allocated page
        let mut test_page = VmPage::default();
        let test_page_ptr = initialize_test_page(&mut test_page);

        // Need a VMO to claim our pages are in
        let vmo = unwrap_ok!(VmObjectPaged::create(0, 0, PAGE_SIZE));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Move the page into the queue it is already in.
        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_wired(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { wired: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_wired(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { wired: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in the wired queue.
        unsafe { pq.remove(test_page_ptr) };
        expect_true!(pq.queue_counts() == Counts::default());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_anonymous(test_page_ptr, &cow, 0, false) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_anonymous(test_page_ptr) });
        if pq.reclaim_is_only_pager_backed() {
            expect_true!(pq.queue_counts() == Counts { anonymous: 1, ..Default::default() });
        } else {
            let mut reclaim = [0; NUM_RECLAIM];
            reclaim[0] = 1;
            expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });
        }

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_anonymous(test_page_ptr, false) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_anonymous(test_page_ptr) });
        if pq.reclaim_is_only_pager_backed() {
            expect_true!(pq.queue_counts() == Counts { anonymous: 1, ..Default::default() });
        } else {
            let mut reclaim = [0; NUM_RECLAIM];
            reclaim[0] = 1;
            expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });
        }

        // SAFETY: `test_page_ptr` is in an anonymous queue.
        unsafe { pq.remove(test_page_ptr) };
        expect_true!(pq.queue_counts() == Counts::default());

        // Now try some pager backed queues.
        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(1, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 1;
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_reclaim(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(test_page_ptr) }.is_some());
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.remove(test_page_ptr) };
        expect_true!(pq.queue_counts() == Counts::default());

        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_pager_backed_dirty(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { pager_backed_dirty: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_pager_backed_dirty(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(test_page_ptr) });
        expect_true!(pq.queue_counts() == Counts { pager_backed_dirty: 1, ..Default::default() });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.remove(test_page_ptr) };
        expect_true!(pq.queue_counts() == Counts::default());
    }

    /// Tests gradual rotation and aging of reclaim queues.
    #[test]
    fn pq_rotate_queue() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        pq.set_active_ratio_multiplier(0);
        pq.start_threads(DurationMono::from_nanos(0), DurationMono::from_nanos(ZX_TIME_INFINITE));

        // Pretend we have a few allocated pages.
        let mut wired_page = VmPage::default();
        let wired_page_ptr = initialize_test_page(&mut wired_page);
        let mut clean_pager_page = VmPage::default();
        let clean_pager_page_ptr = initialize_test_page(&mut clean_pager_page);
        let mut dirty_pager_page = VmPage::default();
        let dirty_pager_page_ptr = initialize_test_page(&mut dirty_pager_page);

        // Need a VMO to claim our pages are in.
        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(1, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Put the pages in and validate initial state.
        // SAFETY: test pages are valid and not in a queue.
        unsafe {
            pq.set_wired(wired_page_ptr, &cow, 0);
            pq.set_reclaim(clean_pager_page_ptr, &cow, 0);
            pq.set_pager_backed_dirty(dirty_pager_page_ptr, &cow, 0);
        }
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(wired_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(dirty_pager_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        let queue = unsafe { pq.debug_page_is_reclaim(clean_pager_page_ptr) };
        expect_true!(queue.is_some());
        expect_eq!(queue.unwrap(), 0);
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 1, inactive: 0 }
        );

        // Gradually rotate the queue.
        pq.rotate_reclaim_queues();
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(wired_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(dirty_pager_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        let queue = unsafe { pq.debug_page_is_reclaim(clean_pager_page_ptr) };
        expect_true!(queue.is_some());
        expect_eq!(queue.unwrap(), 1);
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[1] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 1, inactive: 0 }
        );

        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[2] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 1 }
        );

        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[3] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 1 }
        );

        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[4] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );

        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[5] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );

        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[6] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );

        pq.rotate_reclaim_queues();
        // Further rotations might cause the page to be visible in the same queue, or the isolate,
        // depending on whether the lru processing already ran in preparation of the next aging
        // event.
        let mut counts_last = Counts { pager_backed_dirty: 1, wired: 1, ..Default::default() };
        counts_last.reclaim[NUM_RECLAIM - 1] = 1;
        let counts_isolate =
            Counts { reclaim_isolate: 1, pager_backed_dirty: 1, wired: 1, ..Default::default() };
        let counts = pq.queue_counts();
        expect_true!(counts == counts_last || counts == counts_isolate);

        // Further rotations should not move the page.
        pq.rotate_reclaim_queues();
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(wired_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(dirty_pager_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(clean_pager_page_ptr) });
        let counts = pq.queue_counts();
        expect_true!(counts == counts_isolate);
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 1 }
        );

        // Moving the page should bring it back to the first queue.
        // SAFETY: `clean_pager_page_ptr` is in a page queue.
        unsafe { pq.move_to_reclaim(clean_pager_page_ptr) };
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_wired(wired_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_pager_backed_dirty(dirty_pager_page_ptr) });
        // SAFETY: test pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(clean_pager_page_ptr) }.is_some());
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 1, inactive: 0 }
        );

        // Just double check two rotations.
        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[1] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 1, inactive: 0 }
        );
        pq.rotate_reclaim_queues();
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[2] = 1;
        expect_true!(
            pq.queue_counts()
                == Counts { reclaim, pager_backed_dirty: 1, wired: 1, ..Default::default() }
        );
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 1 }
        );

        // SAFETY: pages are in page queues.
        unsafe {
            pq.remove(wired_page_ptr);
            pq.remove(clean_pager_page_ptr);
            pq.remove(dirty_pager_page_ptr);
        }
    }

    /// Tests moving pages to the don't need queue and access reactivation.
    #[test]
    fn pq_toggle_dont_need_queue() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        pq.set_active_ratio_multiplier(0);
        pq.start_threads(DurationMono::from_nanos(0), DurationMono::from_nanos(ZX_TIME_INFINITE));

        // Pretend we have a couple of allocated pager-backed pages.
        let mut page1 = VmPage::default();
        let page1_ptr = initialize_test_page(&mut page1);
        let mut page2 = VmPage::default();
        let page2_ptr = initialize_test_page(&mut page2);

        // Need a VMO to claim our pager backed pages are in.
        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(2, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Put the pages in and validate initial state.
        // SAFETY: `page1_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(page1_ptr, &cow, 0) };
        // SAFETY: `page1_ptr` is attached to a VM object.
        let queue = unsafe { pq.debug_page_is_reclaim(page1_ptr) };
        expect_true!(queue.is_some());
        expect_eq!(queue.unwrap(), 0);
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 1;
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 1, inactive: 0 }
        );
        // SAFETY: `page2_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(page2_ptr, &cow, 0) };
        // SAFETY: `page2_ptr` is attached to a VM object.
        let queue = unsafe { pq.debug_page_is_reclaim(page2_ptr) };
        expect_true!(queue.is_some());
        expect_eq!(queue.unwrap(), 0);
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[0] = 2;
        expect_true!(pq.queue_counts() == Counts { reclaim, ..Default::default() });
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 2, inactive: 0 }
        );

        // Move the pages to the DontNeed queue.
        // SAFETY: pages are in a page queue.
        unsafe {
            pq.move_to_reclaim_dont_need(page1_ptr);
            pq.move_to_reclaim_dont_need(page2_ptr);
        }
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(page1_ptr) });
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(page2_ptr) });
        expect_true!(pq.queue_counts() == Counts { reclaim_isolate: 2, ..Default::default() });
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 2 }
        );

        // Rotate the queues. This should also process the DontNeed queue.
        pq.rotate_reclaim_queues();
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(page1_ptr) });
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(page2_ptr) });
        expect_true!(pq.queue_counts() == Counts { reclaim_isolate: 2, ..Default::default() });
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 2 }
        );

        // Simulate access for one of the pages. Then rotate the queues again. This should move the
        // accessed page1 out of the DontNeed queue to MRU+1 (as we've rotated the queues after
        // access). SAFETY: pages are in a paqe queue
        unsafe {
            pq.mark_accessed(page1_ptr);
        }
        pq.rotate_reclaim_queues();
        // SAFETY: pages are attached to a VM object.
        let queue = unsafe { pq.debug_page_is_reclaim(page1_ptr) };
        expect_true!(queue.is_some());
        expect_eq!(queue.unwrap(), 1);
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(page2_ptr) });
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[1] = 1;
        expect_true!(
            pq.queue_counts() == Counts { reclaim, reclaim_isolate: 1, ..Default::default() }
        );
        // Two active queues by default, so page1 is still considered active.
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 1, inactive: 1 }
        );

        // Rotate the queues again. The page accessed above should move to the next pager-backed
        // queue.
        pq.rotate_reclaim_queues();
        // SAFETY: pages are attached to a VM object.
        let queue = unsafe { pq.debug_page_is_reclaim(page1_ptr) };
        expect_true!(queue.is_some());
        expect_eq!(queue.unwrap(), 2);
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(page2_ptr) });
        let mut reclaim = [0; NUM_RECLAIM];
        reclaim[2] = 1;
        expect_true!(
            pq.queue_counts() == Counts { reclaim, reclaim_isolate: 1, ..Default::default() }
        );
        // page1 has now moved on past the two active queues, so it now counts as inactive.
        expect_true!(
            pq.get_active_inactive_counts() == ActiveInactiveCounts { active: 0, inactive: 2 }
        );

        // SAFETY: pages are in page queues.
        unsafe {
            pq.remove(page1_ptr);
            pq.remove(page2_ptr);
        }
    }

    /// Tests FIFO ordering of pages aged in the same generation.
    #[test]
    fn pq_single_queue_fifo_order() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        let mut old_page = VmPage::default();
        let old_page_ptr = initialize_test_page(&mut old_page);
        let mut new_page = VmPage::default();
        let new_page_ptr = initialize_test_page(&mut new_page);

        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(2, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // SAFETY: pages are valid and not in a queue.
        unsafe {
            pq.set_reclaim(old_page_ptr, &cow, 0);
            pq.set_reclaim(new_page_ptr, &cow, 1);
        }

        // SAFETY: pages are attached to a VM object.
        let (queue_a, queue_b) = unsafe {
            (pq.debug_page_is_reclaim(old_page_ptr), pq.debug_page_is_reclaim(new_page_ptr))
        };
        expect_true!(queue_a.is_some());
        expect_true!(queue_b.is_some());
        expect_eq!(queue_a.unwrap(), queue_b.unwrap()); // Verify they are in the same queue (same generation)

        // Age pages until they reach the LRU queue.
        for _ in 0..NUM_RECLAIM - 1 {
            pq.rotate_reclaim_queues();
        }

        // Peek the isolate list using the public peek_isolate method.
        // We expect pages to be processed in age order (oldest first, i.e., old_page before
        // new_page). Since they are added to the tail of the isolate list, the oldest page
        // (old_page) should be at the head of the isolate list.
        let backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(backlink.is_some());
        expect_eq!(backlink.as_ref().unwrap().page.as_raw(), old_page_ptr.as_raw());

        // SAFETY: `old_page_ptr` is in a page queue.
        unsafe { pq.remove(old_page_ptr) };
        let next_backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(next_backlink.is_some());
        expect_eq!(next_backlink.as_ref().unwrap().page.as_raw(), new_page_ptr.as_raw());

        // SAFETY: `new_page_ptr` is in a page queue.
        unsafe { pq.remove(new_page_ptr) };
    }

    /// Tests FIFO ordering of pages aged across multiple generations.
    #[test]
    fn pq_multiple_queues_fifo_order() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        pq.set_active_ratio_multiplier(0);
        pq.start_threads(DurationMono::from_nanos(0), DurationMono::from_nanos(ZX_TIME_INFINITE));

        let mut old_page = VmPage::default();
        let old_page_ptr = initialize_test_page(&mut old_page);
        let mut new_page = VmPage::default();
        let new_page_ptr = initialize_test_page(&mut new_page);

        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(2, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Set up old_page and rotate once to push it to the next queue.
        // SAFETY: `old_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(old_page_ptr, &cow, 0) };
        // SAFETY: `old_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(old_page_ptr) }.is_some());
        pq.rotate_reclaim_queues();

        // Set up new_page.
        // SAFETY: `new_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(new_page_ptr, &cow, 1) };
        // SAFETY: `new_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(new_page_ptr) }.is_some());

        // Rotate queues until both pages reach the isolate queue. Since new_page is one
        // generation behind old_page, waiting for new_page ensures old_page is already there.
        let mut rotations = 0;
        // SAFETY: `new_page_ptr` is attached to a VM object.
        while unsafe { !pq.debug_page_is_reclaim_isolate(new_page_ptr) } && rotations < 20 {
            pq.rotate_reclaim_queues();
            rotations += 1;
        }
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(old_page_ptr) });
        // SAFETY: pages are attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(new_page_ptr) });

        // Peek the isolate queue. Since old_page was aged first (older bucket), it should
        // be peeked first (FIFO ordering at the bucket level).
        let backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(backlink.is_some());
        expect_eq!(backlink.as_ref().unwrap().page.as_raw(), old_page_ptr.as_raw());

        // Remove old_page to verify that the next page peeked is new_page.
        // SAFETY: `old_page_ptr` is in a page queue.
        unsafe { pq.remove(old_page_ptr) };
        let next_backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(next_backlink.is_some());
        expect_eq!(next_backlink.as_ref().unwrap().page.as_raw(), new_page_ptr.as_raw());

        // SAFETY: `new_page_ptr` is in a page queue.
        unsafe { pq.remove(new_page_ptr) };
    }

    /// Tests FIFO ordering of pages marked don't need in the isolate queue.
    #[test]
    fn pq_isolate_dont_need_fifo_order() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        let mut old_page = VmPage::default();
        let old_page_ptr = initialize_test_page(&mut old_page);
        let mut new_page = VmPage::default();
        let new_page_ptr = initialize_test_page(&mut new_page);

        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(2, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Set up both pages and mark them "Don't Need".
        // old_page is marked first, then new_page. Use different offsets to represent distinct
        // pages. SAFETY: `old_page_ptr` is valid and not in a queue.
        unsafe {
            pq.set_reclaim(old_page_ptr, &cow, 0);
            pq.move_to_reclaim_dont_need(old_page_ptr);
        }
        // SAFETY: `old_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(old_page_ptr) });

        // SAFETY: `new_page_ptr` is valid and not in a queue.
        unsafe {
            pq.set_reclaim(new_page_ptr, &cow, 1);
            pq.move_to_reclaim_dont_need(new_page_ptr);
        }
        // SAFETY: `new_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(new_page_ptr) });

        // Peek the isolate queue. It should return old_page first (FIFO for Don't Need).
        let backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(backlink.is_some());
        expect_eq!(backlink.as_ref().unwrap().page.as_raw(), old_page_ptr.as_raw());

        // Remove old_page to verify that the next page peeked is new_page.
        // SAFETY: `old_page_ptr` is in a page queue.
        unsafe { pq.remove(old_page_ptr) };
        let next_backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(next_backlink.is_some());
        expect_eq!(next_backlink.as_ref().unwrap().page.as_raw(), new_page_ptr.as_raw());

        // SAFETY: `new_page_ptr` is in a page queue.
        unsafe { pq.remove(new_page_ptr) };
    }

    /// Tests eviction priority of don't need pages over standard aged pages.
    #[test]
    fn pq_isolate_queues_priority() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        pq.set_active_ratio_multiplier(0);
        pq.start_threads(DurationMono::from_nanos(0), DurationMono::from_nanos(ZX_TIME_INFINITE));

        let mut aged_page = VmPage::default();
        let aged_page_ptr = initialize_test_page(&mut aged_page);
        let mut dont_need_page = VmPage::default();
        let dont_need_page_ptr = initialize_test_page(&mut dont_need_page);

        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(2, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Set up the first page and rotate queues until it reaches the isolate queue.
        // Aged pages are placed in the standard isolate queue (isolate_queues[1]).
        // SAFETY: `aged_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(aged_page_ptr, &cow, 0) };
        // SAFETY: `aged_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim(aged_page_ptr) }.is_some());

        let mut rotations = 0;
        // SAFETY: `aged_page_ptr` is attached to a VM object.
        while unsafe { !pq.debug_page_is_reclaim_isolate(aged_page_ptr) } && rotations < 20 {
            pq.rotate_reclaim_queues();
            rotations += 1;
        }
        // SAFETY: `aged_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(aged_page_ptr) });

        // Set up the second page and mark it "Don't Need".
        // "Don't Need" pages are placed in the high-priority isolate queue (isolate_queues[0]).
        // SAFETY: `dont_need_page_ptr` is valid and not in a queue.
        unsafe {
            pq.set_reclaim(dont_need_page_ptr, &cow, 0);
            pq.move_to_reclaim_dont_need(dont_need_page_ptr);
        }
        // SAFETY: `dont_need_page_ptr` is attached to a VM object.
        expect_true!(unsafe { pq.debug_page_is_reclaim_isolate(dont_need_page_ptr) });

        // Peek the isolate queue. peek_isolate checks the high-priority queue (index 0)
        // before the standard queue (index 1). Thus, the "Don't Need" page is returned first,
        // verifying it is prioritized for eviction over the aged page.
        let backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(backlink.is_some());
        expect_eq!(backlink.as_ref().unwrap().page.as_raw(), dont_need_page_ptr.as_raw());

        // Remove the high-priority "Don't Need" page to verify that the next page
        // peeked from the isolate queue is the standard aged page.
        // SAFETY: `dont_need_page_ptr` is in a page queue.
        unsafe { pq.remove(dont_need_page_ptr) };
        let next_backlink = pq.peek_isolate(NUM_RECLAIM - 1);
        assert_true!(next_backlink.is_some());
        expect_eq!(next_backlink.as_ref().unwrap().page.as_raw(), aged_page_ptr.as_raw());

        // SAFETY: `aged_page_ptr` is in a page queue.
        unsafe { pq.remove(aged_page_ptr) };
    }

    /// Tests whether a page is considered reclaimable in different queues.
    #[test]
    fn pq_is_page_reclaimable() {
        pin_init::stack_pin_init!(let pq = PageQueues::init());

        let mut test_page = VmPage::default();
        let test_page_ptr = initialize_test_page(&mut test_page);

        let vmo = unwrap_ok!(make_uncommitted_pager_vmo(1, false, false));
        let cow = unwrap_some!(vmo.debug_get_cow_pages());

        // Only pages in the isolate queue should be considered reclaimable.
        // SAFETY: `test_page_ptr` is valid and not in a queue.
        unsafe { pq.set_reclaim(test_page_ptr, &cow, 0) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_false!(unsafe { PageQueues::is_page_reclaimable(test_page_ptr) });

        // Moving the page to the "Don't Need" queue should move to isolate.
        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_reclaim_dont_need(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_true!(unsafe { PageQueues::is_page_reclaimable(test_page_ptr) });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.move_to_reclaim(test_page_ptr) };
        // SAFETY: `test_page_ptr` is attached to a VM object.
        expect_false!(unsafe { PageQueues::is_page_reclaimable(test_page_ptr) });

        // SAFETY: `test_page_ptr` is in a page queue.
        unsafe { pq.remove(test_page_ptr) };
    }
}
