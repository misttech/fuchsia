// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::event::{AutounsignalEvent, Event};
use crate::kernel::types::PAddr;
use crate::vm::compression::VmCompression;
use crate::vm::evictor::Evictor;
use crate::vm::page::{VmPage, VmPagePtr};
use crate::vm::page_queues::PageQueues;
use crate::vm::pmm_arena::PmmArena;
use crate::vm::pmm_checker::PmmChecker;
use core::pin::Pin;
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use debug::ltracef;
use ksync::{KMutex, LockToken, RawMutex, guarded, kcell_init};
use pin_init::{PinInit, pin_data, pin_init};
use pmm_node_bindings as bindings;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

pub type AllocFailureType = bindings::PmmNode_AllocFailure_Type;

/// This enum is used to specify whether a page, when freed, should have its reuse
/// (i.e. reallocation) delayed.  This feature exists to both improve the PMM checker's ability
/// to detect "bad DMAs" and to reduce the impact when they do occur.  When allocating a page,
/// reusing the most recently freed page often has performance benefits.  However, it can
/// amplify the impact of use-after-free bugs.  This feature enables part of the kernel to
/// express a preference on whether a page should be eligible for immediate reuse or not.  It's
/// a hint.
///
/// |Default| means no preference.  When specified, the page may or may not be immediately reused.
/// In some build/runtime configurations (e.g. kasan) delayed reuse is the default behavior.
///
/// |Yes| indicates that the PMM should delay the reuse of the page by placing it on the "cold" end
/// up the free list, thereby maximizing the amount of time before which it is reallocated.
pub use bindings::PmmOptDelayReuse;

// Flags for PMM allocation routines.
/// no restrictions on which arena to allocate from.
pub const ALLOC_FLAG_ANY: u32 = bindings::PMM_ALLOC_FLAG_ANY;
/// The caller is able to wait and retry this allocation and so pmm allocation functions are allowed
/// to return ZX_ERR_SHOULD_WAIT, as opposed to ZX_ERR_NO_MEMORY, to indicate that the caller should
/// wait and try again. This is intended for the PMM to tell callers who are able to wait that
/// memory is low. The caller should not infer anything about memory state if it is told to wait, as
/// the PMM may tell it to wait for any reason.
pub const ALLOC_FLAG_CAN_WAIT: u32 = bindings::PMM_ALLOC_FLAG_CAN_WAIT;

/// Tell this PmmNode that we've failed a user-visible allocation.  Calling this method will
/// (optionally) trigger an asynchronous OOM response. To improve diagnostics some information
/// about the source of the failure can be provided.
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct AllocFailure {
    pub r#type: AllocFailureType,
    pub size: usize,
    pub free_count: u64,
}

impl Default for AllocFailure {
    fn default() -> Self {
        Self { r#type: AllocFailureType::None, size: 0, free_count: 0 }
    }
}

// Compile-time layout assertions against C++ AllocFailure
zr::static_assert!(
    core::mem::size_of::<AllocFailure>() == core::mem::size_of::<bindings::PmmNode_AllocFailure>()
);
zr::static_assert!(
    core::mem::align_of::<AllocFailure>()
        == core::mem::align_of::<bindings::PmmNode_AllocFailure>()
);
zr::static_assert!(
    core::mem::offset_of!(AllocFailure, r#type)
        == core::mem::offset_of!(bindings::PmmNode_AllocFailure, type_)
);
zr::static_assert!(
    core::mem::offset_of!(AllocFailure, size)
        == core::mem::offset_of!(bindings::PmmNode_AllocFailure, size)
);
zr::static_assert!(
    core::mem::offset_of!(AllocFailure, free_count)
        == core::mem::offset_of!(bindings::PmmNode_AllocFailure, free_count)
);

/// Controls the behavior of requests that have the PMM_ALLOC_FLAG_CAN_WAIT.
#[repr(u32)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ShouldWaitState {
    /// The PMM_ALLOC_FLAG_CAN_WAIT should never be followed and we will always attempt to perform
    /// the allocation, or fail with ZX_ERR_NO_MEMORY. This state is permanent and cannot be left.
    Never,
    /// Allocations do not need to be delayed, but the should_wait_free_pages_level should be
    /// monitored and once tripped should be delayed.
    OnceLevelTripped,
    /// State indicates that the level got tripped, and we should delay any allocations until the
    /// level is reset.
    UntilReset,
}

// Compile-time layout assertions against C++ ShouldWaitState
zr::static_assert!(
    core::mem::size_of::<ShouldWaitState>()
        == core::mem::size_of::<bindings::PmmNode_ShouldWaitState>()
);
zr::static_assert!(
    ShouldWaitState::Never as u32 == bindings::PmmNode_ShouldWaitState::Never as u32
);
zr::static_assert!(
    ShouldWaitState::OnceLevelTripped as u32
        == bindings::PmmNode_ShouldWaitState::OnceLevelTripped as u32
);
zr::static_assert!(
    ShouldWaitState::UntilReset as u32 == bindings::PmmNode_ShouldWaitState::UntilReset as u32
);

/// Waiter node for loaned page freeing synchronization.
#[derive(fbl::SinglyLinkedListContainable)]
#[repr(C)]
pub struct FreeLoanedPagesHolderWaiter {
    #[sll_node]
    pub node: fbl::SinglyLinkedListNode<FreeLoanedPagesHolderWaiter>,
    pub event: Event,
}

// Compile-time layout assertions against C++ Waiter
zr::static_assert!(
    core::mem::size_of::<FreeLoanedPagesHolderWaiter>()
        == core::mem::size_of::<bindings::FreeLoanedPagesHolder_Waiter>()
);
zr::static_assert!(
    core::mem::align_of::<FreeLoanedPagesHolderWaiter>()
        == core::mem::align_of::<bindings::FreeLoanedPagesHolder_Waiter>()
);

/// Object for managing freeing of loaned pages via a temporary holding object. Can be instantiated
/// on the stack and then passed into different PmmNode methods, the object itself has no publicly
/// available methods.
/// This object is not thread safe, and multiple threads must not pass the same instance of this
/// object into PmmNode methods.
/// A given FreeLoanedPagesHolder, as described in |FinishFreeLoanedPages|, may only be used for a
/// single call to |FinishFreeLoanedPages|, after which it is 'dead', and may not be passed to any
/// other PmmNode methods.
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct FreeLoanedPagesHolder {
    /// A given FreeLoanedPagesHolder interval is only allowed to be used once to return pages to
    /// the PMM, this tracks whether this has happened or not.
    /// Only permitting a single instance of freeing simplifies any need to reason about a single
    /// FreeLoanedPagesHolder repeatedly having pages moved into it and free'd to the PMM
    /// concurrently with attempts to wait on it.
    /// Although the lock cannot be annotated, this member is guarded by the relevant
    /// PmmNode::loaned_list_lock_.
    pub used: bool,
    /// List of pages presently owned by this object. Every page in this list is defined to be in
    /// the ALLOC state with |owner| set to this object.
    /// Although the lock cannot be annotated, this member is guarded by the relevant
    /// PmmNode::loaned_list_lock_.
    #[pin]
    pub pages: fbl::DoublyLinkedList<*mut VmPage>,
    /// Maintain a list of waiters to be notified once pages have been freed. The Waiter object
    /// itself is stack allocated in the WithLoanedPage method and registered into this list.
    /// Having this be a list of Events of single waiting thread, instead of a single Event
    /// with a list of waiting threads, allows the waiters to retain a reference to the FLPH
    /// object while waiting. This ensures that once FinishFreeLoanedPages performs the signal
    /// on the waiters, the FLPH object can be safely destroyed.
    pub waiters: fbl::SinglyLinkedList<*mut FreeLoanedPagesHolderWaiter>,
}

// Compile-time layout assertions against C++ FreeLoanedPagesHolder
zr::static_assert!(
    core::mem::size_of::<FreeLoanedPagesHolder>()
        == core::mem::size_of::<bindings::FreeLoanedPagesHolder>()
);
zr::static_assert!(
    core::mem::align_of::<FreeLoanedPagesHolder>()
        == core::mem::align_of::<bindings::FreeLoanedPagesHolder>()
);
zr::static_assert!(
    core::mem::offset_of!(FreeLoanedPagesHolder, used)
        == core::mem::offset_of!(bindings::FreeLoanedPagesHolder, used_)
);
zr::static_assert!(
    core::mem::offset_of!(FreeLoanedPagesHolder, pages)
        == core::mem::offset_of!(bindings::FreeLoanedPagesHolder, pages_)
);
zr::static_assert!(
    core::mem::offset_of!(FreeLoanedPagesHolder, waiters)
        == core::mem::offset_of!(bindings::FreeLoanedPagesHolder, waiters_)
);

impl FreeLoanedPagesHolder {
    /// Creates a new pinned initializer for `FreeLoanedPagesHolder`.
    pub fn init() -> impl pin_init::PinInit<Self, core::convert::Infallible> {
        pin_init::pin_init!(&_this in Self {
            used: false,
            pages <- fbl::DoublyLinkedList::new(),
            waiters: fbl::SinglyLinkedList::new(),
        })
    }
}

#[pin_init::pinned_drop]
impl PinnedDrop for FreeLoanedPagesHolder {
    fn drop(self: core::pin::Pin<&mut Self>) {
        assert!(self.pages.is_empty());
        assert!(self.waiters.is_empty());
    }
}

/// Per-NUMA node collection of physical memory arenas and bookkeeping.
#[repr(C)]
#[guarded]
pub struct PmmNode {
    canary: fbl::Canary<{ fbl::magic(b"PNOD") }>,

    #[mutex]
    lock: KMutex<RawMutex>,

    #[guarded_by(lock)]
    arena_cumulative_size: u64,
    // This is both an atomic and guarded by lock as we would like modifications to require the
    // lock, as logic in the system relies on the free_count not changing whilst the lock is
    // held, but also be an atomic so it can be correctly read without the lock.
    #[guarded_by(lock)]
    free_count: AtomicU64,
    #[guarded_by(lock)]
    free_loaned_count: AtomicU64,
    #[guarded_by(lock)]
    loaned_count: AtomicU64,
    #[guarded_by(lock)]
    loan_cancelled_count: AtomicU64,

    /// Free pages where !loaned.
    #[guarded_by(lock)]
    #[pin]
    free_list: fbl::DoublyLinkedList<*mut VmPage>,
    #[mutex]
    loaned_list_lock: KMutex<RawMutex>,
    /// Free pages where loaned && !loan_cancelled.
    #[guarded_by(loaned_list_lock)]
    #[pin]
    free_loaned_list: fbl::DoublyLinkedList<*mut VmPage>,

    /// The pages comprising the memory temporarily used during phys hand-off,
    /// populated on Init(). It is the responsibility of EndHandoff() to free this
    /// list.
    #[pin]
    phys_handoff_temporary_list: fbl::DoublyLinkedList<*mut VmPage>,

    /// The pages comprising the page-aligned regions of memory that we expect to
    /// turn into VMOs to hand-off to userspace - as determined by
    /// PhysHandoff::IsPhysVmoType() - populated and marked as wired on Init().
    ///
    /// It is expected that this memory will be unwired and turned into VMOs by the
    /// end of the phys hand-off phase, and it is the responsibility of
    /// PmmNode::EndHandoff() to ensure afterward that this list is empty.
    #[guarded_by(lock)]
    #[pin]
    phys_handoff_vmo_list: fbl::DoublyLinkedList<*mut VmPage>,

    /// The pages intended to be permanently reserved.
    #[guarded_by(lock)]
    #[pin]
    permanently_reserved_list: fbl::DoublyLinkedList<*mut VmPage>,

    should_wait: ShouldWaitState,

    /// Below this number of free pages the PMM will transition into delaying allocations.
    #[guarded_by(lock)]
    should_wait_free_pages_level: u64,

    /// The event acts a gate keeper for waking up threads waiting for allocations one at time.
    /// The event gets signalled when there MAY be pages available.
    #[pin]
    may_allocate_evt: AutounsignalEvent,

    /// Indicates whether a PMM alloc call has ever failed with ZX_ERR_NO_MEMORY. Used to trigger
    /// an OOM response.  See |MemoryWatchdog::WorkerThread|.
    alloc_failed_no_mem: AtomicBool,

    /// A record of the first time an allocation failure is reported to aid in diagnostics.
    #[guarded_by(lock)]
    first_alloc_failure: AllocFailure,

    /// If mem_signal is not null, then once the available free memory falls outside of the
    /// defined lower and upper bound the signal is raised. This is a one-shot signal and is
    /// cleared after firing.
    #[guarded_by(lock)]
    mem_signal: *mut Event,
    #[guarded_by(lock)]
    mem_signal_lower_bound: u64,
    #[guarded_by(lock)]
    mem_signal_upper_bound: u64,

    #[pin]
    page_queues: PageQueues,

    #[pin]
    evictor: Evictor,

    #[mutex]
    compression_lock: KMutex<RawMutex>,
    /// The page_compression is a lazily initialized RefPtr to keep the PmmNode constructor
    /// simple, at the cost needing to hold a lock to read the RefPtr. To avoid unnecessarily
    /// contending on the main pmm lock, use a separate one.
    #[guarded_by(compression_lock)]
    page_compression: Option<fbl::RefPtr<VmCompression>>,

    /// Indicates whether pages should have a pattern filled into them when they are freed. This
    /// value can only transition from false->true, and never back to false again. Once this
    /// value is set, the fill size in checker may no longer be changed, and it becomes safe
    /// to call FillPattern even without the lock held.
    /// This is an atomic to allow for reading this outside of the lock, but modifications only
    /// happen with the lock held.
    #[guarded_by(lock, loaned_list_lock)]
    free_fill_enabled: AtomicBool,
    /// Indicates whether it is known that all pages in the free list have had a pattern filled
    /// into them. This value can only transition from false->true, and never back to false
    /// again. Once this value is set the action and armed state in checker may no longer be
    /// changed, and it becomes safe to call AssertPattern even without the lock held.
    #[guarded_by(lock, loaned_list_lock)]
    all_free_pages_filled: bool,
    #[pin]
    checker: PmmChecker,

    /// The rng state for random waiting on allocations. This allows us to use rand_r, which
    /// requires no further thread synchronization, unlike rand().
    #[guarded_by(lock)]
    random_should_wait_seed: usize,

    #[guarded_by(lock)]
    used_arena_count: usize,
    #[guarded_by(lock)]
    arenas: [PmmArena; PmmNode::ARENA_COUNT],

    phantom: core::marker::PhantomData<core::marker::PhantomPinned>,
}

// Compile-time layout assertions against the C++ PmmNode type via bindgen.
zr::static_assert!(core::mem::size_of::<PmmNode>() == core::mem::size_of::<bindings::PmmNode>());
zr::static_assert!(core::mem::align_of::<PmmNode>() == core::mem::align_of::<bindings::PmmNode>());
zr::static_assert!(
    core::mem::offset_of!(PmmNode, canary) == core::mem::offset_of!(bindings::PmmNode, canary_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, lock) == core::mem::offset_of!(bindings::PmmNode, lock_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, arena_cumulative_size)
        == core::mem::offset_of!(bindings::PmmNode, arena_cumulative_size_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, free_count)
        == core::mem::offset_of!(bindings::PmmNode, free_count_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, free_loaned_count)
        == core::mem::offset_of!(bindings::PmmNode, free_loaned_count_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, loaned_count)
        == core::mem::offset_of!(bindings::PmmNode, loaned_count_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, loan_cancelled_count)
        == core::mem::offset_of!(bindings::PmmNode, loan_cancelled_count_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, free_list)
        == core::mem::offset_of!(bindings::PmmNode, free_list_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, loaned_list_lock)
        == core::mem::offset_of!(bindings::PmmNode, loaned_list_lock_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, free_loaned_list)
        == core::mem::offset_of!(bindings::PmmNode, free_loaned_list_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, phys_handoff_temporary_list)
        == core::mem::offset_of!(bindings::PmmNode, phys_handoff_temporary_list_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, phys_handoff_vmo_list)
        == core::mem::offset_of!(bindings::PmmNode, phys_handoff_vmo_list_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, permanently_reserved_list)
        == core::mem::offset_of!(bindings::PmmNode, permanently_reserved_list_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, should_wait)
        == core::mem::offset_of!(bindings::PmmNode, should_wait_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, should_wait_free_pages_level)
        == core::mem::offset_of!(bindings::PmmNode, should_wait_free_pages_level_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, may_allocate_evt)
        == core::mem::offset_of!(bindings::PmmNode, may_allocate_evt_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, alloc_failed_no_mem)
        == core::mem::offset_of!(bindings::PmmNode, alloc_failed_no_mem_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, first_alloc_failure)
        == core::mem::offset_of!(bindings::PmmNode, first_alloc_failure_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, mem_signal)
        == core::mem::offset_of!(bindings::PmmNode, mem_signal_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, mem_signal_lower_bound)
        == core::mem::offset_of!(bindings::PmmNode, mem_signal_lower_bound_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, mem_signal_upper_bound)
        == core::mem::offset_of!(bindings::PmmNode, mem_signal_upper_bound_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, page_queues)
        == core::mem::offset_of!(bindings::PmmNode, page_queues_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, evictor) == core::mem::offset_of!(bindings::PmmNode, evictor_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, compression_lock)
        == core::mem::offset_of!(bindings::PmmNode, compression_lock_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, page_compression)
        == core::mem::offset_of!(bindings::PmmNode, page_compression_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, free_fill_enabled)
        == core::mem::offset_of!(bindings::PmmNode, free_fill_enabled_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, all_free_pages_filled)
        == core::mem::offset_of!(bindings::PmmNode, all_free_pages_filled_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, checker) == core::mem::offset_of!(bindings::PmmNode, checker_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, random_should_wait_seed)
        == core::mem::offset_of!(bindings::PmmNode, random_should_wait_seed_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, used_arena_count)
        == core::mem::offset_of!(bindings::PmmNode, used_arena_count_)
);
zr::static_assert!(
    core::mem::offset_of!(PmmNode, arenas) == core::mem::offset_of!(bindings::PmmNode, arenas_)
);

unsafe impl Sync for PmmNode {}
unsafe impl Send for PmmNode {}

impl PmmNode {
    /// Arenas are allocated from the node itself to avoid any boot allocations. Walking linearly
    /// through them at run time should also be fairly efficient.
    const ARENA_COUNT: usize = bindings::PmmNode_kArenaCount;
    /// Bit constants to fold a vm_page_t pointer into a uint32 and back. The format from LSB
    /// to MSB is : zero-bits | arena-index | page-index |. Where arena-index is 4 bits wide and
    /// zero-bits is 3 bits wide. This limits the number of pages per arena to 2^25.
    const ARENA_BITS: i32 = bindings::PmmNode_kArenaBits;
    pub const INDEX_ZERO_BITS: i32 = bindings::PmmNode_kIndexZeroBits;
    const MAX_PAGES_PER_ARENA: usize = bindings::PmmNode_kMaxPagesPerArena;
    const ARENA_MASK: u32 = bindings::PmmNode_kArenaMask;

    fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            canary: fbl::Canary::new(),
            lock <- KMutex::init(),
            arena_cumulative_size: 0.into(),
            free_count: AtomicU64::new(0).into(),
            free_loaned_count: AtomicU64::new(0).into(),
            loaned_count: AtomicU64::new(0).into(),
            loan_cancelled_count: AtomicU64::new(0).into(),
            free_list <- kcell_init(fbl::DoublyLinkedList::new()),
            loaned_list_lock <- KMutex::init(),
            free_loaned_list <- kcell_init(fbl::DoublyLinkedList::new()),
            phys_handoff_temporary_list <- fbl::DoublyLinkedList::new(),
            phys_handoff_vmo_list <- kcell_init(fbl::DoublyLinkedList::new()),
            permanently_reserved_list <- kcell_init(fbl::DoublyLinkedList::new()),
            should_wait: ShouldWaitState::OnceLevelTripped,
            should_wait_free_pages_level: 0.into(),
            may_allocate_evt <- AutounsignalEvent::init_signaled(),
            alloc_failed_no_mem: AtomicBool::new(false),
            first_alloc_failure: AllocFailure::default().into(),
            mem_signal: core::ptr::null_mut::<Event>().into(),
            mem_signal_lower_bound: 0.into(),
            mem_signal_upper_bound: 0.into(),
            page_queues <- PageQueues::init(),
            evictor <- Evictor::init(),
            compression_lock <- KMutex::init(),
            page_compression: None.into(),
            free_fill_enabled: AtomicBool::new(false),
            all_free_pages_filled: false,
            checker <- PmmChecker::init(),
            random_should_wait_seed: 0.into(),
            used_arena_count: 0.into(),
            arenas: [const { PmmArena::new() }; PmmNode::ARENA_COUNT].into(),
            phantom: core::marker::PhantomData,
        })
    }

    /// Domain-specific conversion: returns raw pointer for `PmmNode`.
    pub fn as_raw(&self) -> *mut bindings::PmmNode {
        (self as *const Self).cast_mut().cast()
    }

    /// Return the slice of arenas from the built-in array that are known to be active. Used in
    /// loops that iterate across all arenas.
    fn active_arenas<'a>(&'a self, token: &'a LockToken<'_, PmmNodeLockClass>) -> &'a [PmmArena] {
        // SAFETY: The lock token proves that either the lock protecting these fields is held, or
        // the caller has determined it is safe.
        unsafe { &self.arenas.get(token)[..*self.used_arena_count.get(token)] }
    }

    /// Converts the number returned by page_to_index() back to a VmPagePtr pointer.
    /// It does not check for invalid indexes such as 0.
    ///
    /// Note: This method is faster than page_to_index, about the cost of some basic math
    ///       and bit manipulation.
    ///
    /// # Safety
    ///
    /// The `index` must be a valid PMM page index.
    pub unsafe fn index_to_page(&self, index: u32) -> VmPagePtr {
        let index = index >> Self::INDEX_ZERO_BITS;
        let arena_ix = (index & Self::ARENA_MASK) as usize;
        let page_ix = (index >> Self::ARENA_BITS) as usize;
        // SAFETY: The arena is only modified during initialization so its safe to synthesize a
        // a lock token.
        unsafe {
            VmPagePtr::new(self.active_arenas(&LockToken::new())[arena_ix].get_page(page_ix - 1))
        }
    }

    /// Returns compressed representation a page_t*, with the following characteristics:
    /// - zeros in the last INDEZ_ZERO_BITS bits, used by clients to store metadata.
    /// - The value 0 is never returned, it can be used as "no page" marker.
    ///
    /// Note: This method needs to traverse (up to) all the memory pools so it's cost is
    ///       low but not trivial.
    ///
    pub fn page_to_index(&self, page: VmPagePtr) -> u32 {
        let page_raw = page.as_raw();
        // SAFETY: The arena is only modified during initialization so its safe to synthesize a
        // lock token.
        let token = unsafe { LockToken::new() };
        for (arena_ix, a) in self.active_arenas(&token).iter().enumerate() {
            // SAFETY: `page` is a valid VmPagePtr so `page_raw` is a valid pointer.
            if unsafe { a.page_belongs_to_arena(page_raw) } {
                // SAFETY: `page_raw` belongs to this arena's `page_array`.
                let page_ix = (unsafe { a.get_index(page_raw) } + 1) as u32;
                return ((page_ix << Self::ARENA_BITS) | (arena_ix as u32))
                    << Self::INDEX_ZERO_BITS;
            }
        }
        0
    }

    /// Converts the number returned by page_to_index() back to a PAddr.
    /// It does not check for invalid indexes such as 0 or kIndexReserved0.
    ///
    /// Note: This method is faster than page_to_index().paddr() as the VmPagePtr itself does not
    /// have to be de-referenced, saving a memory load.
    ///
    /// # Safety
    ///
    /// The `index` must be a valid PMM page index.
    pub unsafe fn index_to_paddr(&self, index: u32) -> PAddr {
        let index = index >> Self::INDEX_ZERO_BITS;
        let arena_ix = (index & Self::ARENA_MASK) as usize;
        let page_ix = (index >> Self::ARENA_BITS) as usize;
        // SAFETY: The arena is only modified during initialization so its safe to synthesize a
        // lock token.
        unsafe {
            let token = LockToken::new();
            let base = self.active_arenas(&token)[arena_ix].base().0;
            PAddr(base + (page_ix - 1) * page::SIZE)
        }
    }

    /// Allocates a single physical page from this node.
    pub fn alloc_page(&self, alloc_flags: u32) -> Result<VmPagePtr, Status> {
        let mut page = core::ptr::null_mut();
        // SAFETY: FFI call passing valid stack pointer.
        let status =
            unsafe { bindings::cpp_pmm_node_alloc_page(self.as_raw(), alloc_flags, &mut page) };
        Status::ok(status)?;
        // SAFETY: page returned from PMM on success is valid.
        unsafe { VmPagePtr::from_ffi(page) }.ok_or(Status::NO_MEMORY)
    }

    /// Allocates `count` physical pages, adding them to the tail of `list`.
    pub fn alloc_pages(
        &self,
        count: usize,
        alloc_flags: u32,
        list: Pin<&mut fbl::DoublyLinkedList<*mut VmPage>>,
    ) -> Result<(), Status> {
        // SAFETY: FFI call passing pointer to `list`.
        let status = unsafe {
            bindings::cpp_pmm_node_alloc_pages(
                self.as_raw(),
                count,
                alloc_flags,
                (list.get_unchecked_mut() as *mut fbl::DoublyLinkedList<*mut VmPage>).cast(),
            )
        };
        Status::ok(status)
    }

    /// Frees a single physical page back to this node.
    ///
    /// # Safety
    ///
    /// Caller guarantees that page is valid and they are the owner.
    pub unsafe fn free_page(&self, page: VmPagePtr, delay_reuse: PmmOptDelayReuse) {
        // SAFETY: FFI call with valid page pointer.
        unsafe { bindings::cpp_pmm_node_free_page(self.as_raw(), page.as_ffi(), delay_reuse) }
    }

    /// Frees every page on `list` back to this node.
    ///
    /// # Safety
    ///
    /// Caller guarantees that all pages in list are valid and they are the owner.
    pub unsafe fn free_list(
        &self,
        list: Pin<&mut fbl::DoublyLinkedList<*mut VmPage>>,
        delay_reuse: PmmOptDelayReuse,
    ) {
        if list.is_empty() {
            return;
        }
        // SAFETY: FFI call with valid list pointer.
        unsafe {
            bindings::cpp_pmm_node_free_list(
                self.as_raw(),
                (list.get_unchecked_mut() as *mut fbl::DoublyLinkedList<*mut VmPage>).cast(),
                delay_reuse,
            )
        }
    }

    /// Return count of unallocated physical pages in this node.
    pub fn count_free_pages(&self) -> u64 {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_count_free_pages(self.as_raw()) }
    }

    /// Return count of unallocated loaned physical pages in this node.
    pub fn count_loaned_free_pages(&self) -> u64 {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_count_loaned_free_pages(self.as_raw()) }
    }

    /// Return count of pages which are presently loaned with the loan cancelled.
    pub fn count_loan_cancelled_pages(&self) -> u64 {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_count_loan_cancelled_pages(self.as_raw()) }
    }

    /// Return count of loaned pages that are not free.
    pub fn count_loaned_not_free_pages(&self) -> u64 {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_count_loaned_not_free_pages(self.as_raw()) }
    }

    /// Return count of loaned pages in this node.
    pub fn count_loaned_pages(&self) -> u64 {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_count_loaned_pages(self.as_raw()) }
    }

    /// Return amount of physical memory in this node, in bytes.
    pub fn count_total_bytes(&self) -> u64 {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_count_total_bytes(self.as_raw()) }
    }

    /// Enable the free fill checker with the specified fill size and action, and begin filling
    /// freed pages (including freed loaned pages) going forward.  See |PmmChecker| for definition
    /// of fill size.
    ///
    /// Note, pages freed piror to calling this method will remain unfilled.  To fill them, call
    /// |FillFreePagesAndArm|.
    ///
    /// Returns true if the checker was enabled with the requested fill_size, or |false| otherwise.
    pub fn enable_free_page_filling(
        &self,
        fill_size: usize,
        action: crate::vm::pmm_checker::CheckFailAction,
    ) -> bool {
        // SAFETY: No preconditions.
        unsafe {
            bindings::cpp_pmm_node_enable_free_page_filling(self.as_raw(), fill_size, action as u8)
        }
    }

    /// Fill all free pages (both non-loaned and loaned) with a pattern and arm the checker.  See
    /// |PmmChecker|.
    ///
    /// This is a no-op if the checker is not enabled.  See |EnableFreePageFilling|
    pub fn fill_free_pages_and_arm(&self) {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_fill_free_pages_and_arm(self.as_raw()) }
    }

    /// Configures the free memory bounds and allows for setting a one shot signal as well as a
    /// level where allocations should start being delayed.
    ///
    /// The event is signaled once the number of PMM free pages falls outside of the range given by
    /// |free_lower_bound| and |free_upper_bound|. As the event is one shot, one signaled this must
    /// be called again to configure a new range. If the number of free pages is already outside the
    /// requested bound then this method fails (returns false) and no event is setup. In this case
    /// the caller should recalculate a correct bounds and try again.
    ///
    /// In addition to exiting the provided memory bounds, the event will also get signaled on the
    /// first time an allocation fails (i.e. the first time at which has_alloc_failed_no_mem would
    /// return true).
    ///
    /// |delay_allocations_level| is the number of PMM free pages below which the PMM will
    /// transition to delaying allocations that can wait, i.e. those with PMM_ALLOC_FLAG_CAN_WAIT.
    /// This transition is sticky, and even if pages are freed to go back above this line,
    /// allocations will remain delayed until this method is called again to re-set the level. For
    /// this reason, and since there is only a single common Event, the |delay_allocations_level|
    /// must either be <= the |free_lower_bound|, ensuring that the caller will have been notified
    /// and can respond by freeing memory and/or setting a new level, or |delay_allocations_level|
    /// can be UINT64_MAX, indicating allocations should start and remain delayed.
    ///
    /// # Safety
    ///
    /// Caller ensures that `event` lives either until this method is called again or the PmmNode is
    /// destroyed.
    pub unsafe fn set_free_memory_signal(
        &self,
        lower_bound: u64,
        upper_bound: u64,
        delay_allocations_pages: u64,
        event: core::ptr::NonNull<Event>,
    ) -> bool {
        // SAFETY: `event.as_raw()` returns a valid Event pointer.
        unsafe {
            bindings::cpp_pmm_node_set_free_memory_signal(
                self.as_raw(),
                lower_bound,
                upper_bound,
                delay_allocations_pages,
                event.as_ptr().cast(),
            )
        }
    }

    /// Waits the system to exit low memory state and then attempts to allocate.
    ///
    /// To prevent herding problem, and because allocation compete for the `PmmNode::lock_` anyway,
    /// only one thread is woken up a time, only if the previous thread successfully allocated.
    ///
    /// In normal conditions,  when the system is in low memory state, this method will return
    /// `ZX_ERR_TIMED_OUT` if the system didn't transition fast enough. If we run into a TOC to TOU,
    /// for the system race `ZX_ERR_SHOULD_WAIT` will be returned, that is the system transitioned
    /// out and back into low memory state before we managed to perform the allocation.
    ///
    /// If `BootOptions::Get()->pmm_alloc_random_wait` is true, then the system
    /// may return spurious `ZX_ERR_SHOULD_WAIT`, in such cases, if the system is
    /// not in a low memory state, a thread is woken up anyway, so forward
    /// progress can be made.
    ///
    /// If |suspendable| is true, the wait will terminate early with
    /// `ZX_ERR_INTERNAL_INTR_RETRY` if the thread is suspended. If false, suspension is ignored and
    /// the wait continues.
    pub fn wait_for_single_page_allocation(
        &self,
        deadline: crate::kernel::deadline::Deadline,
        suspendable: bool,
    ) -> Result<VmPagePtr, Status> {
        let mut page = core::ptr::null_mut();
        // SAFETY: FFI call passing valid stack pointer.
        let status = unsafe {
            bindings::cpp_pmm_node_wait_for_single_page_allocation(
                self.as_raw(),
                deadline.when().0,
                suspendable,
                &mut page,
            )
        };
        Status::ok(status)?;
        // SAFETY: page pointer is valid on success.
        unsafe { VmPagePtr::from_ffi(page) }.ok_or(Status::NO_MEMORY)
    }

    /// Tells the node to stop returning SHOULD_WAIT.
    pub fn stop_returning_should_wait(&self) {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_stop_returning_should_wait(self.as_raw()) }
    }

    /// Returns whether an allocation has failed with NO_MEMORY.
    pub fn has_alloc_failed_no_mem(&self) -> bool {
        // SAFETY: No preconditions.
        unsafe { bindings::cpp_pmm_node_has_alloc_failed_no_mem(self.as_raw()) }
    }

    /// Retrieves information given to |ReportAllocFailure|. Due to book keeping limitations this will
    /// only return information from the first failure.
    pub fn get_first_alloc_failure(&self) -> AllocFailure {
        let mut failure = AllocFailure::default();
        // SAFETY: FFI call passing valid stack pointer.
        unsafe {
            bindings::cpp_pmm_node_get_first_alloc_failure(
                self.as_raw(),
                &mut failure as *mut AllocFailure as *mut bindings::PmmNode_AllocFailure,
            )
        };
        failure
    }

    /// This method should be called when the PMM fails to allocate in a user-visible way and will
    /// (optionally) trigger an asynchronous OOM response.
    pub fn report_alloc_failure(&self, failure: AllocFailure) {
        // SAFETY: FFI call passing valid reference pointer.
        unsafe {
            bindings::cpp_pmm_node_report_alloc_failure(
                self.as_raw(),
                &failure as *const AllocFailure as *const bindings::PmmNode_AllocFailure,
            )
        }
    }

    /// Frees all pages in the given list and places them in the loaned state available to be returned
    /// from AllocLoanedPage.
    ///
    /// |delay_reuse| controls whether the newly loaned pages are eligile for immediate or delayed
    /// reuse.
    ///
    /// # Safety
    ///
    /// Caller guarantees that all pages in list are valid and they are the owner.
    pub unsafe fn begin_loan(
        &self,
        list: Pin<&mut fbl::DoublyLinkedList<*mut VmPage>>,
        delay_reuse: PmmOptDelayReuse,
    ) {
        // SAFETY: FFI call passing valid list pointer.
        unsafe {
            bindings::cpp_pmm_node_begin_loan(
                self.as_raw(),
                (list.get_unchecked_mut() as *mut fbl::DoublyLinkedList<*mut VmPage>).cast(),
                delay_reuse,
            )
        }
    }

    /// Marks a page that had been previously provided to BeginLoan as cancelled. This page may be in
    /// the FREE_LOANED state, or presently in use.
    ///
    /// This call prevents the page from being reused for any new purpose until EndLoan(). For
    /// presently-FREE_LOANED pages, this removes the pages from free_loaned_list_. For presently-used
    /// pages, this specifies that the page will not be added to free_loaned_list_ when later freed.
    /// Once this page is FREE_LOANED (to be ensured by the caller via PhysicalPageProvider reclaim of
    /// the pages), the loan can be ended with EndLoan().
    ///
    /// # Safety
    ///
    /// Caller guarantees that page is valid
    pub unsafe fn cancel_loan(&self, page: VmPagePtr) {
        // SAFETY: FFI call passing valid page pointer.
        unsafe { bindings::cpp_pmm_node_cancel_loan(self.as_raw(), page.as_ffi()) }
    }

    /// Allocates the page to the caller as a regular non-loaned page. Must currently be:
    ///  * Loaned (via BeginLoan).
    ///  * Have had its loan cancelled (via CancelLoan).
    ///  * Be in the FREE_LOANED state.
    ///
    /// # Safety
    ///
    /// Caller guarantees that page is valid
    pub unsafe fn end_loan(&self, page: VmPagePtr) {
        // SAFETY: FFI call passing valid page pointer.
        unsafe { bindings::cpp_pmm_node_end_loan(self.as_raw(), page.as_ffi()) }
    }

    /// Allocates a single page from the loaned pages list. The allocated page will always have
    /// is_loaned() being true, and must be returned by either FreeLoanedPage or FreeLoanedList. If
    /// there are not loaned pages available ZX_ERR_UNAVAILABLE is returned, as an absence of loaned
    /// pages does not constitute an out of memory scenario.
    /// The provided callback must transition the page into a state such that it has a valid backlink,
    /// i.e. it is in the OBJECT state with an owner set, prior to returning.
    /// During the execution of the callback the page contents must *not* be modified.
    pub fn alloc_loaned_page<F: FnOnce(VmPagePtr)>(
        &self,
        allocated: F,
    ) -> Result<VmPagePtr, Status> {
        unsafe extern "C" fn trampoline<F: FnOnce(VmPagePtr)>(
            page: *mut page_bindings::vm_page_t,
            cookie: *mut core::ffi::c_void,
        ) {
            // SAFETY: cookie is a valid pointer to Option<F> on the stack.
            let closure = unsafe { &mut *(cookie.cast::<Option<F>>()) }
                .take()
                .expect("Callback should only be invoked once");
            // SAFETY: page is a valid allocated vm_page_t pointer passed by PmmNode.
            closure(unsafe { VmPagePtr::from_ffi(page) }.expect("Expected value page"));
        }

        let mut closure = Some(allocated);
        let mut out_page = core::ptr::null_mut();
        // SAFETY: FFI call passing valid node, function pointer, cookie pointer, and out pointer.
        let status = unsafe {
            bindings::cpp_pmm_node_alloc_loaned_page(
                self.as_raw(),
                Some(trampoline::<F>),
                core::ptr::from_mut(&mut closure).cast(),
                &mut out_page,
            )
        };
        Status::ok(status)?;
        // SAFETY: `out_page` is a non-null valid page pointer returned on success.
        unsafe { Ok(VmPagePtr::from_ffi(out_page).expect("null page on success")) }
    }

    /// Begins freeing a loaned page that was previously allocated by AllocLoanPage by moving into a
    /// holding object. It is an error to attempt to free a non loaned page. When this method is called
    /// the |page| must have a valid backlink (i.e. be in the OBJECT state with an owner set). This
    /// backlink should be removed by the |release_page| callback, which is invoked under the loaned
    /// pages lock, prior to transition the page into the holding state. The caller *must*, at some
    /// point in the future, complete the page freeing process by passing the provided |flph| into a
    /// |FinishFreeLoanedPages| call.
    ///
    /// # Safety
    ///
    /// Caller guarantees that page is valid, loaned and owned by them.
    pub unsafe fn begin_free_loaned_page<F: FnOnce(VmPagePtr)>(
        &self,
        page: VmPagePtr,
        release_page: F,
        flph: Pin<&mut FreeLoanedPagesHolder>,
    ) {
        unsafe extern "C" fn trampoline<F: FnOnce(VmPagePtr)>(
            page: *mut page_bindings::vm_page_t,
            cookie: *mut core::ffi::c_void,
        ) {
            // SAFETY: cookie is a valid pointer to Option<F> on the stack.
            let closure = unsafe { &mut *(cookie.cast::<Option<F>>()) }
                .take()
                .expect("Callback should only be called once");
            // SAFETY: page is a valid allocated vm_page_t pointer passed by PmmNode.
            closure(unsafe { VmPagePtr::from_ffi(page) }.expect("Expected value page"));
        }

        let mut closure = Some(release_page);
        let flph_ptr: *mut FreeLoanedPagesHolder = unsafe { flph.get_unchecked_mut() };
        // SAFETY: FFI call passing valid node, page, function pointer, cookie pointer, and flph pointer.
        unsafe {
            bindings::cpp_pmm_node_begin_free_loaned_page(
                self.as_raw(),
                page.as_ffi(),
                Some(trampoline::<F>),
                core::ptr::from_mut(&mut closure).cast(),
                flph_ptr.cast(),
            );
        }
    }

    /// Completes the freeing of any loaned pages in |flph|, after which |flph| is allowed to be
    /// destructed. Once this method is called on a given |flph| that object is effectively 'dead' and
    /// is not allowed to be passed to any PmmNode methods.
    pub fn finish_free_loaned_pages(&self, flph: Pin<&mut FreeLoanedPagesHolder>) {
        let flph_ptr: *mut FreeLoanedPagesHolder = unsafe { flph.get_unchecked_mut() };
        // SAFETY: FFI call passing valid node and flph pointer.
        unsafe {
            bindings::cpp_pmm_node_finish_free_loaned_pages(self.as_raw(), flph_ptr.cast());
        }
    }

    /// Add new pages to the free queue. Used when bootstrapping a PmmArena.
    ///
    /// # Safety
    ///
    /// Caller guarantees that all pages in |list| are valid and owned by them.
    pub unsafe fn add_free_pages(
        &mut self,
        mut list: Pin<&mut fbl::DoublyLinkedList<*mut VmPage>>,
    ) {
        ltracef!("list {:p}\n", list.as_ref().get_ref() as *const _);

        // SAFETY: called at boot time as arenas are brought online, no locks are acquired
        let mut token = unsafe { LockToken::new() };

        let mut free_count = 0u64;
        // SAFETY: pop_front does not move data outside the list and therefore the Pin invariants
        // are upheld.
        while let Some(page) = unsafe { list.as_mut().get_unchecked_mut().pop_front() } {
            // SAFETY: `page` comes from the list of valid `VmPage` pointers constructed during
            // arena initialization.
            unsafe {
                debug_assert!(!(*page).is_loaned());
                debug_assert!(!(*page).is_loan_cancelled());
                debug_assert!((*page).is_free());
                self.free_list.get_mut(&mut token).push_back_raw(page);
            }
            free_count += 1;
        }
        // SAFETY: called at boot time as arenas are brought online, no locks are acquired
        unsafe { self.free_count.get(&token) }.fetch_add(free_count, Ordering::Relaxed);
        // SAFETY: called at boot time as arenas are brought online, no locks are acquired
        assert!(unsafe { self.free_count.get(&token) }.load(Ordering::Relaxed) != 0);
        self.may_allocate_evt.signal();

        ltracef!(
            "free count now {}\n",
            unsafe { self.free_count.get(&token) }.load(Ordering::Relaxed)
        );
    }

    /// Retrieve access to the page queues.
    pub fn page_queues(&self) -> &PageQueues {
        &self.page_queues
    }
}

#[unsafe(no_mangle)]
unsafe extern "C" fn rust_pmm_node_add_free_pages(
    node: *mut PmmNode,
    list: *mut fbl::DoublyLinkedList<*mut VmPage>,
) {
    // SAFETY: Caller guarantees these are not null and are pinned.
    unsafe { node.as_mut_unchecked().add_free_pages(Pin::new_unchecked(list.as_mut_unchecked())) }
}

/// Unit tests for PmmNode.
#[cfg(ktest)]
#[unittest::suite(name = "pmm_node_rust")]
mod pmm_node_rust {
    use pin_init::stack_pin_init;
    /// Tests simple creation and destruction
    #[test]
    fn smoke() {
        stack_pin_init!(let _pmm = PmmNode::init());
    }
}
