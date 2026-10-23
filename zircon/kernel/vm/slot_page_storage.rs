// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::vm::compression::{MemoryUsage, VmCompressedStorage};
use crate::vm::compressor::CompressedRef;
use crate::vm::page::{VmPage, VmPageDoublyLinkedList, VmPagePtr};
use crate::vm::page_slab_allocator::{BaseSlabProvider, IdSlabAllocator, PageSlabAllocator};
use crate::vm::page_state::VmPageState;
use crate::vm::physmap::paddr_to_physmap;
use crate::vm::pmm;
use crate::vm::pmm_node::PmmOptDelayReuse;
use core::pin::Pin;
use core::ptr::{self, NonNull};
use fbl::{Canary, DoublyLinkedListContainable};
use kalloc::Box;
use kprint::kprintln;
use ksync::{KMutex, RawCriticalMutex, guarded, kcell_init};
use page_bindings::vm_page_state;
use pin_init::{PinInit, pin_data, pin_init, pin_init_array_from_fn, pinned_drop};

// We want to use a single word to store the bitmap for slot usage, so the number of available
// slots is therefore just the number of bits in a word.
// From this can calculate the size of each slot and how many bits are needed to represent an
// index into these two.

/// Number of slots that each storage page is broken into.
pub const NUM_SLOTS: usize = core::mem::size_of::<u64>() * 8;
/// Size in bytes of each slot.
pub const SLOT_SIZE: usize = page::SIZE / NUM_SLOTS;
/// Number of bits needed to represent a slot index.
const NUM_SLOT_BITS: usize = NUM_SLOTS.ilog2() as usize;
/// Number of bits needed to represent a byte offset within a slot.
const SLOT_SIZE_BITS: usize = SLOT_SIZE.ilog2() as usize;

// Every `Allocation` represents a page that has been compressed. Being able to store 2^24
// `Allocation` items means tracking 2^24 pages that have been compressed, which if uncompressed
// would represent 64GiB of data.
// Due to the implementation of the `IdSlabAllocator`, every doubling of the number of items
// doubles the amount of fixed tracking that is needed. At 2^24 items this requires approx. 1KiB
// of data, which is a very small fraction of the amount of compressed data we are able to track.
// Still, this should not be sized arbitrarily high, as the doubling of size starts to become
// meaningful, and is potentially wasting memory if not storing that many items.
const MAX_STORAGE_ITEMS: usize = 1 << 24;

/// Number of slabs needed by the `IdSlabAllocator` to store `MAX_STORAGE_ITEMS` allocations.
const SLABS_REQUIRED: usize = PageSlabAllocator::<
    { core::mem::size_of::<Allocation>() },
    BaseSlabProvider,
>::slabs_required(MAX_STORAGE_ITEMS);

/// Number of slabs needed by the `IdSlabAllocator` to track the IDs of `SLABS_REQUIRED` slabs.
const SLAB_ID_SLABS_REQUIRED: usize = PageSlabAllocator::<
    { core::mem::size_of::<NonNull<VmPage>>() },
    BaseSlabProvider,
>::slabs_required(SLABS_REQUIRED);

/// Slab allocator used to allocate the `Allocation` metadata.
type AllocationSlabAllocator = IdSlabAllocator<
    { core::mem::size_of::<Allocation>() },
    MAX_STORAGE_ITEMS,
    SLABS_REQUIRED,
    SLAB_ID_SLABS_REQUIRED,
>;

// The following helper methods are used for manipulating the free_block_mask. Slots are tracked
// where a bit being set indicates the block is not allocated, and being clear means it is
// allocated.

/// Given a zero indexed starting slot, and a non-zero length, returns a bitmask with those slots
/// set to true.
const fn slot_mask(base: u8, len: u8) -> u64 {
    debug_assert!(
        (base as usize) < NUM_SLOTS && len > 0 && (base as usize) + (len as usize) <= NUM_SLOTS
    );
    // Need to handle len == NUM_SLOTS separately to avoid an undefined shift.
    if len as usize == NUM_SLOTS {
        return u64::MAX;
    }
    ((1u64 << len) - 1) << base
}

/// Given a free block mask, finds the longest contiguous run of free bits, and returns the length
/// of that run.
const fn contig_free(mut free_mask: u64) -> u8 {
    // Handle the free_mask being all 1's separately to avoid an undefined shift in the loop.
    if free_mask == u64::MAX {
        return NUM_SLOTS as u8;
    }
    let mut max_free: u8 = 0;
    // Remove every run of 0's and 1's until the mask is fully processed, remembering the longest
    // run seen.
    while free_mask != 0 {
        free_mask >>= free_mask.trailing_zeros();
        let ones = free_mask.trailing_ones() as u8;
        if ones > max_free {
            max_free = ones;
        }
        free_mask >>= ones;
    }
    max_free
}

/// Returns the offset in `free_mask` that has a contiguous run of free (i.e. set bits) slots of at
/// least `len`. It is an error to call this if there is not such a run available.
const fn contig_base(mut free_mask: u64, len: u8) -> u8 {
    // Record how many shifts we have made, which will become the offset we return.
    let mut shifts: u8 = 0;
    let target_mask = slot_mask(0, len);
    // TODO(adanis): Consider searching for the smallest contiguous run, instead of just the first
    // one, to minimize fragmentation.
    // Keep cycling past any runs of 0's, and insufficiently long runs of 1's, until we find a run
    // that fits the target mask.
    while (free_mask & target_mask) != target_mask {
        debug_assert!(free_mask != 0);
        let ones = free_mask.trailing_ones() as u8;
        free_mask >>= ones;
        let zeroes = free_mask.trailing_zeros() as u8;
        free_mask >>= zeroes;
        shifts += ones + zeroes;
    }
    shifts
}

/// Given an item of byte size `len`, returns the number of slots required to store it.
const fn slots_needed(len: u64) -> u8 {
    debug_assert!(len > 0 && len <= page::SIZE as u64);
    (((len - 1) / SLOT_SIZE as u64) + 1) as u8
}

const fn last_slot_bytes(len: u64) -> u16 {
    debug_assert!(len > 0);
    (((len - 1) % SLOT_SIZE as u64) + 1) as u16
}

/// Returns the `free_block_mask` of `page`.
///
/// # Safety
///
/// The caller must ensure that `page` is a valid page in the ZRAM state that is owned by a
/// `VmSlotPageStorage` whose `lock` is held.
unsafe fn free_block_mask(page: VmPagePtr) -> u64 {
    // SAFETY: By this function's `# Safety` precondition, `page` is valid and its `zram.slot`
    // union member is active and only accessed with the owning storage's `lock` held.
    unsafe { (*page.as_ref().state_union.get()).zram.slot.free_block_mask }
}

/// Sets the `free_block_mask` of `page` to `mask`.
///
/// # Safety
///
/// The caller must ensure that `page` is a valid page that is owned by a `VmSlotPageStorage`
/// whose `lock` is held, and that is in, or is being placed into, the ZRAM state.
unsafe fn set_free_block_mask(page: VmPagePtr, mask: u64) {
    // SAFETY: By this function's `# Safety` precondition, `page` is valid and its `state_union` is
    // owned by the storage, which only accesses it with its `lock` held.
    unsafe { (*page.as_ref().state_union.get()).zram.slot.free_block_mask = mask };
}

/// Returns the list at `index` within the pinned array of lists `lists`.
fn list_at(
    lists: Pin<&mut [VmPageDoublyLinkedList; NUM_SLOTS]>,
    index: usize,
) -> Pin<&mut VmPageDoublyLinkedList> {
    // SAFETY: The elements of a pinned array are structurally pinned, and projecting to one of
    // them does not move it.
    unsafe { lists.map_unchecked_mut(|lists| &mut lists[index]) }
}

/// Used to track a single allocation of the underlying storage. References to this are what is
/// returned in the `CompressedRef`, and these are allocated out of a slab allocator.
// Safety invariant: `page` is a valid page in the ZRAM state that is owned by the
// `VmSlotPageStorage` that owns this `Allocation`. `1 <= num_slots`,
// `slot_start + num_slots <= NUM_SLOTS` and `1 <= last_slot_bytes <= SLOT_SIZE`, and the slots
// `[slot_start, slot_start + num_slots)` of `page` are allocated exclusively to this `Allocation`
// and hold `byte_size()` initialized bytes.
#[repr(C)]
#[derive(Debug)]
struct Allocation {
    /// Reference to the page being used by this allocation.
    page: VmPagePtr,
    /// Offset of the start slot in the page, zero indexed.
    slot_start: u8,
    /// Number of slots referenced by this data. By definition zero sized data may not be stored,
    /// and so this must always be > 0.
    num_slots: u8,
    /// Amount of data stored in the last slot. This is between [1, `SLOT_SIZE`]
    last_slot_bytes: u16,
    /// Metadata that we are obligated to track for the user for each allocation.
    metadata: u32,
}

// As we're using 8-bit values, ensure that a slot index or offset will not overflow.
zr::static_assert!(NUM_SLOT_BITS <= 8);
zr::static_assert!(SLOT_SIZE_BITS <= 16);
zr::static_assert!(core::mem::size_of::<Allocation>() == 16);

impl Allocation {
    /// The original (non-rounded) size in bytes of the data being stored.
    fn byte_size(&self) -> u64 {
        debug_assert!(self.num_slots > 0);
        debug_assert!(self.last_slot_bytes > 0 && self.last_slot_bytes as usize <= SLOT_SIZE);
        ((self.num_slots - 1) as u64 * SLOT_SIZE as u64) + self.last_slot_bytes as u64
    }

    /// Base address of the raw data.
    fn data(&self) -> *mut u8 {
        // SAFETY: By the `Allocation` safety invariant, `page` is a valid page.
        let paddr = unsafe { self.page.paddr() };
        ptr::with_exposed_provenance_mut::<u8>(
            paddr_to_physmap(paddr).0 + (self.slot_start as usize * SLOT_SIZE),
        )
    }
}

/// Query specific breakdown of the memory usage used internally by the storage system. Intended to
/// be used by tests.
#[derive(Debug, Default, Copy, Clone, PartialEq, Eq)]
pub struct InternalMemoryUsage {
    pub data_bytes: usize,
    pub metadata_bytes: usize,
}

/// Slot-page storage is an allocator optimized around the expectations, and distributions of,
/// compressed pages. At the high level it breaks a page into 64 equal slots that can be used to
/// store data.
///
/// The overall operation is closest to a buddy allocator where a page is broken into 64 slots, and
/// kept in a list indexed by the largest contiguous run of free slots. This largest contiguous run
/// represents the largest allocation supported by the page.
///
/// As free space in the page is measured only in slots, any allocation is rounded up to the next
/// slot size multiple, causing wastage due to internal fragmentation. Performing an allocation is
/// done by finding a page that has at least as many contiguous free slots as the size of the
/// allocation, allocating the run of slots, and then re-calculating the potentially changed
/// contiguous run and placing in the correct list.
///
/// There are three relevant data structures for this system:
///  1. `VmPage` `free_block_mask`: This is a bitmap, stored directly in the `VmPage`, that tracks
///     exactly which slots are free and which slots are allocated.
///  2. Contiguous free slot list: For every possible slot length [0, 64) there is a linked list of
///     pages for which that is their largest run of contiguous free slots. This is just a cache of
///     what can always be calculated from the `free_block_mask`, but this serves to provide a way
///     to find a valid page to allocate from in O(1) time, instead of scanning all pages.
///  3. `Allocation` records: This is the metadata about the specific allocation being tracked and
///     points to the `VmPage` that contains the allocation, the slot range being used, and the
///     true (unrounded) length of the data. This metadata is separately allocated using a slab
///     allocator, instead of the heap, so that a reference to it that fits in a `CompressedRef`
///     can be returned to the user.
///
/// Compressed pages, by number, tend to have a reasonably even distribution over the different
/// size buckets. However, when weighted by cumulative size, there is therefore more data to store
/// at larger compressed sizes. The use of slots allows for a balance of being able to store these
/// large items, and then packing in multiple smaller items around it, as well as being able to
/// store large amount of small items in a single page if need be.
///
/// The use of 64 slots, and the resulting 64-byte slot size for 4k pages, is a trade-off in the
/// resulting fragmentation due to rounding, and the efficiency of being able to track slots in a
/// bitmap that fits in a single machine word.
#[guarded]
#[pin_data(PinnedDrop)]
pub struct VmSlotPageStorage {
    canary: Canary<{ fbl::magic(b"SPS_") }>,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,

    /// Maintain a list of pages for every possible max contiguous free slots in those pages. The 0
    /// entry, i.e. pages that have no free slots, represent completely full pages and are tracked
    /// for consistency
    // Safety invariant: Every page in these lists is a valid page in the ZRAM state that is owned
    // by this storage, and is in the list indexed by `contig_free` of its `free_block_mask`.
    #[pin]
    #[guarded_by(lock)]
    max_contig_remain: [VmPageDoublyLinkedList; NUM_SLOTS],

    #[pin]
    #[guarded_by(lock)]
    allocator: AllocationSlabAllocator,

    /// Informational counters not required for operation, but used to provide statistics.
    #[guarded_by(lock)]
    stored_items: usize,
    #[guarded_by(lock)]
    total_compressed_item_size: usize,
    /// Number of pages in `max_contig_remain`, tracked so that reporting memory usage does not
    /// need to walk every list.
    #[guarded_by(lock)]
    data_pages: usize,
}

// SAFETY: The pages referenced by `max_contig_remain` and `allocator` are PMM pages owned by this
// storage, which may be accessed and freed from any thread, and are only accessed with `lock` held.
unsafe impl Send for VmSlotPageStorage {}
// SAFETY: All mutable state, including the pages referenced by `max_contig_remain` and
// `allocator`, is only accessed with `lock` held, so `&VmSlotPageStorage` may be shared across
// threads. The one exception is the slot data read through slices returned by `compressed_data`
// without `lock` held. This is sound as those slots are allocated exclusively to a live
// `Allocation` and are never written while it is live, and slot allocation and release are
// ordered with such reads by `lock`.
unsafe impl Sync for VmSlotPageStorage {}

#[pinned_drop]
impl PinnedDrop for VmSlotPageStorage {
    fn drop(self: Pin<&mut Self>) {
        let this = self.project();
        for list in this.max_contig_remain.get_pinned_mut().iter() {
            assert!(list.is_empty());
        }
    }
}

impl VmSlotPageStorage {
    /// Returns an initializer for an empty `VmSlotPageStorage`.
    pub fn new() -> impl PinInit<Self> {
        pin_init!(Self {
            canary: Canary::new(),
            lock <- KMutex::init(),
            max_contig_remain <- kcell_init(pin_init_array_from_fn(|_| {
                VmPageDoublyLinkedList::new()
            })),
            allocator <- kcell_init(AllocationSlabAllocator::new()),
            stored_items: 0.into(),
            total_compressed_item_size: 0.into(),
            data_pages: 0.into(),
        })
    }

    /// Allocates and initializes a new `VmSlotPageStorage` on the heap, returning `None` if the
    /// allocation fails.
    pub fn create() -> Option<Pin<Box<Self>>> {
        Box::try_pin_init(Self::new()).ok()
    }

    /// Query specific breakdown of the memory usage used internally by the storage system.
    /// Intended to be used by tests.
    pub fn get_internal_memory_usage(&self) -> InternalMemoryUsage {
        self.canary.assert();
        ksync::lock!(let guard = self.lock_lock());
        guard.get_internal_memory_usage_locked()
    }
}

impl VmSlotPageStorageLockGuard<'_> {
    fn alloc_to_ref_locked(&self, alloc: NonNull<Allocation>) -> CompressedRef {
        zr::static_assert!(
            (usize::BITS - MAX_STORAGE_ITEMS.leading_zeros()) + CompressedRef::ALIGN_BITS <= 32
        );
        CompressedRef::new(self.allocator().alloc_to_id(alloc) << CompressedRef::ALIGN_BITS)
    }

    fn ref_to_alloc_locked(&self, reference: CompressedRef) -> NonNull<Allocation> {
        self.allocator().id_to_alloc(reference.value() >> CompressedRef::ALIGN_BITS)
    }

    fn get_internal_memory_usage_locked(&self) -> InternalMemoryUsage {
        let fields = self.fields();
        let data_bytes = *fields.data_pages * page::SIZE;
        let metadata_bytes = fields.allocator.memory_usage();
        InternalMemoryUsage { data_bytes, metadata_bytes }
    }
}

// SAFETY:
// 1. `store` only returns the caller's `page` when it has not been consumed, in which case it was
//    only read from. It is then still a valid, uniquely owned PMM page in the `Alloc` state that
//    is not in any list, and by the kernel physmap invariant its physmap mapping holds
//    `page::SIZE` initialized bytes.
// 2. References are an `IdSlabAllocator` ID, which is less than `MAX_STORAGE_ITEMS` (2^24),
//    shifted by `CompressedRef::ALIGN_BITS`, and so are never equal to
//    `VmCompression::TEMP_REFERENCE_VALUE`. The `Allocation` they refer to is only released by
//    `free`.
// 3. `compressed_data` returns a slice into the slots of a page owned by this storage. These slots
//    are not reallocated or modified, and the page is not freed, until the reference is passed to
//    `free`, and pages owned by this storage are never returned from `store`.
unsafe impl VmCompressedStorage for VmSlotPageStorage {
    /// # Safety
    ///
    /// The caller must uphold the `# Safety` contract of [`VmCompressedStorage::store`].
    unsafe fn store(
        &self,
        page: VmPagePtr,
        len: usize,
    ) -> (Option<CompressedRef>, Option<VmPagePtr>) {
        // SAFETY: By this method's `# Safety` precondition, `page` is a valid page.
        debug_assert!(!unsafe { page.as_ref() }.get_node().in_container());

        self.canary.assert();
        ksync::lock!(let mut guard = self.lock_lock());

        // Require an `Allocation` for our metadata.
        let Ok(data) = guard.as_mut().allocator_mut().allocate_object::<Allocation>() else {
            return (None, Some(page));
        };

        let fields = guard.as_mut().fields_mut();
        // Allocation cannot fail from here, so update our stats.
        *fields.total_compressed_item_size += len;
        *fields.stored_items += 1;

        let slots = slots_needed(len as u64);
        let last_slot_bytes = last_slot_bytes(len as u64);

        let mut max_contig_remain = fields.max_contig_remain;
        // Search for an existing page to add to first, preferring to break up the smallest
        // contiguous slot run possible.
        let target_list = (slots as usize..NUM_SLOTS).find(|&i| !max_contig_remain[i].is_empty());
        let Some(target_list) = target_list else {
            // No existing page available, convert the provided `page` into a zram page.
            // SAFETY: By this method's `# Safety` precondition, `page` is a valid page.
            debug_assert!(!unsafe { page.as_ref() }.get_node().in_container());
            // SAFETY: By this method's `# Safety` precondition, `page` is uniquely owned by the
            // caller, which has transferred ownership of it to this storage.
            unsafe { page.set_state(VmPageState(vm_page_state::ZRAM)) };
            // `page` had the data starting at offset 0, so the bottom slots are initially
            // allocated, meaning the remainder are free.
            let contig_free = NUM_SLOTS as u8 - slots;
            // An item that needs every slot leaves no free slots, which `slot_mask` cannot express.
            let free_mask = if contig_free == 0 { 0 } else { slot_mask(slots, contig_free) };
            // SAFETY: `page` is now owned by this storage in the ZRAM state, and `lock` is held.
            unsafe { set_free_block_mask(page, free_mask) };
            // SAFETY: `allocate_object` returned storage that is valid for writes of an
            // `Allocation` and that is not referenced by anything else. Slots `[0, slots)` of
            // `page`, which hold the `len` bytes of data, are allocated exclusively to it.
            unsafe {
                data.write(Allocation {
                    page,
                    slot_start: 0,
                    num_slots: slots,
                    last_slot_bytes,
                    metadata: 0,
                });
            }
            // SAFETY: By this method's `# Safety` precondition `page` is not in any list, and it is
            // owned by this storage and remains valid until `free` removes it from the list before
            // returning it to the PMM.
            unsafe {
                list_at(max_contig_remain.as_mut(), contig_free as usize)
                    .push_front_raw(page.as_non_null());
            }
            *fields.data_pages += 1;
            // We have consumed `page`, so do not return it to the caller.
            return (Some(guard.alloc_to_ref_locked(data)), None);
        };

        // Found a non-empty list, remove a page from it under the assumption that its max
        // contiguous slot run will change and so it's going to move lists anyway.
        let target_page = VmPagePtr::new(
            list_at(max_contig_remain.as_mut(), target_list)
                .pop_front()
                .expect("target list is not empty"),
        );
        // Find that part of the free mask to allocate from, and remove those slots from it.
        // SAFETY: By the `max_contig_remain` safety invariant, `target_page` is a valid page in the
        // ZRAM state owned by this storage, and `lock` is held.
        let base = contig_base(unsafe { free_block_mask(target_page) }, slots);
        let alloc_mask = slot_mask(base, slots);
        // SAFETY: As above.
        unsafe { set_free_block_mask(target_page, free_block_mask(target_page) & !alloc_mask) };
        let alloc = Allocation {
            page: target_page,
            slot_start: base,
            num_slots: slots,
            last_slot_bytes,
            metadata: 0,
        };
        let dst = alloc.data();
        // SAFETY: `allocate_object` returned storage that is valid for writes of an `Allocation`
        // and that is not referenced by anything else. The slots described by `alloc` were
        // allocated exclusively to it above, and are filled with the `len` bytes of data below,
        // before `data` is converted into a `CompressedRef` and `lock` is released.
        unsafe { data.write(alloc) };
        // Copy the actual data.
        // SAFETY: By this method's `# Safety` precondition, `page` is a valid page whose physmap
        // mapping contains at least `len` initialized bytes. `dst` points to the `slots` slots
        // just allocated in `target_page`, which hold at least `len` bytes. These do not overlap
        // `page`, as `target_page` is a different page owned by this storage and the physmap maps
        // distinct physical pages to disjoint addresses.
        unsafe {
            ptr::copy_nonoverlapping(
                ptr::with_exposed_provenance::<u8>(paddr_to_physmap(page.paddr()).0),
                dst,
                len,
            );
        }
        // Re-insert the page into the, possibly changed, target list.
        // SAFETY: As above, `target_page` is a valid page in the ZRAM state owned by this storage.
        let contig_free = contig_free(unsafe { free_block_mask(target_page) });
        // SAFETY: `target_page` was removed from its list above, so is not in any list, and it
        // remains valid until `free` removes it from the list before returning it to the PMM.
        unsafe {
            list_at(max_contig_remain.as_mut(), contig_free as usize)
                .push_front_raw(target_page.as_non_null());
        }
        // We copied out of `page` so return it to the caller.
        (Some(guard.alloc_to_ref_locked(data)), Some(page))
    }

    /// # Safety
    ///
    /// The caller must uphold the `# Safety` contract of [`VmCompressedStorage::free`].
    unsafe fn free(&self, reference: CompressedRef) {
        self.canary.assert();
        let page = {
            ksync::lock!(let mut guard = self.lock_lock());

            // Lookup the metadata for this allocation.
            let data = guard.ref_to_alloc_locked(reference);
            // SAFETY: By this method's `# Safety` precondition, `reference` is live and not used
            // again, so `data` points to an initialized `Allocation`, which is only accessed with
            // `lock` held, and which is moved out here before `data` is released below.
            let alloc = unsafe { data.as_ptr().read() };
            let page = alloc.page;
            // SAFETY: By the `Allocation` safety invariant, `page` is a valid page.
            debug_assert!(unsafe { page.as_ref() }.get_node().in_container());

            let fields = guard.as_mut().fields_mut();
            // Update stats tracking.
            *fields.total_compressed_item_size -= alloc.byte_size() as usize;
            *fields.stored_items -= 1;

            // Save the old contig free slot count so we know which list the page was in.
            // SAFETY: By the `Allocation` safety invariant, `page` is a valid page in the ZRAM
            // state owned by this storage, and `lock` is held.
            let old_contig_free = contig_free(unsafe { free_block_mask(page) });

            // Add the slots for this allocation back to the free mask of the page and then can
            // release the `Allocation`.
            let free_mask = slot_mask(alloc.slot_start, alloc.num_slots);
            // SAFETY: As above.
            unsafe { set_free_block_mask(page, free_block_mask(page) | free_mask) };
            // SAFETY: `data` was allocated from `allocator`, and the `Allocation` it held was moved
            // out above and is not used again.
            unsafe { fields.allocator.deallocate_bytes(data.cast()) };

            // Remove the page from its current list.
            let mut max_contig_remain = fields.max_contig_remain;
            // SAFETY: By the `max_contig_remain` safety invariant, `page` is in the list indexed by
            // its previous `contig_free`.
            let erased = unsafe {
                list_at(max_contig_remain.as_mut(), old_contig_free as usize).erase(page.as_ref())
            };
            debug_assert!(erased == Some(page.as_non_null()));
            // SAFETY: As above, `page` is a valid page in the ZRAM state owned by this storage.
            let contig_free = contig_free(unsafe { free_block_mask(page) });
            if contig_free as usize != NUM_SLOTS {
                // SAFETY: `page` was removed from its list above, so is not in any list, and it
                // remains valid until a later `free` removes it from the list before returning it
                // to the PMM.
                unsafe {
                    list_at(max_contig_remain.as_mut(), contig_free as usize)
                        .push_front_raw(page.as_non_null());
                }
                // Page still in use, do not let it get freed.
                None
            } else {
                *fields.data_pages -= 1;
                Some(page)
            }
        };
        // If we ended up with a completely free page, give it back to the PMM.
        if let Some(page) = page {
            // SAFETY: `page` has no allocated slots and is not in any list, so this storage is its
            // sole owner and no longer references it.
            unsafe { pmm::node().free_page(page, PmmOptDelayReuse::Default) };
        }
    }

    /// # Safety
    ///
    /// The caller must uphold the `# Safety` contract of [`VmCompressedStorage::compressed_data`].
    unsafe fn compressed_data(&self, reference: CompressedRef) -> (&[u8], u32) {
        self.canary.assert();
        ksync::lock!(let guard = self.lock_lock());
        let data = guard.ref_to_alloc_locked(reference);
        // SAFETY: By this method's `# Safety` precondition, `reference` is live, so `data` points
        // to an initialized `Allocation`, which is only accessed with `lock` held.
        let data = unsafe { data.as_ref() };
        // SAFETY: By the `Allocation` safety invariant, `data.data()` points to `data.byte_size()`
        // bytes in slots of a storage-owned page that are allocated exclusively to this
        // allocation, so `store` never writes them; by this method's `# Safety` precondition
        // `reference` is not freed (so the page is not released) while the returned slice is in
        // use.
        let slice = unsafe { core::slice::from_raw_parts(data.data(), data.byte_size() as usize) };
        (slice, data.metadata)
    }

    /// # Safety
    ///
    /// The caller must uphold the `# Safety` contract of [`VmCompressedStorage::get_metadata`].
    unsafe fn get_metadata(&self, reference: CompressedRef) -> u32 {
        self.canary.assert();
        ksync::lock!(let guard = self.lock_lock());
        // SAFETY: By this method's `# Safety` precondition, `reference` is live, so the returned
        // pointer points to an initialized `Allocation`, which is only accessed with `lock` held.
        unsafe { guard.ref_to_alloc_locked(reference).as_ref().metadata }
    }

    /// # Safety
    ///
    /// The caller must uphold the `# Safety` contract of [`VmCompressedStorage::set_metadata`].
    unsafe fn set_metadata(&self, reference: CompressedRef, metadata: u32) {
        self.canary.assert();
        ksync::lock!(let guard = self.lock_lock());
        // SAFETY: By this method's `# Safety` precondition, `reference` is live, so the returned
        // pointer points to an initialized `Allocation`, which is only accessed with `lock` held.
        unsafe { (*guard.ref_to_alloc_locked(reference).as_ptr()).metadata = metadata };
    }

    fn dump(&self) {
        self.canary.assert();

        let usage = self.get_memory_usage();
        kprintln!(
            "Storing {} bytes compressed to {} bytes using {} bytes of storage",
            usage.uncompressed_content_bytes,
            usage.compressed_storage_used_bytes,
            usage.compressed_storage_bytes
        );
    }

    fn get_memory_usage(&self) -> MemoryUsage {
        self.canary.assert();
        ksync::lock!(let guard = self.lock_lock());
        let usage = guard.get_internal_memory_usage_locked();
        let fields = guard.fields();
        MemoryUsage {
            uncompressed_content_bytes: (*fields.stored_items * page::SIZE) as u64,
            compressed_storage_bytes: (usage.data_bytes + usage.metadata_bytes) as u64,
            compressed_storage_used_bytes: *fields.total_compressed_item_size as u64,
        }
    }
}
