// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! # Porting Note
//!
//! The C++ and Rust versions of `PageSlabAllocator` and its associated types are not layout
//! identical.

use crate::kernel::types::VAddr;
use crate::vm::page::{VmPage, VmPageDoublyLinkedList, VmPagePtr, VmPageSlabState};
use crate::vm::page_state::VmPageState;
use crate::vm::{heap, physmap, pmm};
use core::alloc::Layout;
use core::convert::Infallible;
use core::error::Error;
use core::ffi::c_void;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::{fmt, mem};
use fbl::DoublyLinkedListContainable;
use page_bindings::vm_page_state;
use pin_init::{PinInit, pin_data, pin_init};
use zx_status::Status;

#[derive(Debug)]
pub struct SlabAllocationError(());

impl fmt::Display for SlabAllocationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("failed to allocate slab")
    }
}

impl Error for SlabAllocationError {}

impl From<SlabAllocationError> for Status {
    fn from(_: SlabAllocationError) -> Self {
        Status::NO_MEMORY
    }
}

pub trait SlabProvider {
    fn alloc_slab(self: Pin<&mut Self>) -> Result<NonNull<VmPage>, SlabAllocationError>;
    /// # Safety
    ///
    /// The argument `slab` must have been previously returned by `alloc_slab`. It must not have
    /// been freed.
    unsafe fn free_slab(self: Pin<&mut Self>, slab: NonNull<VmPage>);
}

pub struct BaseSlabProvider {}

impl SlabProvider for BaseSlabProvider {
    fn alloc_slab(self: Pin<&mut Self>) -> Result<NonNull<VmPage>, SlabAllocationError> {
        let (page, _pa) = pmm::alloc_page(0).map_err(|_status| SlabAllocationError(()))?;
        let page = page.as_non_null();
        // SAFETY: We own this freshly allocated page.
        unsafe {
            page.as_ref().set_state(VmPageState(vm_page_state::SLAB));
            let slab = &mut *slab_state_mut(page);

            // There is enough space to store a cookie per allocation in the slab, so
            // amortize it and record a per slab cookie. On average this should have every different
            // call site using this allocator to get proportional blame.
            slab.profile_cookie = heap::profile_track_alloc(page::SIZE);
        }
        Ok(page)
    }

    unsafe fn free_slab(self: Pin<&mut Self>, slab: NonNull<VmPage>) {
        // SAFETY: Caller is giving us ownership of this page.
        unsafe {
            debug_assert_eq!(slab.as_ref().state(), VmPageState(vm_page_state::SLAB));

            let page = &*slab_state_mut(slab);
            heap::profile_track_free(page.profile_cookie, page::SIZE);

            // SAFETY: We have ownership over this page.
            pmm::free_page(VmPagePtr::new(slab));
        }
    }
}

/// # Safety
///
/// The caller must possess conceptual ownership of `slab` and ensure that `state_union` is
/// in the `SLAB` state, and that accessing this subfield does not race with concurrent access.
unsafe fn slab_state_mut(slab: NonNull<VmPage>) -> *mut VmPageSlabState {
    // SAFETY: Safety deferred to caller per function safety preconditions.
    unsafe { &raw mut (*(*slab.as_ptr()).state_union.get()).slab }
}

union Entry<const ALLOC_SIZE: usize> {
    next: u32,
    storage: [u8; ALLOC_SIZE],
}

/// Simple slab allocator that uses a `VmPage` as its slab to perform allocations out of. This makes
/// the allocator only suitable for small allocations, preferably ones that divide evenly into a
/// page.
///
/// All per-slab metadata is stored in the `VmPage` itself and, by default, the only dependency of
/// the allocator is the `pmm` to allocate and free pages from. The heap is not needed for any other
/// metadata allocations.
///
/// Free regions are tracked in a two level list with each slab having an internal list of free
/// regions, and the allocator itself having a list of slabs that have at least one free region.
///
/// Slabs that become fully empty are able to be returned to the `pmm`, although allocations
/// cannot be moved between slabs, so fragmentation can still occur.
///
/// The contained slab provider can be edited to control the actual allocation and freeing of
/// the slabs themselves.
///
/// This class is not thread safe.
///
/// See the module-level documentation.
#[pin_data(PinnedDrop)]
pub struct PageSlabAllocator<const ALLOC_SIZE: usize, A> {
    #[pin]
    full_slabs: VmPageDoublyLinkedList,
    #[pin]
    available_slabs: VmPageDoublyLinkedList,
    /// Track the total number of allocated slabs (i.e. pages), both full and available.
    allocated_slabs: usize,
    /// This is permitted to be an address-sensitive type.
    #[pin]
    inner: A,
}

impl<const ALLOC_SIZE: usize, A> PageSlabAllocator<ALLOC_SIZE, A> {
    pub const ALLOCS_PER_SLAB: usize = page::SIZE / ALLOC_SIZE;
    const END_OF_LIST: u32 = u32::MAX;

    pub fn new_with(inner: impl PinInit<A, Infallible>) -> impl PinInit<Self, Infallible> {
        const {
            assert!(mem::size_of::<Entry<ALLOC_SIZE>>() == ALLOC_SIZE);
            assert!(ALLOC_SIZE < page::SIZE);
        }

        pin_init!(Self {
            full_slabs <- VmPageDoublyLinkedList::new(),
            available_slabs <- VmPageDoublyLinkedList::new(),
            allocated_slabs: 0,
            inner <- inner,
        })
    }

    /// Allocates an area of uninitialized memory of AllocSize and returns a pointer to it, or
    /// returns an error.
    pub fn allocate_bytes(self: Pin<&mut Self>) -> Result<NonNull<c_void>, SlabAllocationError>
    where
        A: SlabProvider,
    {
        let entry = self.allocate()?;
        Ok(entry.cast())
    }

    /// Allocates an area of uninitialized memory capable of holding a single object of type `T`.
    /// This is largely a convenience wrapper around `allocate_bytes` that validates `T` is
    /// compatible with the size and alignment of the allocations.
    ///
    /// Returns an error on failure.
    pub fn allocate_object<T>(self: Pin<&mut Self>) -> Result<NonNull<T>, SlabAllocationError>
    where
        A: SlabProvider,
    {
        const {
            let layout = Layout::new::<T>();
            // T must fit inside the size of the allocation.
            assert!(layout.size() <= ALLOC_SIZE);
            // Allocations must have at least equivalent alignment.
            assert!(layout.align() <= Self::entry_align());
        }
        let entry = self.allocate()?;
        Ok(entry.cast())
    }

    /// Deallocates the storage referenced by `ptr`, which must be a pointer obtained by an earlier
    /// call to `allocate_object` or `allocate_bytes`.
    ///
    /// # Safety
    ///
    /// `ptr` must be a pointer obtained from this allocator that has not already been deallocated.
    pub unsafe fn deallocate_bytes(self: Pin<&mut Self>, ptr: NonNull<c_void>)
    where
        A: SlabProvider,
    {
        // SAFETY: Caller guarantees `ptr` was allocated by this allocator and not yet freed.
        unsafe {
            self.free(ptr);
        }
    }

    pub fn allocated_slabs(&self) -> usize {
        self.allocated_slabs
    }

    const fn entry_align() -> usize {
        1usize << mem::size_of::<Entry<ALLOC_SIZE>>().trailing_zeros()
    }

    fn alloc_to_slab<T>(ptr: NonNull<T>) -> (NonNull<VmPage>, u32) {
        let ptr: usize = ptr.addr().into();
        let offset = ptr % page::SIZE;
        let page = pmm::paddr_to_vm_page(physmap::physmap_to_paddr(VAddr(ptr)))
            .expect("allocated pointer maps to valid vm_page");
        let page: NonNull<VmPage> = page.as_non_null();
        // SAFETY: `page` is a valid page allocated by this allocator.
        let page_ref = unsafe { page.as_ref() };
        assert_eq!(page_ref.state(), VmPageState(vm_page_state::SLAB));
        debug_assert!(page_ref.get_node().in_container());
        debug_assert!(offset.is_multiple_of(ALLOC_SIZE));
        (page, (offset / ALLOC_SIZE) as u32)
    }

    /// # Safety
    ///
    /// `slab` must have come from this allocator.
    unsafe fn get_entry(slab: NonNull<VmPage>, index: u32) -> NonNull<Entry<ALLOC_SIZE>> {
        // SAFETY: Caller guarantees `slab` came from this allocator.
        let page = unsafe { slab.as_ref() };
        assert_eq!(page.state(), VmPageState(vm_page_state::SLAB));
        debug_assert!((index as usize) < Self::ALLOCS_PER_SLAB);
        let ptr = physmap::paddr_to_physmap(page.paddr());
        let ptr = ptr::with_exposed_provenance_mut::<Entry<ALLOC_SIZE>>(ptr.0);
        // SAFETY: `slab` has been allocated in the physmap and `index` is within the slab bounds.
        unsafe { NonNull::new_unchecked(ptr.add(index as usize)) }
    }

    fn add_slab(self: Pin<&mut Self>) -> Result<(), SlabAllocationError>
    where
        A: SlabProvider,
    {
        let this = self.project();
        // Allocate a new slab.
        let slab = this.inner.alloc_slab()?;

        // SAFETY: We own the freshly allocated slab and `slab_state` is valid for writes.
        unsafe {
            let slab_state = slab_state_mut(slab);
            (*slab_state).free_slot = Self::END_OF_LIST;
            (*slab_state).peak_allocated = 0;
            (*slab_state).allocated = 0;
        }

        // Insert it into the available slabs.
        // SAFETY: `slab` is valid for insertion and not in any other list.
        unsafe {
            this.available_slabs.get_unchecked_mut().push_front_raw(slab);
        }

        *this.allocated_slabs += 1;
        Ok(())
    }

    fn allocate(mut self: Pin<&mut Self>) -> Result<NonNull<Entry<ALLOC_SIZE>>, SlabAllocationError>
    where
        A: SlabProvider,
    {
        // See if there are any slabs available.
        if self.available_slabs.is_empty() {
            self.as_mut().add_slab()?;
        }

        let mut this = self.project();

        // SAFETY: `available_slabs` is pinned and not moved.
        let mut cursor =
            unsafe { this.available_slabs.as_mut().get_unchecked_mut().cursor_front_mut() };

        let page = NonNull::from(cursor.get().expect("available slabs list has elements"));

        let entry;
        // SAFETY: `page` is valid for reads.
        let free_slot = unsafe { (*slab_state_mut(page)).free_slot };

        if free_slot == Self::END_OF_LIST {
            // SAFETY: `page` is valid for reads.
            let peak = unsafe { (*slab_state_mut(page)).peak_allocated };
            debug_assert!((peak as usize) < Self::ALLOCS_PER_SLAB);

            // SAFETY: `page` is valid for entry lookup and `peak` is within bounds.
            entry = unsafe { Self::get_entry(page, peak) };

            // SAFETY: `page` is valid for writes.
            unsafe {
                (*slab_state_mut(page)).peak_allocated += 1;
            }
        } else {
            // SAFETY: `page` is valid for entry lookup and `free_slot` is within bounds.
            entry = unsafe { Self::get_entry(page, free_slot) };

            // SAFETY: `page` is valid for writes and `entry` is valid for reads.
            unsafe {
                (*slab_state_mut(page)).free_slot = (*entry.as_ptr()).next;
            }
        }

        // SAFETY: `page` is valid for writes.
        unsafe {
            (*slab_state_mut(page)).allocated += 1;
        }

        // SAFETY: `page` is valid for reads.
        let free_slot = unsafe { (*slab_state_mut(page)).free_slot };
        // SAFETY: `page` is valid for reads.
        let peak = unsafe { (*slab_state_mut(page)).peak_allocated };
        // SAFETY: `page` is valid for reads.
        let allocated = unsafe { (*slab_state_mut(page)).allocated };

        if free_slot == Self::END_OF_LIST && (peak as usize) == Self::ALLOCS_PER_SLAB {
            debug_assert_eq!(allocated as usize, Self::ALLOCS_PER_SLAB);
            let page = cursor.erase().unwrap();
            // SAFETY: `full_slabs` is pinned and not moved.
            let full_slabs = unsafe { this.full_slabs.get_unchecked_mut() };
            // SAFETY: `page` is valid for insertion and not in any list.
            unsafe {
                full_slabs.push_front_raw(page);
            }
        } else {
            debug_assert!((allocated as usize) < Self::ALLOCS_PER_SLAB);
        }

        Ok(entry)
    }

    unsafe fn free(mut self: Pin<&mut Self>, ptr: NonNull<c_void>)
    where
        A: SlabProvider,
    {
        let this = self.as_mut().project();
        // SAFETY: `available_slabs` is pinned and not moved.
        let available_slabs = unsafe { this.available_slabs.get_unchecked_mut() };
        // SAFETY: `full_slabs` is pinned and not moved.
        let full_slabs = unsafe { this.full_slabs.get_unchecked_mut() };

        // Lookup the slab this was allocated in.
        let (slab, index) = Self::alloc_to_slab(ptr);

        // SAFETY: `slab` is valid for reads.
        let free_slot = unsafe { (*slab_state_mut(slab)).free_slot };
        // SAFETY: `slab` is valid for reads.
        let allocated = unsafe { (*slab_state_mut(slab)).allocated };

        // This will only catch the most egregious kinds of double-frees, but is better than
        // nothing.
        debug_assert_ne!(free_slot, index);
        debug_assert!(allocated > 0);

        if allocated == 1 {
            // Slab has become empty, can free it.
            // SAFETY: `slab` is valid for removal and present in `available_slabs`.
            unsafe {
                available_slabs.erase(slab.as_ref());
            }
            *this.allocated_slabs -= 1;
            // SAFETY: `slab` is valid to free and not in any list.
            unsafe {
                this.inner.free_slab(slab);
            }
            return;
        }

        if allocated as usize == Self::ALLOCS_PER_SLAB {
            // Slab is going from full to having space available, move to the correct list. We place
            // at the back of the list to encourage allocations, which happen on the head, to fill
            // up a page instead of constantly bouncing allocations into different, partially full,
            // pages.
            // SAFETY: `slab` is valid for list transfer from `full_slabs` to `available_slabs`.
            unsafe {
                full_slabs.erase(slab.as_ref());
                available_slabs.push_back_raw(slab);
            }
        }

        // Update the free list for this slab.

        // SAFETY: `slab` is valid for entry lookup and `index` is within bounds.
        let entry = unsafe { Self::get_entry(slab, index) };

        // SAFETY: `slab_state` and `entry` are valid for writes.
        unsafe {
            let slab_state = slab_state_mut(slab);
            (*slab_state).allocated -= 1;

            (*entry.as_ptr()).next = (*slab_state).free_slot;
            (*slab_state).free_slot = index;
        }
    }

    pub fn provider(&self) -> &A {
        &self.inner
    }

    pub fn debug_free_all_slabs(self: Pin<&mut Self>) {
        let this = self.project();
        for p in this.full_slabs.iter() {
            // SAFETY: `p` is valid for reads.
            let slab = unsafe { &*slab_state_mut(NonNull::from(p)) };
            heap::profile_track_free(slab.profile_cookie, page::SIZE);
        }
        for p in this.available_slabs.iter() {
            // SAFETY: `p` is valid for reads.
            let slab = unsafe { &*slab_state_mut(NonNull::from(p)) };
            heap::profile_track_free(slab.profile_cookie, page::SIZE);
        }
        // SAFETY: Slabs in full_slabs and available_slabs are valid allocated PMM pages.
        // We do not move the DoublyLinkedList containers out of their pinned location.
        unsafe {
            pmm::free_list(this.full_slabs);
            pmm::free_list(this.available_slabs);
        }
    }
}

#[pin_init::pinned_drop]
impl<const ALLOC_SIZE: usize, A> PinnedDrop for PageSlabAllocator<ALLOC_SIZE, A> {
    fn drop(self: Pin<&mut Self>) {
        assert!(self.full_slabs.is_empty());
        assert!(self.available_slabs.is_empty());
    }
}
