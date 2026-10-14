// Copyright 2021 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod virtual_alloc_ffi;

use crate::kernel::align::{is_rounded, roundup};
use crate::kernel::types::PAddr;
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE,
    ARCH_UNMAP_OPTION_NONE, ArchMmuFlags, ExistingEntryAction,
};
use crate::vm::page_state::VmPageState;
use crate::vm::pmm;
use crate::vm::vm_aspace::VmAspace;
use bitmap::{Bitmap, RawBitmapGeneric};
use bitmap_storage::BitmapStorage;
use debug::ltracef;
use fbl::Canary;
use page_list::{OwnedPage, PageList};
use pin_init::stack_pin_init;
use zx_status::Status;

const LOCAL_TRACE: u32 = 0;

mod page_list {
    use crate::kernel::types::PAddr;
    use crate::vm::page::{VmPage, VmPageDoublyLinkedList, VmPagePtr};
    use crate::vm::page_state::VmPageState;
    use crate::vm::pmm;
    use core::ops::Deref;
    use core::pin::Pin;
    use core::ptr::NonNull;
    use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
    use zx_status::Status;

    /// An exclusively owned, unmapped physical page allocated from the PMM that is not currently
    /// in any intrusive list. Frees the page back to the PMM on drop unless transferred into a
    /// [`PageList`].
    pub struct OwnedPage(NonNull<VmPage>);

    impl OwnedPage {
        /// Allocates a single physical page from the PMM.
        #[cfg(ktest)]
        pub fn alloc(flags: u32) -> Result<(Self, PAddr), Status> {
            let (page, paddr) = pmm::alloc_page(flags)?;
            Ok((Self(page.as_non_null()), paddr))
        }

        /// Wraps an allocated PMM page that is about to be unmapped and freed.
        ///
        /// # Safety
        ///
        /// `page` must be a valid allocated PMM page uniquely owned by the caller, not in any
        /// intrusive list, and must be unmapped before this `OwnedPage` (or any [`PageList`] it is
        /// placed into) is dropped.
        pub unsafe fn from_allocated(page: VmPagePtr) -> Self {
            Self(page.as_non_null())
        }

        fn into_raw(self) -> NonNull<VmPage> {
            let ptr = self.0;
            core::mem::forget(self);
            ptr
        }
    }

    impl Drop for OwnedPage {
        fn drop(&mut self) {
            // SAFETY: `self.0` is an exclusively owned, unmapped PMM page not in any list.
            unsafe {
                pmm::free_page(VmPagePtr::new(self.0));
            }
        }
    }

    /// An RAII wrapper around [`VmPageDoublyLinkedList`] that holds exclusive conceptual ownership
    /// of its pages and frees any remaining pages back to the PMM on drop.
    #[pin_data(PinnedDrop)]
    pub struct PageList {
        #[pin]
        list: VmPageDoublyLinkedList,
    }

    impl PageList {
        pub fn new() -> impl PinInit<Self, core::convert::Infallible> {
            pin_init!(Self {
                list <- VmPageDoublyLinkedList::new(),
            })
        }

        /// Allocates `count` pages from the PMM and appends them to `self`.
        pub fn alloc_pages(self: Pin<&mut Self>, count: usize, flags: u32) -> Result<(), Status> {
            pmm::alloc_pages(count, flags, self.project().list)
        }

        /// Allocates `count` contiguous pages from the PMM and appends them to `self`.
        pub fn alloc_contiguous(
            self: Pin<&mut Self>,
            count: usize,
            flags: u32,
            align_log2: u8,
        ) -> Result<PAddr, Status> {
            pmm::alloc_contiguous(count, flags, align_log2, self.project().list)
        }

        /// Sets the page state for all pages currently in `self`.
        pub fn set_state(self: Pin<&mut Self>, state: VmPageState) {
            for p in self.list.iter() {
                // SAFETY: `self` holds exclusive conceptual ownership of all pages in the list.
                unsafe {
                    p.set_state(state);
                }
            }
        }

        /// Appends `page` to the back of `self`.
        pub fn push_back(self: Pin<&mut Self>, page: OwnedPage) {
            // SAFETY: `page` is a valid PMM page not in any container, and ownership is
            // transferred to `self`.
            unsafe {
                self.project().list.push_back_raw(page.into_raw());
            }
        }

        /// Prepends `page` to the front of `self`.
        #[cfg(ktest)]
        pub fn push_front(self: Pin<&mut Self>, page: OwnedPage) {
            // SAFETY: `page` is a valid PMM page not in any container, and ownership is
            // transferred to `self`.
            unsafe {
                self.project().list.push_front_raw(page.into_raw());
            }
        }

        /// Removes and returns the first page in `self`.
        #[cfg(ktest)]
        pub fn pop_front(self: Pin<&mut Self>) -> Option<OwnedPage> {
            self.project().list.pop_front().map(OwnedPage)
        }

        /// Removes and returns the last page in `self`.
        #[cfg(ktest)]
        pub fn pop_back(self: Pin<&mut Self>) -> Option<OwnedPage> {
            self.project().list.pop_back().map(OwnedPage)
        }

        /// Splices all pages from `other` onto the front of `self`.
        pub fn splice_front(self: Pin<&mut Self>, other: Pin<&mut Self>) {
            self.project().list.cursor_front_mut().splice(other.project().list);
        }

        /// Splices all pages from `other` onto the back of `self` and returns a [`PageCursor`]
        /// positioned at the first newly spliced page (or at the end if `other` was empty).
        pub fn splice_back(mut self: Pin<&mut Self>, other: Pin<&mut Self>) -> PageCursor<'_> {
            let current = other.list.front().map(NonNull::from);
            self.as_mut().project().list.splice(other.project().list);
            PageCursor { list: self, current }
        }

        /// Frees all pages currently in `self` back to the PMM, leaving `self` empty.
        pub fn free_pages(self: Pin<&mut Self>) {
            if !self.list.is_empty() {
                // SAFETY: All pages in `self` are valid allocated PMM pages owned by `self`.
                unsafe {
                    pmm::free_list(self.project().list);
                }
            }
        }

        /// Clears `self` without freeing the pages back to the PMM, transferring conceptual
        /// ownership of the pages to the hardware page tables after mapping succeeds.
        pub fn forget_pages(self: Pin<&mut Self>) {
            self.project().list.clear();
        }
    }

    impl Deref for PageList {
        type Target = VmPageDoublyLinkedList;

        fn deref(&self) -> &Self::Target {
            &self.list
        }
    }

    #[pinned_drop]
    impl PinnedDrop for PageList {
        fn drop(self: Pin<&mut Self>) {
            self.free_pages();
        }
    }

    /// A cursor over a [`PageList`] that holds exclusive mutable access to the list.
    pub struct PageCursor<'a> {
        list: Pin<&'a mut PageList>,
        current: Option<NonNull<VmPage>>,
    }

    impl PageCursor<'_> {
        /// Returns whether the cursor currently points to a valid page.
        pub fn is_valid(&self) -> bool {
            self.current.is_some()
        }

        /// Sets the state of the page at the current cursor position.
        ///
        /// # Panics
        ///
        /// Panics if the cursor is at the end of the list.
        pub fn set_state(&mut self, state: VmPageState) {
            let page = self.current.expect("cursor must be valid");
            // SAFETY: `self.list` is exclusively borrowed for `'a`, and `self.current` is a
            // valid element of `self.list`, which holds exclusive conceptual ownership of all
            // pages in the list.
            unsafe {
                page.as_ref().set_state(state);
            }
        }

        /// Returns the physical address of the page at the current cursor position.
        ///
        /// # Panics
        ///
        /// Panics if the cursor is at the end of the list.
        pub fn paddr(&self) -> PAddr {
            let page = self.current.expect("cursor must be valid");
            // SAFETY: `self.current` is a valid element of `self.list`.
            unsafe { page.as_ref().paddr() }
        }

        /// Advances the cursor to the next page in the list.
        pub fn move_next(&mut self) {
            if let Some(page) = self.current {
                // SAFETY: `page` is a valid member of `self.list`.
                let mut cursor =
                    unsafe { self.list.as_mut().project().list.cursor_at(page.as_ref()) };
                cursor.move_next();
                self.current = cursor.get().map(NonNull::from);
            }
        }
    }
}

mod bitmap_storage {
    use bitmap::Storage;
    use core::ptr::NonNull;
    use zx_status::Status;

    // Custom storage for the bitmap class that provides for a statically sized storage that can be
    // initialized after construction.
    #[derive(Debug, Default)]
    pub struct BitmapStorage {
        base: Option<NonNull<usize>>,
        size: usize,
    }

    impl BitmapStorage {
        pub const fn new() -> Self {
            Self { base: None, size: 0 }
        }

        /// Initializes the backing storage pointer and size in bytes, zero-initializing the memory.
        ///
        /// # Panics
        ///
        /// Panics if `base` is zero or not aligned to `usize`.
        ///
        /// # Safety
        ///
        /// `base` must point to `size` bytes of valid, mapped memory uniquely owned by the
        /// containing `VirtualAlloc` instance until [`Self::reset`] is called.
        pub unsafe fn init(&mut self, base: usize, size: usize) {
            let ptr = NonNull::new(core::ptr::with_exposed_provenance_mut::<usize>(base))
                .expect("base must be non-zero");
            assert!(ptr.is_aligned(), "base must be aligned to usize");
            let num_words = size / core::mem::size_of::<usize>();
            // SAFETY: Caller guarantees `[base, base + size)` is mapped and uniquely owned, and
            // alignment was verified above. Zeroing ensures the memory is initialized before any
            // slice is formed in `get_data` or `get_data_mut`.
            unsafe {
                core::ptr::write_bytes(ptr.as_ptr(), 0, num_words);
            }
            self.base = Some(ptr);
            self.size = size;
        }

        pub fn reset(&mut self) {
            self.base = None;
            self.size = 0;
        }

        pub fn get_size(&self) -> usize {
            self.size
        }
    }

    impl Storage for BitmapStorage {
        fn allocate(&mut self, size: usize) -> Result<(), Status> {
            if size > self.size {
                return Err(Status::NO_MEMORY);
            }
            Ok(())
        }

        fn get_data(&self) -> &[usize] {
            // SAFETY: When `self.base` is `Some`, it points to `self.size` bytes of mapped,
            // page-aligned, initialized memory owned by this `VirtualAlloc` instance. When `None`,
            // `self.size` is 0.
            unsafe {
                core::slice::from_raw_parts(
                    self.base.unwrap_or(NonNull::dangling()).as_ptr(),
                    self.size / core::mem::size_of::<usize>(),
                )
            }
        }

        fn get_data_mut(&mut self) -> &mut [usize] {
            // SAFETY: When `self.base` is `Some`, it points to `self.size` bytes of mapped,
            // page-aligned, initialized memory uniquely owned by this `VirtualAlloc` instance. When
            // `None`, `self.size` is 0.
            unsafe {
                core::slice::from_raw_parts_mut(
                    self.base.unwrap_or(NonNull::dangling()).as_ptr(),
                    self.size / core::mem::size_of::<usize>(),
                )
            }
        }
    }
}

/// `VirtualAlloc` is a page granule allocator that manages a given virtual region and provides
/// virtually contiguous allocations inside that region. This allocator explicitly has no
/// dependency on the heap and retrieves all its backing memory directly from the pmm. To achieve
/// this it maps pages directly into the hardware page tables via the arch aspace, and
/// consequently assumes that these operations will only depend on the pmm and not the heap for
/// allocating any intermediate page tables.
///
/// This class is thread-unsafe.
#[repr(C)]
pub struct VirtualAlloc {
    canary: Canary<{ fbl::magic(b"VALC") }>,

    // Page state to set allocated pages to.
    allocated_page_state: VmPageState,

    // Record of that padding to be applied to every allocation.
    alloc_guard: usize,

    // The virtual address of the start of the range managed by `bitmap`.
    alloc_base: usize,

    // Heuristic used to attempt to begin searching for a free run in `bitmap` at an optimal point.
    next_search_start: usize,

    align_log2: usize,

    // Bitmap representing allocated pages in the virtual range being managed. This is fully
    // preallocated and reserves for itself a portion of the `init` virtual address range.
    bitmap: RawBitmapGeneric<BitmapStorage>,
}

zr::static_assert!(core::mem::size_of::<VirtualAlloc>() == 64);
zr::static_assert!(core::mem::align_of::<VirtualAlloc>() == 8);

impl Drop for VirtualAlloc {
    fn drop(&mut self) {
        self.destroy();
    }
}

impl VirtualAlloc {
    /// Allocator needs to be told what state to set any allocated pages to. This allows for using
    /// the allocator as a heap or generic allocator for an object.
    pub const fn new(allocated_page_state: VmPageState) -> Self {
        Self {
            canary: Canary::new(),
            allocated_page_state,
            alloc_guard: 0,
            alloc_base: 0,
            next_search_start: 0,
            align_log2: 0,
            bitmap: RawBitmapGeneric::new(BitmapStorage::new()),
        }
    }

    /// Initialize the allocator and make it ready for use. Takes a virtual address range as a
    /// base and size in bytes. These both must be page aligned and there must be no pages mapped
    /// into the hardware page tables for this range.
    /// `alloc_guard` is the minimum number of virtual pages to place between two allocations and
    /// serve as guard pages. Any reads or writes that over or under run an allocation into the
    /// padding will trigger an immediate hardware page fault.
    pub fn init(
        &mut self,
        base: usize,
        size: usize,
        alloc_guard: usize,
        align_log2: usize,
    ) -> Result<(), Status> {
        self.canary.assert();

        if self.alloc_base != 0 {
            // This has already been initialized.
            return Err(Status::BAD_STATE);
        }

        if align_log2 < page::SHIFT {
            return Err(Status::INVALID_ARGS);
        }
        self.align_log2 = align_log2;

        let vaddr_align = 1usize << self.align_log2;

        if size == 0
            || !is_rounded(size, vaddr_align)
            || !is_rounded(base, vaddr_align)
            || base.wrapping_add(size) < base
        {
            return Err(Status::INVALID_ARGS);
        }

        // Work how how many pages we need for the bitmap.
        let total_pages = size / page::SIZE;
        const BITS_PER_PAGE: usize = page::SIZE * (u8::BITS as usize);
        let bitmap_pages = roundup(total_pages, BITS_PER_PAGE) / BITS_PER_PAGE;

        // Validate that there will be anything left after allocating the bitmap for an actual
        // allocation. A single allocation needs padding on both sides of it. This ignores
        // alignment problems caused by the bitmap, and so it's still possible for non page size
        // alignments that if this check passes that no allocations are possible, but this is not
        // meant to be an exhaustive guard.
        if bitmap_pages + alloc_guard * 2 >= total_pages {
            return Err(Status::INVALID_ARGS);
        }

        // Allocate and map the bitmap pages into the start of the range we were given.
        self.alloc_map_pages(base, bitmap_pages)?;
        // SAFETY: `[base, base + bitmap_pages * page::SIZE)` was just mapped by `alloc_map_pages`
        // and remains uniquely owned and mapped until `destroy`.
        unsafe {
            self.bitmap.storage_mut().init(base, bitmap_pages * page::SIZE);
        }

        // Initialize the bitmap, reserving its own pages.
        self.alloc_base = base;
        let _ = self.bitmap.reset(total_pages);
        let _ = self.bitmap.set(0, bitmap_pages);

        // Set our first search to happen after the bitmap.
        self.next_search_start = bitmap_pages;

        self.alloc_guard = alloc_guard;
        Ok(())
    }

    fn bitmap_alloc_range(
        &mut self,
        num_pages: usize,
        start: usize,
        end: usize,
    ) -> Result<usize, Status> {
        debug_assert!(end >= start);
        debug_assert!(num_pages > 0);
        let align_pages = 1usize << (self.align_log2 - page::SHIFT);
        // Want to find a run of `num_pages` + padding on either end. By over-searching we can
        // ensure there is always `alloc_guard` unused pages / unset-bits between each allocation.
        let find_pages = num_pages + self.alloc_guard * 2;

        // Helper to finalize an allocated range once found. This just allows for structuring the
        // code in a slightly more readable way for the two different kinds of searches.
        let complete_alloc = |this: &mut Self, mut start_index: usize| -> Result<usize, Status> {
            // Increase our start to skip the padding we want to leave.
            start_index += this.alloc_guard;
            // Record the end of this allocation as our next search start. We set the end to not
            // include the padding so that the padding at the end of this allocation becomes the
            // padding at the start of the next one.
            this.next_search_start = start_index + num_pages;
            // Set the bits for the 'inner' allocation, leaving the padding we found unset.
            let _ = this.bitmap.set(start_index, start_index + num_pages);
            Ok(start_index)
        };

        // If requested less pages than the alignment then do not bother finding an aligned range,
        // just find anything. The assumption here is that the block of pages we map in later will
        // not be large enough to benefit from any alignment, so might as well avoid fragmentation
        // and do a more efficient search.
        if num_pages >= align_pages && align_pages > 1 {
            let mut current_start = start;
            loop {
                // Construct a candidate range from the start. This candidate needs to be chosen
                // such that after skipping the allocation padding it is aligned.
                let mut candidate = roundup(current_start, align_pages);
                if candidate - current_start < self.alloc_guard {
                    // Add on sufficient alignment multiples that we will be able to subtract
                    // `alloc_guard` without ending up below `start`.
                    candidate += roundup(self.alloc_guard, align_pages);
                }
                candidate -= self.alloc_guard;
                // If the range from the candidate would exceed our search range, then no aligned
                // range.
                if candidate + find_pages > end {
                    break;
                }
                // Scan from the candidate and see if all the bits are clear.
                let Some(set_bit) = self.bitmap.scan(candidate, candidate + find_pages, false)
                else {
                    return complete_alloc(self, candidate);
                };
                // Search from the bit that was set to find the next unset bit. This will become
                // our next starting search location.
                let Some(next_start) = self.bitmap.scan(set_bit, end, true) else {
                    // If all the bits are set then no aligned range.
                    break;
                };
                debug_assert!(next_start > current_start);
                current_start = next_start;
            }
        }
        // See if there's an unaligned range that will satisfy.
        let alloc_start = self.bitmap.find(false, start, end, find_pages)?;
        complete_alloc(self, alloc_start)
    }

    fn bitmap_alloc(&mut self, num_pages: usize) -> Result<usize, Status> {
        // First search from our saved recommended starting location.
        let result = self.bitmap_alloc_range(num_pages, self.next_search_start, self.bitmap.size());
        if result.is_err() {
            // Try again from the beginning (skipping the bitmap itself). Still search to the end
            // just in case the original search start was in the middle of a free run.
            return self.bitmap_alloc_range(num_pages, self.bitmap_pages(), self.bitmap.size());
        }
        result
    }

    /// `alloc_pages` can only be called after a successful `init` call, otherwise it will return
    /// an error. Number of pages requested must be non zero.
    pub fn alloc_pages(&mut self, pages: usize) -> Result<usize, Status> {
        self.canary.assert();

        if self.alloc_base == 0 {
            return Err(Status::BAD_STATE);
        }

        if pages == 0 {
            return Err(Status::INVALID_ARGS);
        }

        // Allocate space from the bitmap, it will set the bits and ensure padding is left around
        // the allocation.
        let start = self.bitmap_alloc(pages)?;

        // Turn the bitmap index into a virtual address and allocate the pages there.
        let vstart = self.alloc_base + start * page::SIZE;
        if let Err(status) = self.alloc_map_pages(vstart, pages) {
            // Return the range back to the bitmap.
            self.bitmap_free(start, pages);
            return Err(status);
        }
        ltracef!("Allocated {pages} pages at {vstart:#x}\n");
        Ok(vstart)
    }

    fn bitmap_free(&mut self, start: usize, num_pages: usize) {
        assert!(start >= self.bitmap_pages());
        assert!(start < self.bitmap.size());
        assert!(num_pages <= self.bitmap.size() - start);
        debug_assert!(self.bitmap.scan(start, start + num_pages, true).is_none());

        let status = self.bitmap.clear(start, start + num_pages);
        assert!(status.is_ok());
        if start < self.next_search_start {
            self.next_search_start = start;
            // To attempt to keep allocations compact check `alloc_guard` bits backwards, and move
            // our search start if unset. This ensures that if we alloc+free that our
            // `next_search_start` gets reset to the original location, otherwise it will
            // constantly creep by `alloc_guard`.
            if self.next_search_start >= self.alloc_guard {
                if let Some(candidate) = self.bitmap.reverse_scan(
                    self.next_search_start - self.alloc_guard,
                    self.next_search_start,
                    false,
                ) {
                    ltracef!(
                        "Reverse scan moved search from {} part way to {}\n",
                        self.next_search_start,
                        candidate + 1
                    );
                    self.next_search_start = candidate + 1;
                } else {
                    ltracef!(
                        "Reverse scan moved search from {} all the way to {}\n",
                        self.next_search_start,
                        self.next_search_start - self.alloc_guard
                    );
                    self.next_search_start -= self.alloc_guard;
                }
            }
        }
    }

    /// Returns a non-zero number of pages at the given virtual address. Partial frees are
    /// supported such that if 2 pages were allocated at X it is allowed to `free_pages(X, 1)` and
    /// separately `free_pages(X + page::SIZE, 1)`.
    pub fn free_pages(&mut self, vaddr: usize, pages: usize) {
        assert!(self.alloc_base != 0);
        assert!(pages > 0);
        assert!(page::is_page_rounded(vaddr));
        self.canary.assert();

        ltracef!("Free {pages} pages at {vaddr:#x}\n");

        // Release the bitmap range prior to unmapping to ensure any attempts to free an invalid
        // range are caught before attempting to unmap 'random' memory.
        assert!(vaddr >= self.alloc_base);
        self.bitmap_free((vaddr - self.alloc_base) / page::SIZE, pages);
        self.unmap_free_pages(vaddr, pages);
    }

    fn unmap_free_pages(&self, vaddr: usize, pages: usize) {
        stack_pin_init!(let free_list = PageList::new());
        ltracef!("Unmapping {pages} pages at {vaddr:x}\n");
        for i in 0..pages {
            let query_res = VmAspace::kernel_aspace().arch_aspace().query(vaddr + i * page::SIZE);
            assert!(query_res.is_ok());
            let (paddr, _) = query_res.unwrap();
            let page = pmm::paddr_to_vm_page(paddr);
            assert!(page.is_some());
            let page = page.unwrap();
            // SAFETY: `page` is an allocated page owned by this `VirtualAlloc`, is not currently
            // in any list, and is unmapped below before `free_list` is dropped.
            free_list.as_mut().push_back(unsafe { OwnedPage::from_allocated(page) });
        }
        // As this is the kernel aspace we cannot tolerate enlarging the operation, as other CPUs
        // may be accessing the nearby portions and we cannot trigger faults.
        // SAFETY: The virtual range `[vaddr, vaddr + pages * page::SIZE)` is owned by this
        // `VirtualAlloc` and is being freed.
        let status = unsafe {
            VmAspace::kernel_aspace().arch_aspace().unmap(vaddr, pages, ARCH_UNMAP_OPTION_NONE)
        };
        assert!(status.is_ok(), "Failed to unmap {pages} pages at {vaddr:x}");
    }

    fn alloc_map_pages(&self, vaddr: usize, num_pages: usize) -> Result<(), Status> {
        struct Cleanup {
            vaddr: usize,
            mapped_count: usize,
        }

        impl Cleanup {
            fn cancel(self) {
                core::mem::forget(self);
            }
        }

        impl Drop for Cleanup {
            fn drop(&mut self) {
                if self.mapped_count > 0 {
                    // SAFETY: The virtual range `[vaddr, vaddr + mapped_count * page::SIZE)` was
                    // mapped by this call and is being unmapped on failure.
                    let status = unsafe {
                        VmAspace::kernel_aspace().arch_aspace().unmap(
                            self.vaddr,
                            self.mapped_count,
                            ARCH_UNMAP_OPTION_NONE,
                        )
                    };
                    assert!(status.is_ok());
                }
            }
        }

        const MMU_FLAGS: ArchMmuFlags =
            ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE;
        assert!(num_pages > 0);
        stack_pin_init!(let alloc_pages = PageList::new());
        let mut cleanup = Cleanup { vaddr, mapped_count: 0 };

        if self.can_attempt_contiguous_mappings() {
            let align_pages = 1usize << (self.align_log2 - page::SHIFT);
            if align_pages > 1 {
                while cleanup.mapped_count + align_pages <= num_pages {
                    stack_pin_init!(let contiguous_pages = PageList::new());
                    // Being in this path we know that `align_pages` is >1, which can only happen
                    // if our `align_log2` is greater than the system `page::SHIFT`. As such we
                    // need to allocate multiple contiguous pages at a greater than system page
                    // size alignment, and so we must use the general `pmm::alloc_contiguous`.
                    let Ok(paddr) = contiguous_pages.as_mut().alloc_contiguous(
                        align_pages,
                        0,
                        self.align_log2 as u8,
                    ) else {
                        // Failing to allocate a contiguous block isn't an error, as the pmm could
                        // just be fragmented. Drop out of this loop and attempt to finish with the
                        // single page mappings.
                        break;
                    };
                    contiguous_pages.as_mut().set_state(self.allocated_page_state);
                    alloc_pages.as_mut().splice_front(contiguous_pages.as_mut());
                    // SAFETY: `vaddr + mapped_count * page::SIZE` is an unmapped range owned by
                    // this `VirtualAlloc`, and `paddr` is a contiguous run of `align_pages`
                    // allocated from the PMM.
                    unsafe {
                        VmAspace::kernel_aspace().arch_aspace().map_contiguous(
                            vaddr + cleanup.mapped_count * page::SIZE,
                            paddr,
                            align_pages,
                            MMU_FLAGS,
                        )?;
                    }
                    cleanup.mapped_count += align_pages;
                }
                if cleanup.mapped_count == num_pages {
                    cleanup.cancel();
                    alloc_pages.as_mut().forget_pages();
                    return Ok(());
                }
            }
        }

        // Allocate any remaining pages.
        stack_pin_init!(let remaining_pages = PageList::new());
        remaining_pages.as_mut().alloc_pages(num_pages - cleanup.mapped_count, 0)?;

        // Place them specifically at the end of any already allocated pages. This ensures that if
        // we should iterate too far we will hit a null page and not one of our contiguous pages to
        // ensure we can never attempt to map something twice. Due to how list_node's work this
        // does not affect the current_page pointer we already retrieved.
        let mut current_page = alloc_pages.as_mut().splice_back(remaining_pages.as_mut());
        assert!(current_page.is_valid());

        while cleanup.mapped_count < num_pages {
            const BATCH_PAGES: usize = 128;
            let mut paddrs = [PAddr(0); BATCH_PAGES];
            let map_pages = core::cmp::min(BATCH_PAGES, num_pages - cleanup.mapped_count);
            assert!(map_pages > 0);
            for paddr in paddrs.iter_mut().take(map_pages) {
                assert!(current_page.is_valid());
                current_page.set_state(self.allocated_page_state);
                *paddr = current_page.paddr();
                current_page.move_next();
            }

            // SAFETY: `vaddr + mapped_count * page::SIZE` is an unmapped range owned by this
            // `VirtualAlloc`, and `paddrs` contains `map_pages` valid physical addresses.
            unsafe {
                VmAspace::kernel_aspace().arch_aspace().map(
                    vaddr + cleanup.mapped_count * page::SIZE,
                    paddrs.as_mut_ptr(),
                    map_pages,
                    MMU_FLAGS,
                    ExistingEntryAction::Error,
                )?;
            }
            cleanup.mapped_count += map_pages;
        }
        // If we successfully mapped everything we should have iterated all the way to the end of
        // the pages we allocated.
        assert!(!current_page.is_valid());

        cleanup.cancel();
        alloc_pages.as_mut().forget_pages();

        ltracef!("Mapped {num_pages} pages at {vaddr:x}\n");
        Ok(())
    }

    fn destroy(&mut self) {
        self.canary.assert();
        // No need to destroy if not yet initialized.
        if self.alloc_base == 0 {
            return;
        }

        let bitmap_pages = self.bitmap_pages();
        // Check that all allocated blocks were freed. Outstanding allocations indicate something
        // is still holding a reference that they will try and use later, so we have no choice but
        // to fail. There are more optimal ways to track outstanding allocations, but as destroying
        // allocators is considered an extremely uncommon operation (largely just when running
        // tests) this O(N) scan is fine. This check needs to ignore the pages for the bitmap
        // itself, as they should still be set.
        assert!(self.bitmap.scan(bitmap_pages, self.bitmap.size(), false).is_none());

        // Release the pages backing the bitmap.
        self.bitmap.storage_mut().reset();
        self.unmap_free_pages(self.alloc_base, bitmap_pages);
        self.alloc_base = 0;
    }

    /// Returns whether or not the allocator will attempt to optimize large allocations with
    /// contiguous allocations and mappings that can get promoted to large pages. It does not
    /// guarantee that it will succeed at doing this though.
    pub fn can_attempt_contiguous_mappings(&self) -> bool {
        // Ideally we would like to create the chance of using large pages with aligned allocations
        // and `map_contiguous`, but we can only do this if we can later safely unmap without
        // enlarging the range.
        VmAspace::kernel_aspace().arch_aspace().unmap_only_enlarge_on_oom()
    }

    /// Returns the number of pages allocated to provide the bitmap for tracking allocations.
    /// Exposed as a debug named function as there is no promise that it has to be a bitmap for
    /// tracking and this interface is likely to change and should only be used for debugging.
    pub fn debug_bitmap_pages(&self) -> usize {
        self.bitmap_pages()
    }

    /// Frees any allocated pages as if `free_pages` had been called on every outstanding
    /// allocation. This is exposed for testing for circumstances where externally tracking every
    /// allocation is burdensome and not the goal of the test.
    pub fn debug_free_all_allocations(&mut self) {
        self.canary.assert();
        debug_assert!(self.alloc_base != 0);

        let bitmap_pages = self.bitmap_pages();

        let mut allocated_page = bitmap_pages;
        while let Some(page) = self.bitmap.scan(allocated_page, self.bitmap.size(), false) {
            allocated_page = page;
            self.free_pages(self.alloc_base + allocated_page * page::SIZE, 1);
        }
    }

    /// Force allocates the given range, overriding any padding or alignment requirements. This is
    /// used for testing to more precisely control available allocation regions. Panics if range is
    /// invalid or already allocated.
    pub fn debug_allocate_vaddr_range(&mut self, vaddr: usize, num_pages: usize) {
        self.canary.assert();
        assert!(page::is_page_rounded(vaddr));
        assert!(num_pages > 0);
        assert!(vaddr >= self.alloc_base + self.bitmap_pages() * page::SIZE);

        let index = (vaddr - self.alloc_base) >> page::SHIFT;

        assert!(self.bitmap.scan(index, index + num_pages, false).is_none());
        let _ = self.bitmap.set(index, index + num_pages);
        let _ = self.alloc_map_pages(vaddr, num_pages);
    }

    fn bitmap_pages(&self) -> usize {
        self.canary.assert();
        assert!(self.alloc_base != 0);

        self.bitmap.storage().get_size() / page::SIZE
    }
}

/// Unit tests for `VirtualAlloc`.
#[cfg(ktest)]
#[unittest::suite(name = "virtual_alloc_rust")]
mod tests {
    use super::VirtualAlloc;
    use super::page_list::{OwnedPage, PageList};
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_CACHED, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE,
        ARCH_UNMAP_OPTION_NONE, ExistingEntryAction,
    };
    use crate::vm::page_state::{VmPageState, bindings as page_bindings};
    use crate::vm::scanner::AutoVmScannerDisable;
    use crate::vm::vm_address_region::{VmAddressRegion, flag as vmar_flag};
    use crate::vm::vm_aspace::VmAspace;
    use core::ops::Deref;
    use core::sync::atomic::{AtomicU64, Ordering};
    use fbl::RefPtr;
    use pin_init::stack_pin_init;
    use unittest::{
        assert_err, assert_false, assert_ok, assert_true, expect_eq, expect_err, expect_false,
        expect_gt, expect_lt, expect_ne, expect_ok, expect_true, unwrap_ok,
    };
    use zx_status::Status;

    const MB: usize = 1024 * 1024;
    const GB: usize = 1024 * 1024 * 1024;
    const TEST_HEAP_SIZE: usize = 16 * MB;
    // We pick 2MiB for the heap alignment as this is a large page on many architectures. Having
    // this be a large page is not actually necessary for this test coverage, but it serves as some
    // bonus mmu testing so we might as well.
    const TEST_HEAP_ALIGN_LOG2: usize = 21;
    const ALIGN_PAGES: usize = 1usize << (TEST_HEAP_ALIGN_LOG2 - page::SHIFT);

    // Helper that checks if there exists any contiguous blocks in the pmm. This can be used by
    // tests to attempt to avoid spurious failures.
    fn can_expect_contiguous() -> bool {
        stack_pin_init!(let pages = PageList::new());

        if pages.as_mut().alloc_contiguous(ALIGN_PAGES, 0, TEST_HEAP_ALIGN_LOG2 as u8).is_err() {
            return false;
        }

        // Allocate some additional pages, do not need to be contiguous.
        if pages.as_mut().alloc_pages(ALIGN_PAGES, 0).is_err() {
            return false;
        }

        // Returns the pages in a jumbled order. This ensures that if, in common case where the pmm
        // is running in LIFO mode, the test were to use `pmm::alloc_pages` instead of
        // `pmm::alloc_contiguous` it will be guaranteed to *not* get contiguous pages, preventing
        // any spurious success.
        //
        // See https://fxbug.dev/349402040 for context.
        let mut heads_tails = true;
        while !pages.is_empty() {
            let page =
                if heads_tails { pages.as_mut().pop_front() } else { pages.as_mut().pop_back() }
                    .expect("list is non-empty");
            drop(page);
            heads_tails = !heads_tails;
        }
        true
    }

    fn range_empty(base: usize, num_pages: usize) -> bool {
        for i in 0..num_pages {
            if VmAspace::kernel_aspace().arch_aspace().query(base + i * page::SIZE).is_ok() {
                return false;
            }
        }
        true
    }

    fn range_contiguous(base: usize, num_pages: usize) -> bool {
        assert!(num_pages > 0);
        // Get the paddr of the first page.
        let Ok((base_paddr, _)) = VmAspace::kernel_aspace().arch_aspace().query(base) else {
            return false;
        };
        // Check all subsequent pages are physically contiguous.
        for i in 1..num_pages {
            let Ok((paddr, _)) =
                VmAspace::kernel_aspace().arch_aspace().query(base + i * page::SIZE)
            else {
                return false;
            };
            if paddr.0 != base_paddr.0 + i * page::SIZE {
                return false;
            }
        }
        true
    }

    // Helper to construct and teardown a vmar for testing.
    struct TestVmar {
        vmar: RefPtr<VmAddressRegion>,
    }

    impl TestVmar {
        fn new() -> Self {
            let root_vmar =
                VmAspace::kernel_aspace().root_vmar().expect("kernel root vmar must exist");
            let vmar = root_vmar
                .create_sub_vmar(
                    0, // 0 offset to request random placement
                    TEST_HEAP_SIZE,
                    TEST_HEAP_ALIGN_LOG2 as u8,
                    vmar_flag::CAN_MAP_READ | vmar_flag::CAN_MAP_WRITE,
                    b"virtual_alloc test",
                )
                .expect("CreateSubVmar must succeed");
            Self { vmar }
        }

        fn region_empty(&self) -> bool {
            range_empty(self.vmar.base().0, self.vmar.size() / page::SIZE)
        }
    }

    impl Deref for TestVmar {
        type Target = VmAddressRegion;

        fn deref(&self) -> &Self::Target {
            &self.vmar
        }
    }

    impl Drop for TestVmar {
        fn drop(&mut self) {
            assert!(self.region_empty());
            let _ = self.vmar.destroy();
        }
    }

    // Helper for touching a pages in a virtual address range to ensure they can be accessed
    // without faulting. By default a unique value is written to and then read from every page,
    // writing can be optionally disabled to validate that the contents of the range have not
    // changed since last time.
    fn touch_pages(vaddr: usize, num_pages: usize, write: bool) -> bool {
        for page_base in (vaddr..vaddr + (num_pages * page::SIZE)).step_by(page::SIZE) {
            let ptr = core::ptr::with_exposed_provenance_mut::<u64>(page_base);
            // SAFETY: `page_base` is a page-aligned mapped virtual address owned by the caller.
            let var = unsafe { AtomicU64::from_ptr(ptr) };
            if write {
                var.store(page_base as u64, Ordering::Relaxed);
            }
            if var.load(Ordering::Relaxed) != page_base as u64 {
                return false;
            }
        }
        true
    }

    /// Test that some simple ways to use a virtual allocator work as expected.
    #[test]
    fn virtual_alloc_smoke_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 1, page::SHIFT));

        let result = alloc.alloc_pages(8);
        assert_true!(result.is_ok());
        let result = unwrap_ok!(result);
        expect_true!(touch_pages(result, 8, true));

        let result2 = alloc.alloc_pages(8);
        assert_true!(result2.is_ok());
        let result2 = unwrap_ok!(result2);
        expect_ne!(result, result2);
        expect_true!(touch_pages(result2, 8, true));
        expect_true!(touch_pages(result, 8, false));

        expect_false!(alloc.alloc_pages(GB).is_ok());

        alloc.free_pages(result, 8);
        alloc.free_pages(result2, 4);
        alloc.free_pages(result2 + 4 * page::SIZE, 4);
    }

    /// Tests minimum valid sizes for initializing the allocator.
    #[test]
    fn virtual_alloc_valid_size_test() {
        let vmar = TestVmar::new();

        {
            let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
            // This would only hold the bitmap, but not the padding for it
            assert_err!(
                alloc.init(vmar.base().0, page::SIZE, 16, page::SHIFT),
                Status::INVALID_ARGS
            );
            assert_err!(
                alloc.init(vmar.base().0, 16 * page::SIZE, 16, page::SHIFT),
                Status::INVALID_ARGS
            );
            // Would hold the bitmap and one padding.
            assert_err!(
                alloc.init(vmar.base().0, 17 * page::SIZE, 16, page::SHIFT),
                Status::INVALID_ARGS
            );
            // Would hold bitmap and two paddings, but still cannot actually allocate a page.
            assert_err!(
                alloc.init(vmar.base().0, 33 * page::SIZE, 16, page::SHIFT),
                Status::INVALID_ARGS
            );
            // Succeeds, and should support a single page of allocation.
            expect_ok!(alloc.init(vmar.base().0, 34 * page::SIZE, 16, page::SHIFT));
            let result = alloc.alloc_pages(1);
            expect_true!(result.is_ok());
            // Should not be able to do additional allocations.
            expect_false!(alloc.alloc_pages(1).is_ok());
            alloc.free_pages(unwrap_ok!(result), 1);
            expect_false!(alloc.alloc_pages(2).is_ok());
        }
        expect_true!(vmar.region_empty());

        {
            let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
            // Two allocations should have a single padding between them, not double padding. To
            // have two allocations of 1 page we should have a layout of
            // [bitmap(1)] [padding(16)] [allocation(1)] [padding(16)] [allocation(1)] [padding(16)
            // = 51.
            assert_ok!(alloc.init(vmar.base().0, 51 * page::SIZE, 16, page::SHIFT));
            let result1 = alloc.alloc_pages(1);
            expect_true!(result1.is_ok());
            let result2 = alloc.alloc_pages(1);
            expect_true!(result2.is_ok());
            expect_false!(alloc.alloc_pages(1).is_ok());
            alloc.free_pages(unwrap_ok!(result1), 1);
            alloc.free_pages(unwrap_ok!(result2), 1);
        }
        expect_true!(vmar.region_empty());
    }

    /// Tests that repeated freeing and allocating reuses the same compact virtual addresses.
    #[test]
    fn virtual_alloc_compact_test() {
        let vmar = TestVmar::new();

        const NUM_ALLOC: usize = 8;
        let mut allocs = [0usize; NUM_ALLOC];

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 2, page::SHIFT));

        for slot in &mut allocs {
            let result = alloc.alloc_pages(3);
            expect_true!(result.is_ok());
            *slot = unwrap_ok!(result);
            touch_pages(*slot, 3, true);
        }
        for &addr in &allocs {
            touch_pages(addr, 3, false);
        }

        // We should now be able to repeatedly free and alloc one of the middle allocations and
        // have it continuously give us the same virtual address.
        for _ in 0..200 {
            alloc.free_pages(allocs[NUM_ALLOC / 2], 3);
            let result = alloc.alloc_pages(3);
            expect_true!(result.is_ok());
            expect_eq!(allocs[NUM_ALLOC / 2], unwrap_ok!(result));
            touch_pages(allocs[NUM_ALLOC / 2], 3, true);
        }

        // Freeing an allocation in the middle, then the end, should first reuse the middle and
        // then the end.
        alloc.free_pages(allocs[NUM_ALLOC / 2], 3);
        alloc.free_pages(allocs[NUM_ALLOC - 1], 3);

        let result = alloc.alloc_pages(3);
        expect_true!(result.is_ok());
        expect_eq!(allocs[NUM_ALLOC / 2], unwrap_ok!(result));
        touch_pages(allocs[NUM_ALLOC / 2], 3, true);

        let result = alloc.alloc_pages(3);
        expect_true!(result.is_ok());
        expect_eq!(allocs[NUM_ALLOC - 1], unwrap_ok!(result));
        touch_pages(allocs[NUM_ALLOC - 1], 3, true);

        // Now if we free everything and realloc, should get the same starting address.
        for &addr in &allocs {
            touch_pages(addr, 3, false);
            alloc.free_pages(addr, 3);
        }

        let result = alloc.alloc_pages(3);
        expect_true!(result.is_ok());
        expect_eq!(allocs[0], unwrap_ok!(result));
        touch_pages(allocs[0], 3, true);

        // Cleanup.
        alloc.free_pages(allocs[0], 3);
    }

    /// Tests that partial frees move the search start back appropriately.
    #[test]
    fn virtual_alloc_partial_free_compact_test() {
        let vmar = TestVmar::new();

        const NUM_ALLOC: usize = 8;
        let mut allocs = [0usize; NUM_ALLOC];

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 2, page::SHIFT));

        for slot in &mut allocs {
            let result = alloc.alloc_pages(3);
            expect_true!(result.is_ok());
            *slot = unwrap_ok!(result);
            touch_pages(*slot, 3, true);
        }

        // Free one of the middle allocations, and part of the one before it.
        alloc.free_pages(allocs[NUM_ALLOC / 2], 3);
        alloc.free_pages(allocs[NUM_ALLOC / 2 - 1] + page::SIZE, 2);

        // Now when we alloc we should get something earlier than the full allocation that we
        // freed.
        let result = alloc.alloc_pages(3);
        expect_true!(result.is_ok());
        let result = unwrap_ok!(result);
        expect_lt!(result, allocs[NUM_ALLOC / 2]);
        expect_gt!(result, allocs[NUM_ALLOC / 2 - 1]);
        touch_pages(result, 3, true);

        // Finish freeing the early allocation. A new full allocation should no longer fit anywhere
        // and have to go at the end.
        alloc.free_pages(allocs[NUM_ALLOC / 2 - 1], 1);

        let result2 = alloc.alloc_pages(3);
        expect_true!(result2.is_ok());
        let result2 = unwrap_ok!(result2);
        expect_gt!(result2, allocs[NUM_ALLOC - 1]);
        touch_pages(result2, 3, true);

        // cleanup all the allocs.
        allocs[NUM_ALLOC / 2 - 1] = result;
        allocs[NUM_ALLOC / 2] = result2;

        for &addr in &allocs {
            touch_pages(addr, 3, false);
            alloc.free_pages(addr, 3);
        }
    }

    /// Tests large multi-batch allocations.
    #[test]
    fn virtual_alloc_large_alloc_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 2, page::SHIFT));

        // Create and validate some large allocations to ensure the batching in the mapping path
        // works correctly.
        let result = alloc.alloc_pages(128);
        assert_true!(result.is_ok());
        let result = unwrap_ok!(result);
        touch_pages(result, 128, true);
        alloc.free_pages(result, 128);

        let result = alloc.alloc_pages(250);
        assert_true!(result.is_ok());
        let result = unwrap_ok!(result);
        touch_pages(result, 250, true);
        alloc.free_pages(result, 250);
    }

    /// Tests rollback cleanup when an arch aspace mapping fails partway through.
    #[test]
    fn virtual_alloc_arch_alloc_failure_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 2, page::SHIFT));

        // Perform an allocation to see what virtual address we expect to be using.
        let result = alloc.alloc_pages(250);
        assert_true!(result.is_ok());
        let vaddr = unwrap_ok!(result);
        touch_pages(vaddr, 250, true);
        alloc.free_pages(vaddr, 250);

        // Let's map our own page towards the end of the allocation.
        let (_page, mut page_paddr) = unwrap_ok!(OwnedPage::alloc(0));

        // SAFETY: `vaddr + 240 * page::SIZE` is within `vmar` and unmapped, and `page_paddr` is
        // valid.
        expect_ok!(unsafe {
            VmAspace::kernel_aspace().arch_aspace().map(
                vaddr + 240 * page::SIZE,
                &mut page_paddr,
                1,
                ARCH_MMU_FLAG_CACHED | ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
                ExistingEntryAction::Error,
            )
        });

        // Attempting our allocation again should fail.
        let result = alloc.alloc_pages(250);
        expect_false!(result.is_ok());

        // No other pages should have gotten mapped in though as once our page was discovered
        // everything should have been unmapped and cleaned up.
        expect_true!(range_empty(vaddr, 240));

        // Once we unmap our page we should successfully allocate the original mapping.
        // SAFETY: `vaddr + 240 * page::SIZE` was mapped above.
        expect_ok!(unsafe {
            VmAspace::kernel_aspace().arch_aspace().unmap(
                vaddr + 240 * page::SIZE,
                1,
                ARCH_UNMAP_OPTION_NONE,
            )
        });

        let result = alloc.alloc_pages(250);
        assert_true!(result.is_ok());
        expect_eq!(vaddr, unwrap_ok!(result));
        touch_pages(vaddr, 250, true);

        alloc.free_pages(vaddr, 250);
    }

    /// Tests that allocating zero pages returns `INVALID_ARGS`.
    #[test]
    fn virtual_alloc_zero_pages_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));

        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 1, page::SHIFT));

        // Allocating zero pages is considered an error.
        expect_err!(alloc.alloc_pages(0), Status::INVALID_ARGS);
    }

    /// Tests allocator initialization error cases.
    #[test]
    fn virtual_alloc_init_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));

        // Any allocation should fail.
        expect_err!(alloc.alloc_pages(1), Status::BAD_STATE);
        expect_err!(alloc.alloc_pages(0), Status::BAD_STATE);

        // Bases and sizes need to be aligned
        expect_err!(
            alloc.init(vmar.base().0 + 1, vmar.size(), 1, page::SHIFT),
            Status::INVALID_ARGS
        );
        expect_err!(
            alloc.init(vmar.base().0 + 1, vmar.size() + 1, 1, page::SHIFT),
            Status::INVALID_ARGS
        );
        expect_err!(
            alloc.init(vmar.base().0, vmar.size() + 1, 1, page::SHIFT),
            Status::INVALID_ARGS
        );
        expect_err!(
            alloc.init(vmar.base().0 + 1, vmar.size() - 1, 1, page::SHIFT),
            Status::INVALID_ARGS
        );

        // Require at least page size alignment
        expect_err!(alloc.init(vmar.base().0, vmar.size(), 1, 0), Status::INVALID_ARGS);
        expect_err!(
            alloc.init(vmar.base().0, vmar.size(), 1, page::SHIFT - 1),
            Status::INVALID_ARGS
        );

        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 1, page::SHIFT));

        // Should not be able to re-init
        expect_err!(alloc.init(vmar.base().0, vmar.size(), 1, page::SHIFT), Status::BAD_STATE);
    }

    /// Tests aligned allocations and fallback to unaligned allocations.
    #[test]
    fn virtual_alloc_aligned_alloc_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));

        expect_ok!(alloc.init(vmar.base().0, vmar.size(), 1, TEST_HEAP_ALIGN_LOG2));

        // Create a large allocation that we will use for future allocations. Size it under the
        // assumption that the bitmap and padding will only use up a single aligned block.
        const LARGE_ALLOC_PAGES: usize = (TEST_HEAP_SIZE >> page::SHIFT) - ALIGN_PAGES * 2;
        // Validate that we have at least a few alignment multiples so that any maths we do below
        // is guaranteed to not overflow.
        zr::static_assert!(LARGE_ALLOC_PAGES / ALIGN_PAGES > 5);
        zr::static_assert!(LARGE_ALLOC_PAGES.is_multiple_of(ALIGN_PAGES));
        let result = alloc.alloc_pages(LARGE_ALLOC_PAGES);
        assert_true!(result.is_ok());

        // Record the base address of the large block. We'll use this to calculate all our test
        // areas.
        let base_test_vaddr = unwrap_ok!(result);

        // Alloc single pages until failure to ensure that in the future any allocations can only
        // succeed where we and when we want them to.
        while alloc.alloc_pages(1).is_ok() {}

        // Free a range in the middle such that with padding and alignment taken into account a
        // single large allocation would fit.
        alloc.free_pages(base_test_vaddr + (ALIGN_PAGES * 2 - 1) * page::SIZE, ALIGN_PAGES + 2);
        let contiguous = alloc.can_attempt_contiguous_mappings() && can_expect_contiguous();
        let result = alloc.alloc_pages(ALIGN_PAGES);
        assert_true!(result.is_ok());
        let result = unwrap_ok!(result);
        expect_eq!(base_test_vaddr + ALIGN_PAGES * 2 * page::SIZE, result);
        expect_true!(!contiguous || range_contiguous(result, ALIGN_PAGES));

        // Free the range and re-allocate the lowest page in the gap.
        alloc.free_pages(result, ALIGN_PAGES);
        alloc.debug_allocate_vaddr_range(base_test_vaddr + (ALIGN_PAGES * 2 - 1) * page::SIZE, 1);

        // Gap should be too small to allocate `ALIGN_PAGES` now.
        assert_false!(alloc.alloc_pages(ALIGN_PAGES).is_ok());

        // Free another page higher up. This should allow the allocation to succeed, although we
        // can make no claims on it being contiguous, since it's no longer aligned.
        alloc.free_pages(base_test_vaddr + (ALIGN_PAGES * 3 + 1) * page::SIZE, 1);
        let result = alloc.alloc_pages(ALIGN_PAGES);
        assert_true!(result.is_ok());
        alloc.free_pages(unwrap_ok!(result), ALIGN_PAGES);

        // Free a large range before and after this, but it should still use the aligned range even
        // though it creates fragmentation that prevents future allocations.
        alloc.free_pages(base_test_vaddr + ALIGN_PAGES * page::SIZE, ALIGN_PAGES);
        alloc.free_pages(base_test_vaddr + (ALIGN_PAGES * 3 + 2) * page::SIZE, ALIGN_PAGES - 2);
        let contiguous = alloc.can_attempt_contiguous_mappings() && can_expect_contiguous();
        let result = alloc.alloc_pages(ALIGN_PAGES);
        assert_true!(result.is_ok());
        let result = unwrap_ok!(result);
        expect_eq!(base_test_vaddr + ALIGN_PAGES * 2 * page::SIZE, result);
        expect_true!(!contiguous || range_contiguous(result, ALIGN_PAGES));
        expect_false!(alloc.alloc_pages(ALIGN_PAGES).is_ok());

        // We didn't properly track our allocations but it's not the goal of this test so just ask
        // the allocator to clean up for us.
        alloc.debug_free_all_allocations();
    }

    /// Tests that large aligned allocations use contiguous physical mappings when available.
    #[test]
    fn virtual_alloc_large_allocs_are_contiguous_test() {
        let _scanner_disable = AutoVmScannerDisable::new();

        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));

        expect_ok!(alloc.init(vmar.base().0, vmar.size(), 1, TEST_HEAP_ALIGN_LOG2));

        if alloc.can_attempt_contiguous_mappings() && can_expect_contiguous() {
            let result = alloc.alloc_pages(ALIGN_PAGES);
            assert_true!(result.is_ok());
            let result = unwrap_ok!(result);
            expect_true!(range_contiguous(result, ALIGN_PAGES));
            alloc.free_pages(result, ALIGN_PAGES);
        } else {
            kprint::kprintln!(
                "Failed to find contiguous range of {ALIGN_PAGES} pages, \
                 skipping test: virtual_alloc_large_allocs_are_contiguous_test"
            );
        }
    }

    /// Tests fallback to non-contiguous pages when contiguous PMM blocks are exhausted.
    #[test]
    fn virtual_alloc_contiguous_fallback_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));

        expect_ok!(alloc.init(vmar.base().0, vmar.size(), 1, TEST_HEAP_ALIGN_LOG2));

        stack_pin_init!(let pages = PageList::new());

        // Now steal all the contiguous blocks out of the pmm.
        stack_pin_init!(let temp_pages = PageList::new());
        while temp_pages
            .as_mut()
            .alloc_contiguous(ALIGN_PAGES, 0, TEST_HEAP_ALIGN_LOG2 as u8)
            .is_ok()
        {
            // Keep the first page and return the rest. This prevents us OOMing whilst still
            // blocking future allocations. We only need to hold one page since this allocation is
            // size aligned and so no page is a candidate for any other allocation.
            let head_page = temp_pages.as_mut().pop_front().expect("head_page must exist");
            pages.as_mut().push_front(head_page);
            temp_pages.as_mut().free_pages();
        }

        // A size aligned allocation should still work, as it should fall back to using
        // non-contiguous pages.
        let result = alloc.alloc_pages(ALIGN_PAGES);
        assert_true!(result.is_ok());
        alloc.free_pages(unwrap_ok!(result), ALIGN_PAGES);
    }

    /// Tests allocating and freeing the first and last valid pages of the managed range.
    #[test]
    fn virtual_alloc_free_boundary_test() {
        let vmar = TestVmar::new();

        let mut alloc = VirtualAlloc::new(VmPageState(page_bindings::vm_page_state::ALLOC));
        assert_ok!(alloc.init(vmar.base().0, vmar.size(), 1, page::SHIFT));

        let bitmap_pages = alloc.debug_bitmap_pages();
        let first_page_vaddr = vmar.base().0 + bitmap_pages * page::SIZE;
        let last_page_vaddr = vmar.base().0 + vmar.size() - page::SIZE;

        // Allocate and free the first valid page
        alloc.debug_allocate_vaddr_range(first_page_vaddr, 1);
        assert_true!(touch_pages(first_page_vaddr, 1, true));
        alloc.free_pages(first_page_vaddr, 1);

        // Allocate and free the last valid page
        alloc.debug_allocate_vaddr_range(last_page_vaddr, 1);
        assert_true!(touch_pages(last_page_vaddr, 1, true));
        alloc.free_pages(last_page_vaddr, 1);
    }
}
