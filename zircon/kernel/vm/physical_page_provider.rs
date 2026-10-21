// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicPtr, AtomicU64, Ordering};

use crate::counters::define_kcounter;
use crate::kernel::deadline::{Deadline, TimerSlack};
use crate::kernel::event::Event;
use crate::kernel::types::PAddr;
use crate::platform_rs::timer::DurationMono;
use crate::vm::anonymous_page_request::AnonymousPageRequest;
use crate::vm::page::{VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::page_source::{
    PageProviderTag, PageRequest, PageRequestType, PageSource, PageSourceProperties,
};
use crate::vm::page_state::VmPageState;
use crate::vm::physical_page_borrowing_config::PhysicalPageBorrowingConfig;
use crate::vm::pmm_node::PmmOptDelayReuse;
use crate::vm::vm_cow_pages::{DeferredOps, SupplyOptions, VmCowPages, VmCowRange};
use crate::vm::vm_page_list::VmPageSpliceList;
use crate::vm::{physmap, pmm};

use fbl::{DoublyLinkedList, DoublyLinkedListContainable};
use ksync::{KMutex, RawMutex, guarded, kcell_init};
use page_bindings::{vm_page_state, vm_page_t};
use page_source_bindings::PageSource as CppPageSource;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use vm_constants_rs as constants;
use vm_cow_pages_bindings::VmCowPages as CppVmCowPages;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

::zr::static_assert_size_and_align!(
    PhysicalPageProvider,
    constants::kPhysicalPageProviderStateSize,
    constants::kPhysicalPageProviderStateAlign,
);

define_kcounter!(PHYSICAL_RECLAIM_TOTAL_REQUESTS, "physical.reclaim.total_requests", Sum);
define_kcounter!(PHYSICAL_RECLAIM_SUCCEEDED_REQUESTS, "physical.reclaim.succeeded_requests", Sum);
define_kcounter!(PHYSICAL_RECLAIM_FAILED_REQUESTS, "physical.reclaim.failed_requests", Sum);

/// Page provider implementation that provides requested loaned physical pages.
///
/// This is used by contiguous VMOs which have had pages decommitted, when the pages are again
/// committed. The reason we use a PageProvider for this is it lines up well with the pager model in
/// the sense that a page request can be processed while not holding the contiguous VMO's lock.
///
/// PhysicalPageProvider always operates async (similar to PagerProxy), because we'd like to (in
/// typical non-overlapping commit/decommit usage) have one batch that covers the entire commit,
/// regardless of the fact that some of the pages may already be free and therefore could be
/// immediately obtained. Quite often at least one page will be presently owned by a different
/// VMO, so we may as well always do one big async batch that deals with all the presently
/// non-FREE pages.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct PhysicalPageProvider {
    #[mutex]
    mtx: KMutex<RawMutex>,

    /// Loaned pages should only have their loaned state manipulated by their 'owner', and should
    /// only have their loaned state inspected when it is known to be stable. Due to the
    /// cancellable nature of page requests we generally cannot know when inspecting a page if it
    /// is actually loaned out, or was previously loaned out but this is an old page requests and
    /// we could be racing with it no longer being loaned (or potentially be loaned again). As such
    /// we use this lock to ensure that, whenever we cannot be certain that there is no other actor
    /// that could be manipulating or inspecting the loaned state, we force a serialization.
    #[mutex]
    loaned_state_lock: KMutex<RawMutex>,

    /// Only written once, under `mtx`, just after construction.  Reads after that don't need to be
    /// under `mtx`.
    ///
    /// This is set to the physical base address of the associated VmCowPages.  We track this here
    /// and not in VmCowPages because VmCowPages intentionally doesn't directly know about
    /// contiguous concerns such as `phys_base`.
    ///
    /// The write-once-then-read-unlocked discipline is expressed with an atomic: `init_ptrs`
    /// stores with `Release` and every unlocked read loads with `Acquire`.
    phys_base: AtomicU64,

    /// Set to the total size in bytes of the associated VmCowPages, to allow for asserts that
    /// requests are in range.  While we could ask VmCowPages each time, the `size` doesn't change
    /// for contiguous VMOs, so caching here keeps things simpler (for now).
    size: u64,

    /// Same write-once discipline as `phys_base`.
    ///
    /// The pointee is only guaranteed live while the `VmCowPages` is still attached to us (the
    /// remaining deref sites, `free_pages`, `on_detach` and `wait_on_event`, each document why
    /// that holds). It must never be dereferenced in `drop`: a `PageRequest` can keep our
    /// `PageSource` (and hence us) alive past the `VmCowPages` lifetime
    /// (https://fxbug.dev/565516649).
    cow_pages: AtomicPtr<CppVmCowPages>,

    /// Same write-once discipline as `phys_base`.
    page_source: AtomicPtr<CppPageSource>,

    /// To cease loaning pages all pages first become owned by the physical page provider and get
    /// collected in `free_list`, upon which they can be returned (i.e. freed) to the PMM.
    #[guarded_by(mtx)]
    #[pin]
    free_list: VmPageDoublyLinkedList,

    /// Queue of `PageRequest`s that have been sent to us and not yet processed. `wait_on_event`
    /// drains this queue before waiting on the event it was given.
    #[guarded_by(mtx)]
    #[pin]
    pending_requests: DoublyLinkedList<NonNull<PageRequest>, PageProviderTag>,

    /// Modifications to `detached` are guarded by both our lock and the `cow_pages` lock to provide
    /// us a way to synchronize the processing of requests (which happen in a different thread
    /// context) with becoming detached, as these can otherwise happen concurrently. Through the
    /// use of `detached` the `wait_on_event` logic can ensure that when it calls back into the
    /// `cow_pages` it is not racing with dead transition and potentially supply pages to closed or
    /// closing VMO. See comments in `wait_on_event` and `on_detach` for more.
    #[guarded_by(mtx)]
    detached: bool,

    #[guarded_by(mtx)]
    closed: bool,
}

impl PhysicalPageProvider {
    /// Sentinel value representing an uninitialized `phys_base`.
    const INVALID_PHYS_BASE: u64 = u64::MAX;

    /// In-place initializer for `PhysicalPageProvider`.
    pub fn init(size: u64) -> impl PinInit<Self, core::convert::Infallible> {
        debug::ltracef!("\n");
        pin_init!(Self {
            mtx <- KMutex::init(),
            loaned_state_lock <- KMutex::init(),
            phys_base: AtomicU64::new(Self::INVALID_PHYS_BASE),
            size,
            cow_pages: AtomicPtr::new(core::ptr::null_mut()),
            page_source: AtomicPtr::new(core::ptr::null_mut()),
            free_list <- kcell_init(VmPageDoublyLinkedList::new()),
            pending_requests <- kcell_init(DoublyLinkedList::new()),
            detached: false.into(),
            closed: false.into(),
        })
    }

    /// Returns the static page source properties.
    fn properties(&self) -> PageSourceProperties {
        PageSourceProperties { is_user_pager: false, supports_request_type: [true, false, false] }
    }

    /// Initializes associated pointers and physical base address.
    ///
    /// Called shortly after construction, before any use.
    fn init_ptrs(
        &self,
        cow_pages: *mut CppVmCowPages,
        page_source: *mut CppPageSource,
        phys_base: u64,
    ) {
        debug_assert!(!cow_pages.is_null());
        debug_assert!(!::page::is_aligned(Self::INVALID_PHYS_BASE as usize));
        debug_assert!(::page::is_aligned(phys_base as usize));
        debug_assert!(self.cow_pages.load(Ordering::Acquire).is_null());
        debug_assert_eq!(self.phys_base.load(Ordering::Acquire), Self::INVALID_PHYS_BASE);
        ksync::lock!(let _guard = self.lock_mtx());
        self.cow_pages.store(cow_pages, Ordering::Release);
        self.page_source.store(page_source, Ordering::Release);
        self.phys_base.store(phys_base, Ordering::Release);
    }

    /// Queues an asynchronous read request.
    ///
    /// Called under lock of contiguous VMO that needs the pages.  The request is later processed
    /// at the start of `wait_on_event`.
    fn send_async_request(&self, request: &PageRequest) {
        debug_assert_ne!(self.phys_base.load(Ordering::Acquire), Self::INVALID_PHYS_BASE);
        debug_assert_eq!(request.request_type(), PageRequestType::READ);
        ksync::lock!(let mut guard = self.lock_mtx());
        let fields = guard.as_mut().fields_mut();
        assert!(!*fields.closed);

        // PhysicalPageProvider always operates async (similar to PagerProxy), because we'd like
        // to (in typical non-overlapping commit/decommit usage) have one batch that covers the
        // entire commit, regardless of the fact that some of the pages may already be free and
        // therefore could be immediately obtained.  Quite often at least one page will be
        // presently owned by a different VMO, so we may as well always do one big async batch
        // that deals with all the presently non-FREE_LOANED pages.
        //
        // At this point the page may be FREE_LOANED, or in use by a different VMO. Allocation of
        // a new page to a VMO has an interval during which the page is not free, but also isn't
        // state == OBJECT yet.  During processing we rely on that interval occurring only under
        // the other VMO's lock, but we can't acquire the other VMO's lock here since we're
        // already currently holding the underlying owning contiguous VMO's lock.
        // SAFETY: `PageSource` only sends a request that is not already queued, and it keeps
        // the request alive until it either clears it via `clear_async_request` or the request
        // completes, so it outlives its membership in `pending_requests`.
        unsafe { fields.pending_requests.push_back_raw(NonNull::from(request)) };
    }

    /// Removes a previously queued asynchronous request.
    fn clear_async_request(&self, request: &PageRequest) {
        debug_assert_ne!(self.phys_base.load(Ordering::Acquire), Self::INVALID_PHYS_BASE);
        debug_assert_eq!(request.request_type(), PageRequestType::READ);
        ksync::lock!(let mut guard = self.lock_mtx());
        let fields = guard.as_mut().fields_mut();
        assert!(!*fields.closed);
        if request.in_container() {
            // SAFETY: `PageSource` routes a request back to the same provider that queued it, so
            // a request in a `PageProviderTag` list is in this `pending_requests`.
            let _ = unsafe { fields.pending_requests.erase(request) };
        }

        // No need to chase down any currently-processing request here, since before processing
        // a request, we stash the values of all fields we need from the PageRequest under the
        // lock.  So any currently-processing request is independent from the PageRequest that
        // started it.
    }

    /// Swaps an old request node with a new request node in the pending queue.
    fn swap_async_request(&self, old: &PageRequest, new_req: &PageRequest) {
        debug_assert_ne!(self.phys_base.load(Ordering::Acquire), Self::INVALID_PHYS_BASE);
        debug_assert_eq!(old.request_type(), PageRequestType::READ);
        debug_assert_eq!(new_req.request_type(), PageRequestType::READ);
        ksync::lock!(let mut guard = self.lock_mtx());
        let fields = guard.as_mut().fields_mut();
        assert!(!*fields.closed);
        if old.in_container() {
            let mut pending_requests = fields.pending_requests;
            // SAFETY: `PageSource` routes both requests to the provider that queued `old`, so
            // `old` is only ever in this `pending_requests`, and `new_req` is a fresh request
            // that is not yet in any `PageProviderTag` list and outlives its membership here.
            unsafe {
                pending_requests.as_mut().cursor_at(old).insert_before_raw(NonNull::from(new_req));
                let _ = pending_requests.erase(old);
            }
        }
    }

    /// Frees pages back to PMM or queues them into `free_list` if detached.
    fn free_pages(&self, mut pages: Pin<&mut VmPageDoublyLinkedList>) {
        {
            // Check if we are detached, and if so put the pages straight in the `free_list` instead
            // of loaning them out, since we will be being closed soon and will only have to go and
            // retrieve them.
            // Also check for the case where we're not fully initialized yet. It's possible to fail
            // partway through inserting pages during creation, which will require freeing any
            // already inserted pages back to the PMM.
            ksync::lock!(let mut guard = self.lock_mtx());
            let fields = guard.as_mut().fields_mut();
            if *fields.detached || self.phys_base.load(Ordering::Acquire) == Self::INVALID_PHYS_BASE
            {
                fields.free_list.splice(pages.as_mut());
                return;
            }
        }
        // This should always be called in a way that is serialized with other operations on our
        // cow_pages, otherwise there is a race where an operation on the cow_pages could observe an
        // absence of pages, but then be unable to retrieve them because it is not synchronized with
        // this free_pages call. Both parties using the paged_vmo_lock serves to create a
        // synchronization that avoids this scenario.
        let page_source = self.page_source.load(Ordering::Acquire);
        assert!(!page_source.is_null());
        // SAFETY: `page_source` is written exactly once by `init_ptrs` and never modified
        // afterwards, and the `PageSource` that owns this provider outlives it.
        let page_source = unsafe { PageSource::from_raw_ref(page_source) };
        debug_assert!(page_source.paged_vmo_lock_is_held());
        // This marks the pages loaned, and makes them FREE_LOANED for potential use by other
        // clients that are ok with getting loaned pages when allocating. Must hold the
        // `loaned_state_lock` as we are manipulating the loaned state of pages that could get
        // inspected by `unloan_range` due to interactions with cancelled page requests.
        ksync::lock!(let _guard = self.lock_loaned_state_lock());
        let cow_pages = self.cow_pages.load(Ordering::Acquire);
        let delay_reuse = if cow_pages.is_null() {
            PmmOptDelayReuse::Default
        } else {
            // SAFETY: `cow_pages` is written exactly once by `init_ptrs` and never modified
            // afterwards, and it is non-null here because `phys_base` has been initialized.
            // `free_pages` is only ever reached from the owning `VmCowPages` (via its
            // `PageSource`), and, as documented above, that caller serializes with us on the
            // `paged_vmo_lock`, so the pointee is live for the duration of this call.
            //
            // Note that nothing enforces that invariant: we hold no reference on the pointee
            // and nothing clears `cow_pages` when the `VmCowPages` is destroyed.
            unsafe { VmCowPages::from_raw_ref(cow_pages) }.should_delay_reuse_on_free()
        };
        // SAFETY: `pages` contains valid allocated pages that we own.
        unsafe {
            pmm::node().begin_loan(pages, delay_reuse);
        }
    }

    /// Validates that `page` matches `phys_base + offset` for debugging.
    fn debug_is_page_ok(&self, page: *mut vm_page_t, offset: u64) -> bool {
        ksync::lock!(let _guard = self.lock_mtx());
        let cow_pages = self.cow_pages.load(Ordering::Acquire);
        let phys_base = self.phys_base.load(Ordering::Acquire);
        debug_assert_eq!(!cow_pages.is_null(), phys_base != Self::INVALID_PHYS_BASE);
        // Assume pages added before we know the `cow_pages` or `phys_base` are ok.
        if cow_pages.is_null() {
            return true;
        }
        // SAFETY: `page` must be a valid pointer; the original C++ code derefs
        // directly without validation.
        let page_ptr = unsafe { VmPagePtr::from_ffi(page) }.expect("valid page pointer");
        // SAFETY: `page_ptr` is a valid allocated page.
        let paddr = unsafe { page_ptr.paddr().0 } as u64;
        paddr.checked_sub(phys_base) == Some(offset)
    }

    /// Called when the provider is detached from `VmCowPages`.
    fn on_detach(&self) {
        // It's possible for destruction to happen prior to initialization completing, and this can
        // lead to `on_detach` being called from what would be the `cow_pages`, but prior to
        // `init_ptrs` being called and the `cow_pages` pointer being setup. In this case we can
        // safely ignore locking requirements and just set detached.
        ksync::lock!(let mut guard = self.lock_mtx());
        let fields = guard.as_mut().fields_mut();
        assert!(!*fields.closed);
        if self.phys_base.load(Ordering::Acquire) == Self::INVALID_PHYS_BASE {
            assert!(self.cow_pages.load(Ordering::Acquire).is_null());
            *fields.detached = true;
        } else {
            // The current synchronization strategy relies on `on_detach` being called with the VMO
            // lock being held. This allows us to assume that the `detached` flag only transitions
            // under the `VmCowPages` lock.
            // SAFETY: `cow_pages` is written exactly once by `init_ptrs` and never modified
            // afterwards, and it is non-null here because `phys_base` has been initialized. The
            // owning `VmCowPages` is what calls `on_detach`, so the pointee is live.
            let cow_pages =
                unsafe { VmCowPages::from_raw_ref(self.cow_pages.load(Ordering::Acquire)) };
            debug_assert!(cow_pages.lock_is_held());
            *fields.detached = true;
        }
    }

    /// Called when the provider is closed.
    fn on_close(&self) {
        ksync::lock!(let mut guard = self.lock_mtx());
        let fields = guard.as_mut().fields_mut();
        assert!(!*fields.closed);
        *fields.closed = true;
    }

    /// Dequeues the next pending request from the queue, returning (offset, length).
    ///
    /// If there are no more requests in `pending_requests`, returns `None`.
    ///
    /// If `pending_requests` has another request, returns `Some((request_offset,
    /// request_length))`, where `request_offset` is the offset of that request and
    /// `request_length` is its length.
    fn dequeue_request(&self) -> Option<(u64, u64)> {
        ksync::lock!(let mut guard = self.lock_mtx());
        let fields = guard.as_mut().fields_mut();
        // `closed` can be true here, but if `closed` is true, then `pending_requests` is also
        // empty, so we won't process any more requests once `closed` is true. However, there is
        // also no point in processing requests if we have detached, as these requests will be
        // cancelled anyhow.
        debug_assert!(!*fields.closed || fields.pending_requests.is_empty());
        if fields.pending_requests.is_empty() || *fields.detached {
            // Done with all requests (or remaining requests cancelled).
            return None;
        }
        let request = fields.pending_requests.pop_front();
        debug_assert!(request.is_some());
        // SAFETY: We just checked there is a pending request.
        let request = unsafe { request.unwrap_unchecked() };
        // SAFETY: `pop_front` yields a request that was queued by `send_async_request` and that
        // C++ keeps alive at least until it is cleared from the queue, which cannot happen while
        // we hold `mtx`.
        let request = unsafe { request.as_ref() };
        debug_assert_eq!(request.request_type(), PageRequestType::READ);
        let offset = request.offset();
        let length = request.len();
        debug_assert!(range_check::in_range_max(offset, length, self.size));
        Some((offset, length))
    }

    /// Helper that unloans any loaned pages in the specified range and puts them, in offset
    /// order, in `pages`. Any pages that are not loaned will not be returned, and the caller must
    /// tolerate gaps in the list.
    fn unloan_range(
        &self,
        range_offset: u64,
        length: u64,
        mut pages: Pin<&mut VmPageDoublyLinkedList>,
    ) {
        // Notify borrowing config that an unloan is starting. This temporarily disables background
        // page borrowing/sweeping in ProcessLruQueue to avoid lock contention on the borrowing
        // VMOs' `paged_vmo_lock` (which are part of the active working set) between the LRU thread
        // and this unloaning thread.
        PhysicalPageBorrowingConfig::get().notify_unloan_started();
        let _cleanup = zr::defer(|| PhysicalPageBorrowingConfig::get().notify_unloan_finished());

        let phys_base = self.phys_base.load(Ordering::Acquire);
        debug_assert_ne!(phys_base, Self::INVALID_PHYS_BASE);

        ksync::lock!(let mut loaned_guard = self.lock_loaned_state_lock());

        // Evict needed physical pages from other VMOs, so that needed physical pages become free.
        // This is iterating over the destination offset in `cow_pages`.  The needed pages can be
        // scattered around in various VMOs and offsets of those VMOs and, by the time we get to
        // looking at them, could even already be returned to the PMM and free.
        let range_end = range_offset.wrapping_add(length);
        let mut offset = range_offset;
        while offset < range_end {
            let paddr = PAddr((phys_base + offset) as usize);
            offset += ::page::SIZE as u64;

            let page_ptr = pmm::paddr_to_vm_page(paddr);
            debug_assert!(page_ptr.is_some());
            let page_ptr = page_ptr.unwrap();
            // Page should never have entered the regular FREE state as it should either be
            // loaned out in a VMO, in the FREE_LOANED state, or owned by us in our VmCowPages.
            // SAFETY: `page_ptr` is a valid page whose state we are serialized against by the
            // `loaned_state_lock`.
            debug_assert!(!unsafe { page_ptr.is_free() });

            // If the page is not currently loaned out, then skip, our work here is done.
            // SAFETY: `page_ptr` is a valid page whose loaned state we hold the
            // `loaned_state_lock` for.
            if !unsafe { page_ptr.is_loaned() } {
                continue;
            }

            // SAFETY: `page_ptr` is a valid allocated page.
            unsafe {
                // Cancel the loan allowing us to track the page down.
                pmm::node().cancel_loan(page_ptr);
            }

            // Cancelling the loan took the loaned pages lock and so just prior to that
            // completing we knew that every page was either:
            //  1. Found in the LOANED_FREE state, and is therefore still in that state.
            //  2. Completely installed in a VMO with a valid backlink.
            // SAFETY: `page_ptr` is a valid page.
            if !unsafe { page_ptr.is_free_loaned() } {
                // Between cancelling the loan and now, the page could be in the progress of
                // migrating back to the PMM. If we just perform `get_cow_for_loaned_page` then we
                // could observe a scenario where the page is still in the OBJECT state, but
                // has its backlink cleared. To avoid this we perform the lookup under the
                // loaned pages lock, ensuring we either see the page while it is still in the
                // VMO, with a valid backlink, or after it has fully migrated back to the PMM.
                let mut maybe_vmo_backlink = None;
                pmm::node().with_loaned_page(page_ptr, |page| {
                    // SAFETY: `page` is held in the loaned state by `with_loaned_page`, and we
                    // just cancelled its loan, so its state is not changing under us.
                    maybe_vmo_backlink =
                        unsafe { pmm::page_queues().get_cow_for_loaned_page(page) };
                });
                // Either we got a backlink, or the page is already back in the PMM.
                // SAFETY: `page_ptr` is a valid page.
                debug_assert!(maybe_vmo_backlink.is_some() || unsafe { page_ptr.is_free_loaned() });
                if let Some(vmo_backlink) = maybe_vmo_backlink {
                    let backlink_offset = vmo_backlink.offset;
                    // A backlink for a loaned page always carries a cow.
                    let cow_container = vmo_backlink.cow.expect("backlink without a cow");
                    // As we will be calling back into the VMO we want to drop the loaned state
                    // lock to both avoid lock ordering issues, and to not excessively hold the
                    // lock. During this section we are not modifying or inspecting the loaned
                    // state so the lock is not needed. The cow page we call may inspect the loaned
                    // state, but it does so iff it knows it is the owner and so it may safely do
                    // so.
                    loaned_guard.as_mut().call_unlocked(|| {
                        // SAFETY: `page_ptr` is a valid object-associated page.
                        debug_assert!(!unsafe { page_ptr.is_always_need() });
                        let mut needs_evict = true;

                        // Check if we should attempt to replace the page to avoid eviction.
                        if PhysicalPageBorrowingConfig::get().is_replace_on_unloan_enabled() {
                            stack_pin_init!(let page_request = AnonymousPageRequest::new());
                            let replace_result = cow_container.replace_page(
                                page_ptr,
                                backlink_offset,
                                false,
                                Some(page_request.as_mut()),
                            );
                            // If replacement failed for any reason, fall back to eviction. If
                            // replacement succeeded then the page got directly returned to the
                            // pmm.
                            needs_evict = replace_result.is_err();
                            if replace_result == Err(Status::SHOULD_WAIT) {
                                page_request.as_mut().cancel();
                            }
                            // If replacement succeeded, i.e. we are not going to fall back to
                            // eviction, then the page should be back in the PMM.
                            // SAFETY: `page_ptr` is a valid page.
                            debug_assert!(needs_evict || unsafe { page_ptr.is_free_loaned() });
                        }
                        if needs_evict {
                            // SAFETY: `page_ptr` is an object-associated page of `cow_container`,
                            // as we just looked it up via its backlink.
                            let reclaimed = unsafe {
                                cow_container.evict_loaned_page(page_ptr, backlink_offset)
                            };
                            // Either we succeeded eviction, or another thread raced and did it
                            // first. If another thread did it first then it would have done so
                            // under the VMO lock, which we have since acquired, and so we know
                            // the page is either on the way (in a FreeLoanedPagesHolder) or in
                            // the PMM. We can ensure the page is fully migrated to the PMM by
                            // waiting for any holding to be concluded.
                            // SAFETY: `page_ptr` is a valid page.
                            debug_assert!(
                                reclaimed.is_err() || unsafe { page_ptr.is_free_loaned() }
                            );
                            // SAFETY: `page_ptr` is a valid page.
                            if !unsafe { page_ptr.is_free_loaned() } {
                                // The empty closure is deliberate: this synchronizes with any
                                // pending free holder without inspecting the page.
                                pmm::node().with_loaned_page(page_ptr, |_| {});
                                // SAFETY: `page_ptr` is a valid page.
                                debug_assert!(unsafe { page_ptr.is_free_loaned() });
                            }
                        }
                    });
                }
                // For all the scenarios, no backlink, successful replacement or eviction attempts,
                // the page must have ended up in the PMM.
                // SAFETY: `page_ptr` is a valid page.
                assert!(unsafe { page_ptr.is_free_loaned() });
            }

            // Now that the page is definitely in the FREE_LOANED state, gain ownership from the
            // PMM.
            // SAFETY: `page_ptr` is in FREE_LOANED state and gaining ownership from PMM.
            unsafe {
                pmm::node().end_loan(page_ptr);
            }
            debug_assert_eq!(page_ptr.state(), VmPageState(vm_page_state::ALLOC));
            // SAFETY: `page_ptr` is a valid allocated page pointer that we now own, and the page
            // outlives its membership in `pages`.
            unsafe { pages.as_mut().push_back_raw(page_ptr.as_non_null()) };
        } // for pages of request
    }

    /// Processes all queued page requests and then waits on `event`.
    ///
    /// Before actually waiting on the event, uses the calling thread (which isn't holding any
    /// locks) to process all the requests in `pending_requests`.
    // TODO(https://fxbug.dev/42084841): Reason about the use of `suspendable`, ignored for now.
    fn wait_on_event(&self, event: &Event, _suspendable: bool) -> Result<(), Status> {
        // Before processing any events we synchronize with `on_detach` and retrieve a reference to
        // the `cow_pages`. This ensures we can safely dereference the `cow_pages` later on,
        // knowing we are not racing with its destructor.
        let cow_ref = {
            ksync::lock!(let guard = self.lock_mtx());
            if *guard.fields().detached {
                return Ok(());
            }
            let cow_pages = self.cow_pages.load(Ordering::Acquire);
            assert!(!cow_pages.is_null());
            // SAFETY: `cow_pages` points to a live `VmCowPages` while we hold `mtx`.
            let cow = unsafe { VmCowPages::upgrade_from_raw(cow_pages) };
            // As we hold the lock, and had not yet detached, the `cow_pages` must not have run its
            // dead transition, and so it must still be possible to get a ref to it.
            cow.expect("cow_pages is live but could not be upgraded")
        };
        // SAFETY: `page_source` points to the `PageSource` that owns this provider, and so
        // outlives it.
        let page_source =
            unsafe { PageSource::from_raw_ref(self.page_source.load(Ordering::Acquire)) };
        let phys_base = self.phys_base.load(Ordering::Acquire);

        // When `wait_on_event` is called, we know that the event being waited on is associated
        // with a request that's already been queued, so we can use this thread to process _all_
        // the queued requests first, and then wait on the event which then won't have any reason
        // to block this thread, since every page of every request that existed on entry to this
        // method has been succeeded or failed by the time we wait on the passed-in event.
        while let Some((request_offset, request_length)) = self.dequeue_request() {
            debug_assert!(
                request_offset.checked_add(request_length).is_some_and(|end| end > request_offset)
            );
            stack_pin_init!(let unloaned_pages = VmPageDoublyLinkedList::new());
            // These are ordered by `cow_pages` offsets (destination offsets), but may have gaps
            // due to not all the pages being loaned.
            self.unloan_range(request_offset, request_length, unloaned_pages.as_mut());

            stack_pin_init!(let contiguous_pages = VmPageDoublyLinkedList::new());
            // Process all the loaned pages by finding any contiguous runs and processing those as
            // a batch. There is no correctness reason to process them in batches, but it is more
            // efficient for the `cow_pages` to receive a run where possible instead of repeatedly
            // being given single pages.
            while !unloaned_pages.is_empty() {
                let next_paddr = unloaned_pages.front().unwrap().paddr().0;
                if contiguous_pages.is_empty()
                    || (contiguous_pages.back().unwrap().paddr().0 + ::page::SIZE) == next_paddr
                {
                    let page = unloaned_pages.as_mut().pop_front().unwrap();
                    // SAFETY: `page` is a valid page popped from `unloaned_pages`.
                    unsafe { contiguous_pages.as_mut().push_back_raw(page) };
                    // Generally want to keep trying to find more contiguous pages, unless there
                    // are no more pages.
                    if !unloaned_pages.is_empty() {
                        continue;
                    }
                }

                // An interfering decommit can occur after we've moved these pages into VmCowPages,
                // but not yet moved the entire commit request into VmCowPages.  If not all pages
                // end up present in `cow_pages` on return to the user from the present commit, due
                // to concurrent decommit, that's just normal commit semantics.
                //
                // Supply the pages we got to `cow_pages`.  Also tell it what range to claim is
                // supplied now for convenience.
                //
                // If there's an interfering decommit, then that decommit can only interfere after
                // we've added the pages to VmCowPages, so isn't an immediate concern here.
                //
                // We want to use `VmCowPages::supply_pages` to avoid a proliferation of VmCowPages
                // code that calls `on_pages_supplied` / `on_pages_failed`, so to call
                // `supply_pages` we need a VmPageSpliceList.  We put all the pages in the "head"
                // portion of the VmPageSpliceList since there are no VmPageListNode(s) involved in
                // this path.  We also zero the pages here, since `supply_pages` doesn't do that.
                //
                // We can zero the pages before we supply them, which avoids holding the
                // `VmCowPages::lock` while zeroing, and also allows us to flush the zeroes to RAM
                // here just in case any client is (incorrectly) assuming that non-pinned pages
                // necessarily remain cache clean once they are cache clean.
                let mut supply_length = 0u64;
                for p in contiguous_pages.iter() {
                    let vaddr = physmap::paddr_to_physmap(p.paddr());
                    debug_assert_ne!(vaddr.0, 0);
                    // SAFETY: `vaddr` points to valid physical memory mapped in physmap.
                    unsafe {
                        crate::arch_rs::ops::zero_page(vaddr);
                        crate::arch_rs::cache::arch_clean_invalidate_cache_range(
                            vaddr.0,
                            ::page::SIZE,
                        );
                    }
                    supply_length += ::page::SIZE as u64;
                }
                let first_paddr = contiguous_pages.front().unwrap().paddr().0 as u64;
                let supply_offset = first_paddr - phys_base;

                stack_pin_init!(let splice_list = VmPageSpliceList::new());

                // Any pages that do not get supplied for any reason, due to failures from supply
                // or because we got detached, should be re-loaned.
                struct ReclaimUnprocessedGuard<'a> {
                    splice_list: Pin<&'a mut VmPageSpliceList>,
                    contiguous_pages: Pin<&'a mut VmPageDoublyLinkedList>,
                    provider: &'a PhysicalPageProvider,
                }
                impl Drop for ReclaimUnprocessedGuard<'_> {
                    fn drop(&mut self) {
                        // Pop all remaining content off of this splice list, appending any pages
                        // to `contiguous_pages`.
                        while !self.splice_list.is_processed() {
                            let mut page_or_marker = self.splice_list.as_mut().pop();
                            if page_or_marker.is_page() {
                                let p = page_or_marker.release_page();
                                // SAFETY: `p` was just released from the splice list, which had
                                // sole ownership of it, so it is a valid page that nothing else
                                // references.
                                let page = unsafe { p.as_ref() };
                                debug_assert!(!page.get_node().in_container());
                                // SAFETY: `p` is a valid page that is not in any container, and
                                // the pages backing `VmPage`s outlive their membership in
                                // `self.contiguous_pages`.
                                unsafe {
                                    self.contiguous_pages.as_mut().push_back_raw(p.as_non_null())
                                };
                            }
                        }

                        if !self.contiguous_pages.is_empty() {
                            ksync::lock!(let _guard = self.provider.lock_loaned_state_lock());
                            // We're about to return these pages to the PMM.  There is no need to
                            // delay their reuse because we know these pages would have never been
                            // involved in a DMA, because they have never been pinned, because we
                            // don't pin loaned pages.  See
                            // `VmCowPages::replace_pages_with_non_loaned_locked`.
                            let delay_reuse = PmmOptDelayReuse::Default;
                            // SAFETY: The pages in `contiguous_pages` are valid and owned by us.
                            unsafe {
                                pmm::node().begin_loan(self.contiguous_pages.as_mut(), delay_reuse);
                            }
                        }
                    }
                }

                let mut guard = ReclaimUnprocessedGuard {
                    splice_list: splice_list.as_mut(),
                    contiguous_pages: contiguous_pages.as_mut(),
                    provider: self,
                };

                let ReclaimUnprocessedGuard { splice_list, contiguous_pages, .. } = &mut guard;
                let status = VmPageSpliceList::create_from_page_list(
                    supply_length,
                    contiguous_pages.as_mut(),
                    splice_list.as_mut(),
                );
                if let Err(status) = status {
                    // Only possible error is out of memory.
                    assert_eq!(status, Status::NO_MEMORY);
                    debug_assert!(PageSource::is_valid_internal_failure_code(status));
                    page_source.on_pages_failed(supply_offset, supply_length, status);
                    // Do not attempt to then supply the pages, move to the next range.
                    continue;
                }
                debug_assert!(guard.contiguous_pages.is_empty());

                // First take the VMO lock before taking our lock to ensure lock ordering is
                // correct. As we hold a reference we know that even if racing with `on_close` this
                // is a valid object.
                //
                // `deferred` is declared before the lock guard so that it is destructed after the
                // guard releases the lock, which is what lets it perform its deferred actions
                // unlocked.
                stack_pin_init!(let deferred = DeferredOps::new(&cow_ref));
                {
                    ksync::lock!(let cow_lock = cow_ref.lock());
                    // Now take our lock and check to see if we have been detached.
                    let detached = {
                        ksync::lock!(let provider_guard = self.lock_mtx());
                        *provider_guard.fields().detached
                    };
                    // We can use the cached value of `detached` since `on_detach` only runs with
                    // the VMO lock held, which we presently hold. This means that if detached is
                    // false we can safely call VMO methods knowing that we are not racing with any
                    // detach or close attempts.
                    if !detached {
                        // The splice list being inserted only contains true `VmPage`s, so we
                        // don't need to call `process_pages_for_supply` to dereference them.
                        let supply_result = cow_ref.supply_pages_locked(
                            cow_lock.token(),
                            VmCowRange::new(supply_offset, supply_length),
                            guard.splice_list.as_mut(),
                            SupplyOptions::PhysicalPageProvider,
                            deferred.as_mut(),
                            None,
                        );
                        if let Err(supply_result) = supply_result {
                            // Supply can only fail due to being out of memory as we currently
                            // hold the lock and know that it cannot be racing with a detach for
                            // close.
                            debug_assert_eq!(supply_result, Status::NO_MEMORY);
                            debug_assert!(PageSource::is_valid_internal_failure_code(
                                supply_result
                            ));
                            page_source.on_pages_failed(
                                supply_offset,
                                supply_length,
                                supply_result,
                            );
                        }
                    }
                }
            }
            debug_assert!(contiguous_pages.is_empty() && unloaned_pages.is_empty());
        } // while have requests to process

        PHYSICAL_RECLAIM_TOTAL_REQUESTS.add(1);
        // The event now should be in one of three states:
        //  1. We processed the related request above in the loop, and the event got signaled as a
        //     consequence of the pages we supplied.
        //  2. A different thread dequeued the request and either processed it, or is still
        //     processing it.
        //  3. The request is in the process of being cancelled and the underlying request packet
        //     got dequeued, and the event has or will be signaled.
        // In all cases it is a *kernel* thread under our control that should signal the thread,
        // and so although we may need to wait, only a kernel bug should cause this to block
        // indefinitely.
        // To attempt to detect such bugs we wait with a generous timeout before making some noise.
        const REPORT_WAIT_TIME: DurationMono = DurationMono::from_seconds(60);
        let mut waited: u32 = 0;
        let wait_result = loop {
            let deadline = Deadline::after_mono(REPORT_WAIT_TIME, TimerSlack::none());
            match event.wait(&deadline) {
                Err(Status::TIMED_OUT) => {
                    waited += 1;
                    kprint::kprintln!(
                        "WARNING: PhysicalPageProvider has waited {} seconds on event.",
                        (REPORT_WAIT_TIME * (waited as i64)).into_seconds()
                    );
                }
                res => break res,
            }
        };
        if wait_result.is_ok() {
            PHYSICAL_RECLAIM_SUCCEEDED_REQUESTS.add(1);
        } else {
            PHYSICAL_RECLAIM_FAILED_REQUESTS.add(1);
        }
        wait_result
    }

    /// Dumps debug state using `DepthPrinter`.
    ///
    /// `provider_ptr` is the address of the C++ `PhysicalPageProvider` that owns this state, which
    /// is what the dump identifies the provider by.
    fn dump(&self, provider_ptr: *const core::ffi::c_void, depth: u32, max_items: u32) {
        ksync::lock!(let guard = self.lock_mtx());
        let fields = guard.fields();
        let mut printer = dump::DepthPrinter::console(depth as usize);
        dump::emit!(
            printer,
            "physical_page_provider {:p} cow_pages_ {:p} phys_base_ 0x{:x} closed {}",
            provider_ptr,
            self.cow_pages.load(Ordering::Acquire),
            self.phys_base.load(Ordering::Acquire),
            if *fields.closed { 1 } else { 0 }
        );
        printer.begin_list(max_items as usize);
        for req in fields.pending_requests.iter() {
            debug_assert_eq!(req.request_type(), PageRequestType::READ);
            dump::emit!(printer, "  pending req [0x{:x}, 0x{:x})", req.offset(), req.len());
        }
        printer.end_list();
    }
}

#[pinned_drop]
impl PinnedDrop for PhysicalPageProvider {
    fn drop(self: Pin<&mut Self>) {
        debug::ltracef!("PhysicalPageProvider::drop: {:p}\n", self.as_ref().get_ref());
        let mut this = self;
        let phys_base = *this.as_mut().project().phys_base.get_mut();
        // Possible we were destructed without being initialized and in error paths we can destruct
        // without `detached` or `closed` becoming true, so cannot check for those.
        if phys_base == Self::INVALID_PHYS_BASE {
            let free_list = this.project().free_list.get_pinned_mut();
            // If a failure is encountered during creation, we might have accumulated some pages
            // which need to be returned to the PMM. Initialization isn't complete and these pages
            // haven't been used yet, so no need to unloan and no need to delay reuse either.
            if !free_list.is_empty() {
                core::hint::cold_path();
                // SAFETY: Returning uninitialized creation pages to PMM.
                unsafe {
                    pmm::node().free_list(free_list, PmmOptDelayReuse::Default);
                }
            }
            return;
        }

        stack_pin_init!(let free_list = VmPageDoublyLinkedList::new());
        // Move the cached pages into a locally pinned list to avoid borrow checker conflicts when
        // passing the free_list into unloan_range.
        free_list.as_mut().splice(this.as_mut().project().free_list.get_pinned_mut());

        // To return our pages first retrieve any pages that are loaned, before returning
        // everything to the PMM. This is inefficient as instead of retrieving the loaned page
        // only to put it back in the PMM we could unloan the page 'in place' where it is, however
        // contiguous VMOs that are loaning pages are not expected to be destructed often (or at
        // all) and so this is not presently a needed optimization.
        this.unloan_range(0, this.size, free_list.as_mut());
        assert_eq!(free_list.iter().count() as u64, this.size / (::page::SIZE as u64));

        // The `VmCowPages` may already have been destroyed, as a `PageRequest` can hold a reference
        // to our `PageSource` (and hence to us) beyond the `VmCowPages` lifetime, so `cow_pages`
        // must not be dereferenced here (https://fxbug.dev/565516649). Unconditionally request
        // delayed reuse: contiguous VMOs are used for DMA in practice, so `ever_pinned_` is almost
        // certainly set anyway.
        // SAFETY: Returning loaned pages to PMM.
        unsafe {
            pmm::node().free_list(free_list, PmmOptDelayReuse::Yes);
        }
    }
}

// C FFI exports

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_init(
    storage: *mut core::ffi::c_void,
    size: u64,
) {
    let slot = storage.cast::<PhysicalPageProvider>();
    let init = PhysicalPageProvider::init(size);
    // SAFETY: `slot` is valid memory provided by C++ `PhysicalPageProvider::state_`.
    let _ = unsafe { init.__pinned_init(slot) };
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_destroy(storage: *mut core::ffi::c_void) {
    let slot = storage.cast::<PhysicalPageProvider>();
    // SAFETY: `slot` points to an initialized `PhysicalPageProvider`.
    unsafe {
        core::ptr::drop_in_place(slot);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_init_ptrs(
    storage: *mut core::ffi::c_void,
    cow_pages: *mut CppVmCowPages,
    page_source: *mut CppPageSource,
    phys_base: u64,
) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.init_ptrs(cow_pages, page_source, phys_base);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_properties(
    storage: *const core::ffi::c_void,
) -> PageSourceProperties {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.properties()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_send_async_request(
    storage: *mut core::ffi::c_void,
    request: &PageRequest,
) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.send_async_request(request);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_clear_async_request(
    storage: *mut core::ffi::c_void,
    request: &PageRequest,
) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.clear_async_request(request);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_swap_async_request(
    storage: *mut core::ffi::c_void,
    old_req: &PageRequest,
    new_req: &PageRequest,
) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.swap_async_request(old_req, new_req);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_free_pages(
    storage: *mut core::ffi::c_void,
    pages: &mut VmPageDoublyLinkedList,
) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    // SAFETY: `pages` is pinned by the C++ caller, which never moves the list.
    let pages = unsafe { Pin::new_unchecked(pages) };
    state.free_pages(pages);
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_debug_is_page_ok(
    storage: *const core::ffi::c_void,
    page: *mut vm_page_t,
    offset: u64,
) -> bool {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.debug_is_page_ok(page, offset)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_on_detach(storage: *mut core::ffi::c_void) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.on_detach();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_on_close(storage: *mut core::ffi::c_void) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.on_close();
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_wait_on_event(
    storage: *mut core::ffi::c_void,
    event: &Event,
    suspendable: bool,
) -> zx_types::zx_status_t {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    Status::result_into_raw(state.wait_on_event(event, suspendable))
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_physical_page_provider_dump(
    storage: *const core::ffi::c_void,
    provider_ptr: *const core::ffi::c_void,
    depth: u32,
    max_items: u32,
) {
    // SAFETY: `storage` points to an initialized `PhysicalPageProvider`.
    let state = unsafe { &*(storage as *const PhysicalPageProvider) };
    state.dump(provider_ptr, depth, max_items);
}

#[cfg(ktest)]
/// Unit tests for `PhysicalPageProvider`.
#[unittest::suite(name = "physical_page_provider")]
mod tests {
    use super::PhysicalPageProvider;
    use pin_init::stack_pin_init;
    use unittest::{expect_false, expect_true};

    /// Tests that properties returns non-user pager and read-only request support.
    #[test]
    fn test_properties() {
        stack_pin_init!(let provider = PhysicalPageProvider::init(4096));
        let props = provider.properties();
        expect_false!(props.is_user_pager);
        expect_true!(props.supports_request_type[0]);
        expect_false!(props.supports_request_type[1]);
        expect_false!(props.supports_request_type[2]);
    }

    /// Tests that initialization and page validation work properly.
    #[test]
    fn test_init_and_debug_page_ok() {
        stack_pin_init!(let provider = PhysicalPageProvider::init(8192));
        expect_true!(provider.debug_is_page_ok(core::ptr::null_mut(), 0));
    }

    /// Tests that on_detach and on_close succeed on uninitialized provider.
    #[test]
    fn test_detach_and_close() {
        stack_pin_init!(let provider = PhysicalPageProvider::init(4096));
        provider.on_detach();
        provider.on_close();
    }
}
