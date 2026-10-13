// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod page_cache_ffi;

use crate::arch_rs::{CpuAlignMarker, MAX_CACHE_LINE, curr_cpu_num};
use crate::counters::define_kcounter;
use crate::kernel::percpu::PerCpu;
use crate::kernel::thread::{AutoPreemptDisabler, current_memory_allocation_state_is_enabled};
use crate::vm::page::VmPageDoublyLinkedList;
use crate::vm::page_state::VmPageState;
use crate::vm::page_state::bindings::vm_page_state;
use crate::vm::pmm::{self, ALLOC_FLAG_CAN_WAIT};
use core::ffi::c_int;
use core::ops::Deref;
use core::pin::Pin;
use ksync::{KCell, KMutex, RawMutex};
use page_cache_ffi::{cpp_global_prng_draw, rand_r};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop, stack_pin_init};
use zx_status::Status;

const TRACE_ENABLED: bool = false;
const RAND_MAX: c_int = 0x7fff_ffff;

define_kcounter!(PAGE_CACHE_HIT_PAGES, "cache.page.hit", Sum);
define_kcounter!(PAGE_CACHE_MISS_PAGES, "cache.page.missed", Sum);
define_kcounter!(PAGE_CACHE_REFILL_PAGES, "cache.page.refilled", Sum);
define_kcounter!(PAGE_CACHE_RETURN_PAGES, "cache.page.returned", Sum);
define_kcounter!(PAGE_CACHE_FREE_PAGES, "cache.page.freed", Sum);

fn count_hit_pages(page_count: usize) {
    PAGE_CACHE_HIT_PAGES.add(page_count as i64);
}

fn count_miss_pages(page_count: usize) {
    PAGE_CACHE_MISS_PAGES.add(page_count as i64);
}

fn count_refill_pages(page_count: usize) {
    PAGE_CACHE_REFILL_PAGES.add(page_count as i64);
}

fn count_return_pages(page_count: usize) {
    PAGE_CACHE_RETURN_PAGES.add(page_count as i64);
}

fn count_free_pages(page_count: usize) {
    PAGE_CACHE_FREE_PAGES.add(page_count as i64);
}

/// An RAII wrapper around `VmPageDoublyLinkedList` that automatically frees remaining pages
/// back to the PMM upon destruction.
#[repr(transparent)]
#[pin_data(PinnedDrop)]
pub struct PageList {
    #[pin]
    pages: VmPageDoublyLinkedList,
}

// SAFETY: `PageList` owns the `VmPage` nodes linked within it and can be safely
// transferred across threads.
unsafe impl Send for PageList {}

impl PageList {
    /// Creates an in-place initializer for a new, empty `PageList`.
    pub fn new() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            pages <- VmPageDoublyLinkedList::new(),
        })
    }

    /// Returns a shared reference to the underlying `VmPageDoublyLinkedList`.
    pub fn list(&self) -> &VmPageDoublyLinkedList {
        &self.pages
    }

    /// Returns a pinned mutable reference to the underlying `VmPageDoublyLinkedList`.
    pub fn list_mut(self: Pin<&mut Self>) -> Pin<&mut VmPageDoublyLinkedList> {
        self.project().pages
    }
}

impl Deref for PageList {
    type Target = VmPageDoublyLinkedList;

    fn deref(&self) -> &Self::Target {
        &self.pages
    }
}

#[pinned_drop]
impl PinnedDrop for PageList {
    fn drop(self: Pin<&mut Self>) {
        if !self.pages.is_empty() {
            // SAFETY: Pages in `self.pages` are valid allocated PMM pages that have not
            // already been freed.
            unsafe {
                pmm::free_list(self.project().pages);
            }
        }
    }
}

/// The number of pages remaining in the per-CPU cache after an allocation request.
pub type AvailablePages = usize;

// `#[repr(C)]` preserves field order so the fast-path fields (`cache_lock`, `available_pages`, and
// `free_list`, 48 bytes total) stay together in the first 64-byte cache line, while slow-path
// `fill_lock` and `random_should_wait_state` sit in the second cache line.
#[ksync::guarded]
#[repr(C)]
struct CpuCache {
    _align: CpuAlignMarker,
    #[mutex]
    cache_lock: KMutex<RawMutex>,
    #[guarded_by(cache_lock)]
    available_pages: usize,
    #[pin]
    #[guarded_by(cache_lock)]
    free_list: PageList,
    #[mutex]
    fill_lock: KMutex<RawMutex>,
    #[guarded_by(cache_lock)]
    random_should_wait_state: Option<usize>,
}

zr::static_assert!(core::mem::align_of::<CpuCache>() == MAX_CACHE_LINE);
zr::static_assert!(core::mem::offset_of!(CpuCache, fill_lock) <= MAX_CACHE_LINE);

impl CpuCache {
    fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            _align: CpuAlignMarker,
            cache_lock <- KMutex::init(),
            available_pages: 0.into(),
            free_list <- KCell::pin_init(PageList::new()),
            fill_lock <- KMutex::init(),
            random_should_wait_state: None.into(),
        })
    }
}

/// `PageCache` provides a front end to the PMM that reserves a given number of
/// pages in per-CPU caches to reduce contention on the PMM.
///
/// TODO(https://fxbug.dev/42147478): Add support for KASAN.
/// TODO(https://fxbug.dev/42147480): Flush page caches when CPUs go offline.
#[repr(C)]
#[derive(Default)]
pub struct PageCache {
    reserve_pages: usize,
    per_cpu_caches: Option<fbl::Array<CpuCache>>,
}

zr::static_assert!(core::mem::size_of::<PageCache>() == 24);
zr::static_assert!(core::mem::align_of::<PageCache>() == 8);
zr::static_assert!(core::mem::offset_of!(PageCache, reserve_pages) == 0);
zr::static_assert!(core::mem::offset_of!(PageCache, per_cpu_caches) == 8);

// SAFETY: `PageCache` internally synchronizes all mutable per-CPU cache access
// via `CpuCache`'s `fill_lock` and `cache_lock`.
unsafe impl Send for PageCache {}
unsafe impl Sync for PageCache {}

impl PageCache {
    /// Creates a new page cache with the given number of reserve pages per CPU.
    /// Filling the per-CPU page caches is deferred until the first allocation
    /// request.
    pub fn new(reserve_pages: usize) -> Result<Self, Status> {
        let cpu_count = PerCpu::processor_count();
        debug_assert!(cpu_count != 0);

        let mut entries = fbl::Array::<CpuCache>::try_new_uninit_slice(cpu_count)
            .map_err(|_| Status::NO_MEMORY)?;
        for item in entries.iter_mut() {
            let init = CpuCache::init();
            // SAFETY: `item.as_mut_ptr()` is a valid, writable pointer to uninitialized
            // memory in `entries`, and `entries` remains pinned on the heap for the
            // lifetime of `PageCache`.
            unsafe {
                let _ = init.__pinned_init(item.as_mut_ptr());
            }
        }
        // SAFETY: All elements in `entries` were initialized in the loop above.
        let entries = unsafe { entries.assume_init() };
        debug_assert!((entries.as_ptr() as usize) & (MAX_CACHE_LINE - 1) == 0);
        Ok(Self { reserve_pages, per_cpu_caches: Some(entries) })
    }

    /// Returns the number of reserve pages per CPU configured for this cache.
    pub fn reserve_pages(&self) -> usize {
        self.reserve_pages
    }

    /// Seeds the per-CPU PRNG state used to randomly return `Status::SHOULD_WAIT`
    /// on waitable allocations.
    pub fn seed_random_should_wait(&self) {
        let per_cpu_caches = self.per_cpu_caches.as_deref().unwrap_or(&[]);
        assert!(!per_cpu_caches.is_empty());
        for entry in per_cpu_caches.iter() {
            ksync::lock!(let mut guard = entry.lock_cache_lock());
            let mut seed: usize = 0;
            // SAFETY: `seed` is a valid stack variable of `size_of::<usize>()` bytes.
            unsafe {
                cpp_global_prng_draw((&raw mut seed).cast::<u8>(), core::mem::size_of::<usize>());
            }
            *guard.as_mut().fields_mut().random_should_wait_state = Some(seed);
        }
    }

    /// Allocates `count` pages from the page cache into `out_pages`.
    pub fn alloc(
        &self,
        count: usize,
        out_pages: Pin<&mut PageList>,
    ) -> Result<AvailablePages, Status> {
        self.allocate(count, 0, out_pages)
    }

    /// Allocates the given number of pages from the page cache into `out_pages`.
    /// Falls back to the PMM if the cache is insufficient to fulfill the request.
    pub fn allocate(
        &self,
        page_count: usize,
        alloc_flags: u32,
        out_pages: Pin<&mut PageList>,
    ) -> Result<AvailablePages, Status> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_ENABLED,
            "kernel:sched",
            "PageCache::Allocate",
            "page_count" => page_count as u64,
            "alloc_flags" => alloc_flags
        );
        debug_assert!(current_memory_allocation_state_is_enabled());
        let per_cpu_caches = self.per_cpu_caches.as_deref().unwrap_or(&[]);
        debug_assert!(!per_cpu_caches.is_empty());

        let _preempt_disable = AutoPreemptDisabler::new();
        let current_cpu = curr_cpu_num() as usize;
        self.allocate_entry(&per_cpu_caches[current_cpu], page_count, alloc_flags, out_pages)
    }

    /// Frees pages back to the page cache. Excess pages are returned to the PMM.
    pub fn free(&self, pages: Pin<&mut PageList>) {
        let mut page_list = pages.list_mut();
        let _trace =
            crate::ktrace_rs::begin_scope_cond!(TRACE_ENABLED, "kernel:sched", "PageCache::Free");
        let per_cpu_caches = self.per_cpu_caches.as_deref().unwrap_or(&[]);
        debug_assert!(!per_cpu_caches.is_empty());

        if !page_list.is_empty() {
            {
                // Note that _preempt_disable is destroyed before excess pages in
                // page_list are freed to the PMM, intentionally resulting in excess
                // pages being freed outside of this local preemption disablement.
                let _preempt_disable = AutoPreemptDisabler::new();
                let current_cpu = curr_cpu_num() as usize;
                self.free_entry(&per_cpu_caches[current_cpu], page_list.as_mut());
            }
            if !page_list.is_empty() {
                // SAFETY: Remaining pages in `page_list` are valid allocated PMM pages
                // that did not fit in the per-CPU cache.
                unsafe { pmm::free_list(page_list) };
            }
        }
    }

    /// Attempts to allocate the given number of pages from the CPU cache. If the
    /// cache is insufficient for the request, falls back to the PMM to fulfill the
    /// request and refill the cache. The requested number of pages may be zero, in
    /// which case only the cache is filled.
    fn allocate_entry(
        &self,
        entry: &CpuCache,
        requested_pages: usize,
        alloc_flags: u32,
        out_pages: Pin<&mut PageList>,
    ) -> Result<AvailablePages, Status> {
        if requested_pages > 0 {
            ksync::lock!(let mut guard = entry.lock_cache_lock());
            let fields = guard.as_mut().fields_mut();
            // If the allocation is waitable, check if user wants random should waits,
            // and if so randomly error 10% of allocation requests.
            if (alloc_flags & ALLOC_FLAG_CAN_WAIT) != 0
                && let Some(seed) = fields.random_should_wait_state.as_mut()
                // SAFETY: `seed` is a valid mutable pointer to the per-CPU PRNG state
                // guarded by `cache_lock`.
                && unsafe { rand_r(seed) } < (RAND_MAX / 10)
            {
                return Err(Status::SHOULD_WAIT);
            }
            if requested_pages <= *fields.available_pages {
                return Ok(Self::allocate_cache_pages(guard.as_mut(), requested_pages, out_pages));
            }
        }
        self.allocate_pages_and_fill_cache(entry, requested_pages, alloc_flags, out_pages)
    }

    /// Allocates the given number of pages from the given CPU cache.
    fn allocate_cache_pages(
        mut guard: Pin<&mut CpuCacheCacheLockGuard<'_>>,
        requested_pages: usize,
        out_pages: Pin<&mut PageList>,
    ) -> AvailablePages {
        let mut fields = guard.as_mut().fields_mut();
        debug_assert!(requested_pages <= *fields.available_pages);
        debug_assert!(requested_pages > 0);

        *fields.available_pages -= requested_pages;
        let mut out_list = out_pages.list_mut();
        let mut free_list = fields.free_list.as_mut().list_mut();
        for _ in 0..requested_pages {
            let page = free_list.as_mut().pop_back().expect("free_list underflow");
            // SAFETY: We have conceptual ownership of `page` after popping it from `free_list`.
            unsafe {
                page.as_ref().set_state(VmPageState(vm_page_state::ALLOC));
                out_list.as_mut().push_front_raw(page);
            }
        }

        count_hit_pages(requested_pages);

        *fields.available_pages
    }

    /// Returns the given list of pages to the given CPU cache, leaving excess
    /// pages in `page_list` to be returned to the PMM by the caller.
    fn free_entry(&self, entry: &CpuCache, page_list: Pin<&mut VmPageDoublyLinkedList>) {
        ksync::lock!(let mut guard = entry.lock_cache_lock());
        let mut fields = guard.as_mut().fields_mut();

        let mut free_count = 0;
        let mut return_count = 0;
        stack_pin_init!(let return_list = VmPageDoublyLinkedList::new());

        // Filter out non-borrowed pages to return to the free list. Pages remaining
        // in page_list are freed to the PMM outside the lock.
        let mut cursor = page_list.cursor_front_mut();
        while let Some(page) = cursor.get_mut() {
            debug_assert!(!page.is_loaned());
            if *fields.available_pages < self.reserve_pages {
                // SAFETY: We have conceptual ownership of `page` in `page_list`.
                unsafe { page.set_state(VmPageState(vm_page_state::CACHE)) };
                let erased = cursor.erase().expect("cursor had current element");
                // SAFETY: `erased` was just removed from `page_list` and is not in any list.
                unsafe { return_list.as_mut().push_back_raw(erased) };
                *fields.available_pages += 1;
                return_count += 1;
            } else {
                free_count += 1;
                cursor.move_next();
            }
        }

        // Return the selected pages to the free list.
        let free_list = fields.free_list.as_mut().list_mut();
        free_list.cursor_front_mut().splice(return_list);
        count_return_pages(return_count);
        count_free_pages(free_count);
    }

    fn allocate_pages_and_fill_cache(
        &self,
        entry: &CpuCache,
        requested_pages: usize,
        alloc_flags: u32,
        out_pages: Pin<&mut PageList>,
    ) -> Result<AvailablePages, Status> {
        let _trace = crate::ktrace_rs::begin_scope_cond!(
            TRACE_ENABLED,
            "kernel:sched",
            "PageCache::AllocatePagesAndFillCache",
            "requested_pages" => requested_pages as u64,
            "alloc_flags" => alloc_flags
        );

        // Serialize cache fill + allocate operations on this cache. Contention
        // means another thread tried to allocate from the PMM and blocked on the
        // PMM lock. There's no benefit to following the owning thread into the PMM
        // allocator; by the time this lock is released the cache may have enough
        // pages to satisfy this request without falling back to the PMM again.
        ksync::lock!(entry.lock_fill_lock());

        // Acquire the cache lock after the fill lock. If this thread blocked on the
        // previous lock, there is a chance this thread is now running on a
        // different CPU. However, there's also a good chance the cache is now
        // sufficient to fulfill the request without falling back to the PMM.
        ksync::lock!(let mut cache_guard = entry.lock_cache_lock());

        // Re-validate the request after acquiring the locks. Another thread may
        // have filled the cache sufficiently already.
        if requested_pages > *cache_guard.available_pages() {
            let refill_pages = self.reserve_pages - *cache_guard.available_pages();
            let total_pages = requested_pages + refill_pages;
            count_refill_pages(refill_pages);
            count_miss_pages(requested_pages);

            stack_pin_init!(let page_list = VmPageDoublyLinkedList::new());

            // Release the cache lock while calling into the PMM to permit other
            // threads to access the cache if this thread blocks.
            let status = cache_guard.as_mut().call_unlocked(|| {
                let mut pages = total_pages;
                let mut status = pmm::alloc_pages(pages, alloc_flags, page_list.as_mut());
                // If the alloc_flags contained ALLOC_FLAG_CAN_WAIT then we could get
                // Status::SHOULD_WAIT as a return code. In this case, as per the
                // documentation of pmm::alloc_pages, we must fall back to attempting to
                // allocate single pages.
                if status == Err(Status::SHOULD_WAIT) {
                    status = Ok(());
                    while pages > 0 {
                        match pmm::alloc_page(alloc_flags) {
                            Ok((page, _pa)) => {
                                // SAFETY: `page` was just allocated from the PMM and is not in
                                // any list.
                                unsafe { page_list.as_mut().push_back_raw(page.as_non_null()) };
                                pages -= 1;
                            }
                            Err(err) => {
                                status = Err(err);
                                break;
                            }
                        }
                    }
                }
                status
            });

            if let Err(err) = status {
                // Even if allocating all the pages failed, we may have received a few.
                // If so, put them into the cache and then see if we have enough for the
                // actual requested allocation. Since this is an uncommon edge case do
                // not try and be too optimal with the implementation.
                let mut fields = cache_guard.as_mut().fields_mut();
                let mut free_list = fields.free_list.as_mut().list_mut();
                while let Some(page) = page_list.as_mut().pop_front() {
                    // SAFETY: We have conceptual ownership of `page` and it is not in any list.
                    unsafe {
                        page.as_ref().set_state(VmPageState(vm_page_state::CACHE));
                        free_list.as_mut().push_back_raw(page);
                    }
                    *fields.available_pages += 1;
                }
                if requested_pages <= *fields.available_pages {
                    return Ok(Self::allocate_cache_pages(
                        cache_guard.as_mut(),
                        requested_pages,
                        out_pages,
                    ));
                }
                return Err(err);
            }

            let out_list = out_pages.list_mut();

            // Set the page state of the refill pages and find the end of the refill list.
            if refill_pages > 0 {
                let mut cursor = page_list.as_mut().cursor_front_mut();
                for i in 0..refill_pages {
                    let page = cursor.get_mut().expect("refill_pages > page_list len");
                    // SAFETY: We have conceptual ownership of `page` in `page_list`.
                    unsafe { page.set_state(VmPageState(vm_page_state::CACHE)) };
                    if i + 1 < refill_pages {
                        cursor.move_next();
                    }
                }

                // Separate the return list from the refill list and add the latter to the
                // free list.
                let mut out_cursor = out_list.cursor_back_mut();
                cursor.split_after(&mut out_cursor);
            } else {
                out_list.splice(page_list.as_mut());
            }

            let mut fields = cache_guard.as_mut().fields_mut();
            let free_list = fields.free_list.as_mut().list_mut();
            free_list.cursor_front_mut().splice(page_list);
            *fields.available_pages += refill_pages;
        } else if requested_pages > 0 {
            return Ok(Self::allocate_cache_pages(
                cache_guard.as_mut(),
                requested_pages,
                out_pages,
            ));
        }

        Ok(*cache_guard.available_pages())
    }
}

/// In-tree kernel unit tests for `page_cache`.
#[cfg(ktest)]
#[unittest::suite(name = "page_cache_rust")]
mod tests {
    use super::{PageCache, PageList};
    use crate::arch_rs::curr_cpu_num;
    use crate::kernel::thread::{AutoPreemptDisabler, ThreadPtr};
    use crate::kernel::types::cpu_num_to_mask;
    use crate::vm::pmm::ALLOC_FLAG_CAN_WAIT;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_false, unwrap_ok};
    use zx_status::Status;

    /// Tests page cache allocation, refill, free, and random `SHOULD_WAIT` behavior.
    #[test]
    fn page_cache_tests() {
        const RESERVE_PAGES: usize = 8;
        let page_cache = unwrap_ok!(PageCache::new(RESERVE_PAGES));
        expect_eq!(RESERVE_PAGES, page_cache.reserve_pages());

        page_cache.seed_random_should_wait();

        // Stay on one CPU during the following tests to verify numeric properties of
        // a single per-CPU cache. Accounting for CPU migration during the tests would
        // make them overly complicated for little value.
        // SAFETY: Kernel unit tests run after threading is initialized.
        let current_thread = unsafe { ThreadPtr::current() };
        // SAFETY: `current_thread` is valid for the duration of this test.
        let original_affinity_mask = unsafe { current_thread.get_cpu_affinity() };
        let _restore_affinity = zr::defer(|| {
            // SAFETY: `current_thread` is valid for the duration of this test.
            unsafe {
                current_thread.set_cpu_affinity(original_affinity_mask);
            }
        });

        {
            let _preempt_disable = AutoPreemptDisabler::new();
            let current_cpu = curr_cpu_num();
            // SAFETY: `current_thread` is valid.
            unsafe {
                current_thread.set_cpu_affinity(cpu_num_to_mask(current_cpu));
            }
        }

        // An allocation from an empty or insufficient page cache fills the cache AND
        // returns the pages requested.
        {
            let page_count = RESERVE_PAGES / 2;
            stack_pin_init!(let page_list = PageList::new());
            let available_pages = unwrap_ok!(page_cache.alloc(page_count, page_list.as_mut()));
            expect_eq!(page_count, page_list.iter().count());
            expect_eq!(RESERVE_PAGES, available_pages);
        }

        // An allocation from a sufficient page cache does not fill the cache AND
        // reduces the number of pages available.
        {
            let page_count = RESERVE_PAGES / 2;
            stack_pin_init!(let page_list = PageList::new());
            let available_pages = unwrap_ok!(page_cache.alloc(page_count, page_list.as_mut()));
            expect_eq!(page_count, page_list.iter().count());
            expect_eq!(page_count, available_pages);
        }

        // An allocation that is too large for the page cache fills the cache AND
        // returns the pages requested.
        {
            let page_count = RESERVE_PAGES * 2;
            stack_pin_init!(let page_list = PageList::new());
            let available_pages = unwrap_ok!(page_cache.alloc(page_count, page_list.as_mut()));
            expect_eq!(page_count, page_list.iter().count());
            expect_eq!(RESERVE_PAGES, available_pages);
        }

        // Exercise basic free.
        {
            let page_count = RESERVE_PAGES / 2;
            stack_pin_init!(let page_list = PageList::new());
            let available_pages = unwrap_ok!(page_cache.alloc(page_count, page_list.as_mut()));
            expect_eq!(page_count, page_list.iter().count());
            expect_eq!(RESERVE_PAGES - page_count, available_pages);

            page_cache.free(page_list.as_mut());
            expect_eq!(0, page_list.iter().count());

            stack_pin_init!(let null_list = PageList::new());
            let null_available = unwrap_ok!(page_cache.alloc(0, null_list.as_mut()));
            expect_eq!(RESERVE_PAGES, null_available);
        }

        // Exercise intermixing small and oversized allocations and frees.
        {
            let large_page_count = RESERVE_PAGES * 2;
            stack_pin_init!(let large_list = PageList::new());
            let large_available =
                unwrap_ok!(page_cache.alloc(large_page_count, large_list.as_mut()));
            expect_eq!(large_page_count, large_list.iter().count());
            expect_eq!(RESERVE_PAGES, large_available);

            let page_count = 1;
            stack_pin_init!(let page_list = PageList::new());
            let available_pages = unwrap_ok!(page_cache.alloc(page_count, page_list.as_mut()));
            expect_eq!(page_count, page_list.iter().count());
            expect_eq!(RESERVE_PAGES - page_count, available_pages);

            page_cache.free(large_list.as_mut());
            expect_eq!(0, large_list.iter().count());

            stack_pin_init!(let null_list = PageList::new());
            let null_available = unwrap_ok!(page_cache.alloc(0, null_list.as_mut()));
            expect_eq!(RESERVE_PAGES, null_available);
            expect_eq!(0, null_list.iter().count());

            page_cache.free(page_list.as_mut());
            expect_eq!(0, page_list.iter().count());

            stack_pin_init!(let null_list2 = PageList::new());
            let null_available2 = unwrap_ok!(page_cache.alloc(0, null_list2.as_mut()));
            expect_eq!(RESERVE_PAGES, null_available2);
            expect_eq!(0, null_list2.iter().count());
        }

        // Verify that random should wait will work by repeatedly allocating and freeing a
        // page until we get should wait.
        {
            // Allocate a page without SHOULD_WAIT to ensure that the cache has items.
            stack_pin_init!(let page_list = PageList::new());
            let _ = unwrap_ok!(page_cache.alloc(1, page_list.as_mut()));

            // Now repeatedly free and allocate this page until we get should wait.
            let status = loop {
                page_cache.free(page_list.as_mut());
                if let Err(err) = page_cache.allocate(1, ALLOC_FLAG_CAN_WAIT, page_list.as_mut()) {
                    break err;
                }
            };
            expect_eq!(status.into_raw(), Status::SHOULD_WAIT.into_raw());
        }
    }

    /// Tests that PageList drop automatically frees unconsumed pages back to PMM.
    #[test]
    fn test_page_list_drop_frees_pages() {
        let cache = unwrap_ok!(PageCache::new(4));

        {
            stack_pin_init!(let pages = PageList::new());
            let _available_pages = unwrap_ok!(cache.alloc(2, pages.as_mut()));
            expect_false!(pages.is_empty());
            // Dropping `pages` without calling `cache.free`:
            // `PageList::drop` must safely return pages to the PMM without leaking.
        }
    }
}
