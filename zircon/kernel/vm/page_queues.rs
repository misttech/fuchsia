// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::page::VmPagePtr;
use core::marker::{PhantomData, PhantomPinned};
use page_queues_bindings as bindings;
use zr::Opaque;
use zx_types::zx_duration_mono_t;

/// Used to identify the reason that aging is triggered, mostly for debugging and informational
/// purposes.
pub type AgeReason = bindings::PageQueues_AgeReason;

/// Describes any action to take when processing the LRU queue. This is applied to pages that would
/// otherwise have to be moved from the old LRU queue into the isolate queue.
pub type LruAction = bindings::PageQueues_LruAction;

#[derive(Debug)]
pub struct QueueAge(pub usize);

#[repr(C)]
pub struct PageQueues {
    raw: Opaque<bindings::PageQueues>,
    phantom: PhantomData<PhantomPinned>,
}

impl PageQueues {
    /// The number of reclamation queues is slightly arbitrary, but to be useful you want at least 3
    /// representing
    ///  * Very new pages that you probably don't want to evict as doing so probably implies you are
    ///    in swap death
    ///  * Slightly old pages that could be evicted if needed
    ///  * Very old pages that you'd be happy to evict
    ///
    /// With two active queues 8 page queues are used so that there is some fidelity of information
    /// in the inactive queues. Additional queues have reduced value as sufficiently old pages
    /// quickly become equivalently unlikely to be used in the future.
    pub const NUM_RECLAIM: usize = bindings::PageQueues_kNumReclaim;

    /// Two active queues are used to allow for better fidelity of active information. This prevents
    /// a race between aging once and needing to collect/harvest age information.
    pub const NUM_ACTIVE_QUEUES: usize = bindings::PageQueues_kNumActiveQueues;

    /// The amount of pages that will have to move around the queues before the active/inactive
    /// ratio is re-checked. This therefore represents how much error the active ratio aging process
    /// might have, or how delayed the MRU generation might be. In the worst case once the active
    /// ratio is triggered this value is how much page data needs to then change queues before the
    /// aging process happens.
    pub const ACTIVE_INACTIVE_ERROR_MARGIN: usize = bindings::PageQueues_kActiveInactiveErrorMargin;

    /// In addition to active and inactive, we want to consider some of the queues as 'oldest' to
    /// provide an additional way to limit eviction. Presently the processing of the LRU queue to
    /// make room for aging is not integrated with the Evictor, and so will not trigger eviction,
    /// therefore to have a non-zero number of pages ever appear in an oldest queue for eviction the
    /// last two queues are considered the oldest.
    pub const NUM_OLDEST_QUEUES: usize = bindings::PageQueues_kNumOldestQueues;

    /// Number of different isolate queues that are available. Different isolate queues allow for
    /// separating isolate pages into different buckets such that more nuanced choices on what page
    /// to reclaim can be made.
    ///
    /// We use 2 queues to separate "Don't Need" pages (high reclamation priority, index 0) from
    /// standard aged pages (standard reclamation priority, index 1).
    pub const ISOLATE_QUEUE_DONT_NEED: usize = bindings::PageQueues_kIsolateQueueDontNeed;
    pub const ISOLATE_QUEUE_STANDARD: usize = bindings::PageQueues_kIsolateQueueStandard;
    pub const NUM_ISOLATE_QUEUES: usize = bindings::PageQueues_kNumIsolateQueues;

    pub const DEFAULT_MIN_MRU_ROTATE_TIME: zx_duration_mono_t =
        bindings::PageQueues_kDefaultMinMruRotateTime;
    pub const DEFAULT_MAX_MRU_ROTATE_TIME: zx_duration_mono_t =
        bindings::PageQueues_kDefaultMaxMruRotateTime;

    /// This is presently an arbitrary constant, since the min and max mru rotate time are currently
    /// fixed at the same value, meaning that the active ratio can not presently trigger, or
    /// prevent, aging.
    pub const DEFAULT_ACTIVE_RATIO_MULTIPLIER: u64 =
        bindings::PageQueues_kDefaultActiveRatioMultiplier;

    /// Domain-specific conversion: returns raw pointer for `PageQueues`.
    pub fn as_raw(&self) -> *mut bindings::PageQueues {
        self.raw.get()
    }

    /// Returns whether `page` is in the wired queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is attached to a VM object.
    pub unsafe fn debug_page_is_wired(&self, page: VmPagePtr) -> bool {
        // SAFETY: `self` is valid for required accesses, and the caller guarantees `page` is
        // attached to a VM object per function safety preconditions.
        unsafe { bindings::cpp_page_queues_debug_page_is_wired(self.as_raw(), page.as_raw()) }
    }

    /// Returns whether `page` is in any anonymous queue.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is attached to a VM object.
    pub unsafe fn debug_page_is_any_anonymous(&self, page: VmPagePtr) -> bool {
        // SAFETY: `self` is valid for required accesses, and the caller guarantees `page` is
        // attached to a VM object per function safety preconditions.
        unsafe {
            bindings::cpp_page_queues_debug_page_is_any_anonymous(self.as_raw(), page.as_raw())
        }
    }

    /// Returns `Some(QueueAge)` if `page` is currently in a reclaim queue, or `None` if it is not.
    ///
    /// # Safety
    ///
    /// The caller must guarantee `page` is attached to a VM object.
    pub unsafe fn debug_page_is_reclaim(&self, page: VmPagePtr) -> Option<QueueAge> {
        let mut age = 0;
        // SAFETY: `self` and `&mut age` are valid for required accesses, and the caller
        // guarantees `page` is attached to a VM object per function safety preconditions.
        let is_reclaim = unsafe {
            bindings::cpp_page_queues_debug_page_is_reclaim(self.as_raw(), page.as_raw(), &mut age)
        };
        if is_reclaim { Some(QueueAge(age)) } else { None }
    }

    /// Rotates the reclaim queues.
    pub fn rotate_reclaim_queues(&self) {
        // SAFETY: `self.as_raw()` returns a valid `PageQueues` pointer.
        unsafe { bindings::cpp_page_queues_rotate_reclaim_queues(self.as_raw()) }
    }
}
