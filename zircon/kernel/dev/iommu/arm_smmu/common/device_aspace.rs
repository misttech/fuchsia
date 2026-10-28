// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM SMMU stage-1 device address space.

#[cfg(not(is_kernel))]
use core::ffi::c_void;
use core::pin::Pin;
use core::sync::atomic::Ordering;

use fbl::UniquePtr;
use region_alloc::{AllowOverlap, Region, RegionAllocator, RegionSpan};
use zx_status::Status;

use super::constants::{PAGE_MASK, PAGE_SHIFT, PAGE_SIZE, PAGE_SIZE_USIZE};
use super::env::{PinnedVmoSource, SmmuEnv, SmmuPage};
use super::page_cache::PageCache;
use super::translation_table::{TranslationTableHelper, table_slice};
use super::vmsav8_64::{
    get_table_entry_paddr, is_table_entry, is_valid_entry, make_page_entry, write_pte,
    zero_table_page,
};

/// Default maximum number of cached translation table pages retained by a [`DeviceAspace`] (`4`).
///
/// Try to keep around 4 pages in our page cache at all times. This way, if someone is frequently
/// pinning and unpinning a reasonably sized region (less than `2^18` pages), we'll always find the
/// pages we need in the cache (once the cache is hot), even if the mapping ends up straddling a
/// level 3 boundary. Note, this is assuming that our allocation does not also straddle a level 2
/// or level 1 boundary.
///
/// If that is ever a problem, one option would be to increase the cache to 6 pages total to avoid
/// the worst case thrash.
pub const DEFAULT_MAX_PAGE_CACHE_ENTRIES: u32 = 4;

/// An allocation in a device's aspace is a `UniquePtr` to a region from our region allocator.
pub type Allocation = UniquePtr<Region>;

/// Trait implemented by SMMU context bank TLB invalidation helpers passed into
/// [`DeviceAspace::map`], [`DeviceAspace::unmap`], and [`DeviceAspace::free_translation_tables`].
pub trait TlbInvalidator {
    /// Invalidates cached TLB entries covering `[base, base + size)` in this device address space,
    /// or the entire address space when `size == 0`.
    fn invalidate(&mut self, base: u64, size: u64);

    /// Invalidates all cached TLB entries associated with this device address space.
    #[inline]
    fn invalidate_all(&mut self) {
        self.invalidate(0, 0);
    }

    /// Returns `true` if this invalidator is properly initialized.
    ///
    /// The C++ `TlbInvalOp` is a nullable callback/context pair, and `DeviceAspace::Unmap`
    /// debug-asserts that both are set. A native Rust invalidator (e.g. the one `SmmuBti` passes
    /// in) borrows the object it invalidates through, so it cannot be "unset" in that sense and
    /// has nothing to check: this default implementation always returns `true`, which makes the
    /// corresponding `debug_assert!(tlb_inval_op.is_valid())` in [`DeviceAspace::unmap`] trivially
    /// true for such invalidators. The check is only meaningful for implementations which wrap a
    /// nullable callback, such as the `TlbInvalOp` FFI adapter used by host tests, which
    /// overrides this method.
    #[inline]
    fn is_valid(&self) -> bool {
        true
    }
}

/// Callback function pointer type for [`TlbInvalOp`].
///
/// Takes the context pointer the user constructed the [`TlbInvalOp`] with, as well as the base
/// address and size of the region in the device's address space to invalidate. `0` is passed for
/// `size` in the case that the entire device address space should be invalidated.
#[cfg(not(is_kernel))]
pub type TlbInvalCallback = unsafe extern "C" fn(ctx: *mut c_void, base: u64, size: u64);

/// Definition of a callback which can be used to invalidate TLBs at the proper point in
/// [`DeviceAspace::map`] and [`DeviceAspace::unmap`] operations.
#[cfg(not(is_kernel))]
#[repr(C)]
#[derive(Debug)]
pub struct TlbInvalOp {
    cbk: Option<TlbInvalCallback>,
    ctx: *mut c_void,
}

#[cfg(not(is_kernel))]
const _: () = {
    assert!(core::mem::size_of::<TlbInvalOp>() == 16);
    assert!(core::mem::align_of::<TlbInvalOp>() == 8);
};

#[cfg(not(is_kernel))]
impl TlbInvalOp {
    /// Constructs a [`TlbInvalOp`] from a C callback function pointer and opaque context pointer.
    ///
    /// # Safety
    ///
    /// When `cbk` is `Some(f)` and `ctx` is non-null, invoking `f(ctx, base, size)` must be sound
    /// for all [`TlbInvalOp::invalidate`] calls on the returned [`TlbInvalOp`].
    #[inline]
    pub const unsafe fn new(cbk: Option<TlbInvalCallback>, ctx: *mut c_void) -> Self {
        Self { cbk, ctx }
    }

    /// Returns `true` if both the callback and context pointer are non-null.
    #[inline]
    pub const fn is_valid(&self) -> bool {
        self.cbk.is_some() && !self.ctx.is_null()
    }

    /// Invokes the underlying callback to invalidate `[base, base + size)`.
    #[inline]
    pub fn invalidate(&mut self, base: u64, size: u64) {
        let cbk = self.cbk.expect("TlbInvalOp callback must be non-null");
        assert!(!self.ctx.is_null(), "TlbInvalOp context pointer must be non-null");
        // SAFETY: `cbk` is `Some` and `self.ctx` is non-null, so the caller of `TlbInvalOp::new`
        // guaranteed that invoking `cbk(self.ctx, base, size)` is sound.
        unsafe {
            cbk(self.ctx, base, size);
        }
    }

    /// Invokes the underlying callback with `(0, 0)` to invalidate the entire address space.
    #[inline]
    pub fn invalidate_all(&mut self) {
        self.invalidate(0, 0);
    }
}

#[cfg(not(is_kernel))]
impl TlbInvalidator for TlbInvalOp {
    #[inline]
    fn invalidate(&mut self, base: u64, size: u64) {
        Self::invalidate(self, base, size);
    }

    #[inline]
    fn invalidate_all(&mut self) {
        Self::invalidate_all(self);
    }

    #[inline]
    fn is_valid(&self) -> bool {
        Self::is_valid(self)
    }
}

/// Manages a 4-level VMSAv8-64 stage-1 translation table tree and IOVA region allocator for an
/// SMMU context bank.
pub struct DeviceAspace {
    aspace_start: u64,
    aspace_len: u64,
    max_cache_pages: u32,
    // TODO(johngro): This is currently doing all of its bookkeeping using direct
    // heap allocations. Consider switching it to use a RegionPool slab allocator
    // instead.
    //
    // In Zircon Rust, `RegionAllocator` requires `PinInit` initialization because its internal
    // intrusive WAVL trees hold self-referential node pointers; storing it in a
    // `Pin<kalloc::Box<RegionAllocator>>` keeps `RegionAllocator` pinned at a stable heap address
    // while allowing `DeviceAspace` itself to be constructed and moved into a `kalloc::Box`.
    avail_regions: Pin<kalloc::Box<RegionAllocator>>,
    page_cache: PageCache,
    /// An instance of a helper class which holds state for us when we are
    /// performing operations like mapping/unmapping.
    tt_helper: TranslationTableHelper,
    /// We currently only use the VMSAv8-64 translation table format with 4k
    /// granules. This means that we have 4 levels of translation table, each with
    /// 512 64-bit table entries.
    ///
    /// As long as we are operating, we always hold onto (at least) the root page,
    /// which we model as an `SmmuPage` whose 4 KiB physical page contains 512
    /// 64-bit entries in the translation table.
    root_tt_page: Option<SmmuPage>,
}

impl DeviceAspace {
    /// Creates and initializes a new [`DeviceAspace`] covering `[aspace_start, aspace_start +
    /// aspace_len)` with up to `max_cache_pages` cached translation table pages.
    ///
    /// Note, the inclusive length of the address space should always fit within a 64-bit unsigned
    /// integer since the maximum coverage of a set of VMSAv8-64 page tables is 48 bits.
    pub fn create(
        aspace_start: u64,
        aspace_len: u64,
        max_cache_pages: u32,
    ) -> Result<kalloc::Box<Self>, Status> {
        // Address spaces must have a non-zero length, and may not wrap.
        if aspace_len == 0 || aspace_start.checked_add(aspace_len).is_none() {
            return Err(Status::INVALID_ARGS);
        }

        // Temporary porting note: the C++ `DeviceAspace::Create` took the root page from the page
        // cache first and added the region afterwards, so a failure to add the region destroyed
        // the object with the root page still in flight: a leaked PMM page in release builds, and
        // a tripped destructor assert in debug builds. The steps are reordered here.
        //
        // Pre-allocate the `DeviceAspace` and `RegionAllocator` boxes and add the initial valid
        // address space region to `avail_regions` before checking out `root_tt_page` from
        // `PageCache`. Because `PageCache::drop` debug-asserts `in_flight_pages == 0`, allocating
        // `root_tt_page` last ensures no fallible step can drop `page_cache` while `root_tt_page`
        // is in flight.
        let mut uninit_aspace =
            kalloc::Box::<Self>::try_new_uninit().map_err(|_| Status::NO_MEMORY)?;

        let avail_regions =
            kalloc::Box::try_pin_init(RegionAllocator::init()).map_err(|_| Status::NO_MEMORY)?;

        // Attempt to add the region representing our valid address space to our
        // region allocator.
        avail_regions
            .add_region(RegionSpan { base: aspace_start, size: aspace_len }, AllowOverlap::No)?;

        // Allocate a page from the PMM to serve as the root translation table page.
        let mut page_cache = PageCache::new();
        let root_tt_page = page_cache.get_page()?;

        uninit_aspace.write(Self {
            aspace_start,
            aspace_len,
            max_cache_pages,
            avail_regions,
            page_cache,
            tt_helper: TranslationTableHelper::new(),
            root_tt_page: Some(root_tt_page),
        });
        // SAFETY: `uninit_aspace` was initialized by `MaybeUninit::write` above.
        let aspace = unsafe { uninit_aspace.assume_init() };

        Ok(aspace)
    }

    /// Maps `pinned_vmo` into this device address space with `perms`, optionally at the specific
    /// IOVA `location`.
    pub fn map(
        &mut self,
        pinned_vmo: &impl PinnedVmoSource,
        perms: u32,
        tlb_inval_op: &mut impl TlbInvalidator,
        location: Option<u64>,
    ) -> Result<Allocation, Status> {
        // All requests have to be:
        //
        // 1) page aligned.
        // 2) integer multiple of our page size.
        // 3) for at least one page.
        let mut offset = pinned_vmo.offset();
        let size = pinned_vmo.size();
        if (offset & PAGE_MASK) != 0 || (size & PAGE_MASK) != 0 || size == 0 {
            return Err(Status::INVALID_ARGS);
        }

        if let Some(loc) = location
            && (loc & PAGE_MASK) != 0
        {
            return Err(Status::INVALID_ARGS);
        }

        // I'm not sure what a legit maximum allocation size is, but 2^32 pages is too
        // much.
        if (size >> PAGE_SHIFT) > (u32::MAX as u64) {
            return Err(Status::INVALID_ARGS);
        }

        // First, attempt to reserve a region in our address space bookkeeping. If
        // our user has asked for a specific location, try to allocate the region
        // there. Otherwise, just ask for any region which can fit our mapping.
        let alloc = if let Some(loc) = location {
            // If the base + size overflows the address space, then this is an invalid
            // request.
            let Some(exclusive_end) = loc.checked_add(size) else {
                return Err(Status::INVALID_ARGS);
            };

            // If the proposed region lies outside the aspace valid address range in any
            // way, return `INVALID_ARGS` instead of `ALREADY_EXISTS`, to make it clear that
            // the request was malformed, not that it collided with an existing
            // allocation.
            debug_assert!(exclusive_end > 0);
            if loc < self.first_valid_address() || (exclusive_end - 1) > self.last_valid_address() {
                return Err(Status::INVALID_ARGS);
            }

            self.avail_regions.get_region_specific(RegionSpan { base: loc, size }).map_err(
                |status| {
                    // When the region allocator library cannot find a place for our
                    // allocation, it will return `ZX_ERR_NOT_FOUND`. If this happens when our
                    // user is managing their own address space, return `ZX_ERR_ALREADY_EXISTS`
                    // instead, to more clearly indicate a collision.
                    if status == Status::NOT_FOUND { Status::ALREADY_EXISTS } else { status }
                },
            )?
        } else {
            self.avail_regions.get_region(size, PAGE_SIZE)?
        };

        // Now use our `TranslationTableHelper` helper to populate our translation
        // tables. If something goes wrong during this process, hand our allocation
        // off to `unmap` to undo the changes we have made before we get out.
        let root_tt_page = self.root_tt_page.expect("root_tt_page must be valid during map");
        let alloc_base = alloc.base();
        let alloc_size = alloc.size();
        let page_total = (size >> PAGE_SHIFT) as u32;

        let populate_res: Result<(), Status> = (|| {
            for i in 0..page_total {
                // Start by looking up the physical address at our current offset.
                let phys_tgt = pinned_vmo.lookup_contiguous(offset, PAGE_SIZE)?;

                // If this is the first page, we need to initialize our helper with the
                // starting device virtual address. Otherwise, we need to advance it.
                //
                // SAFETY: `root_tt_page` and all intermediate table pages reachable from it are
                // valid, live 4 KiB translation table pages checked out from `self.page_cache` and
                // exclusively managed by `self`.
                unsafe {
                    if i == 0 {
                        self.tt_helper.initialize_for_map(
                            alloc_base,
                            root_tt_page,
                            &mut self.page_cache,
                        )?;
                    } else {
                        self.tt_helper.advance(
                            self.aspace_start,
                            self.aspace_len,
                            &self.avail_regions,
                            &mut self.page_cache,
                        )?;
                    }

                    // Success. Create our new entry and advance our offset by a page.
                    self.tt_helper.assign_page_entry(make_page_entry(phys_tgt, perms));
                }
                offset += PAGE_SIZE;
            }
            Ok(())
        })();

        if let Err(err) = populate_res {
            self.unmap(alloc, tlb_inval_op);
            return Err(err);
        }

        // We're done and success is now guaranteed. Finish the operation, flushing
        // the pages for any levels which still have dirty entries from CPU cache to
        // physical memory, and invalidating the TLBs for our region in the process.
        //
        // SAFETY: `self.tt_helper` was initialized above and all table pages referenced by it are
        // live 4 KiB translation table pages owned by `self`.
        unsafe {
            self.tt_helper.finish_operation(
                self.aspace_start,
                self.aspace_len,
                &self.avail_regions,
                &mut self.page_cache,
            );
        }
        tlb_inval_op.invalidate(alloc_base, alloc_size);
        Ok(alloc)
    }

    /// Unmaps `alloc` from this device address space, invalidating TLB entries via `tlb_inval_op`
    /// before trimming the page cache.
    pub fn unmap(&mut self, alloc: Allocation, tlb_inval_op: &mut impl TlbInvalidator) {
        // Start by caching our allocation's device virtual address and size. Then
        // return the allocation to the region allocator.
        //
        // Every time we remove a translation table entry from one of the translation
        // levels, we will mark the level as being dirty. Every time we finish with
        // a page at a given level, either because we advance to the next page or
        // because we are at the end of the operation, we can test the region map to
        // see if the virtual address range covered by the page level is available for
        // allocation. If it is, we know that the translation table page is no longer
        // in use by any other allocation, and we can put it on the list to return to
        // the PMM after TLB invalidation.
        // Note: this is only a real check for invalidators which wrap a nullable callback. See
        // `TlbInvalidator::is_valid`.
        debug_assert!(tlb_inval_op.is_valid());
        debug_assert_eq!(alloc.base() & PAGE_MASK, 0);
        debug_assert_eq!(alloc.size() & PAGE_MASK, 0);

        let base = alloc.base();
        let size = alloc.size();
        let mut addr = base;
        drop(alloc);

        // We don't permit mappings larger than 2^32 pages (and even that would be
        // pretty extreme).
        debug_assert!((size >> PAGE_SHIFT) < (u32::MAX as u64));

        let root_tt_page = self.root_tt_page.expect("root_tt_page must be valid during unmap");
        // SAFETY: `root_tt_page` and all intermediate table pages reachable from it are valid,
        // live 4 KiB translation table pages checked out from `self.page_cache` and exclusively
        // managed by `self`.
        let init_res = unsafe {
            self.tt_helper.initialize_for_unmap(addr, root_tt_page, &mut self.page_cache)
        };
        debug_assert_eq!(init_res, Ok(())); // Init for an unmap can never fail.

        debug_assert!(base.checked_add(size).is_some_and(|end| end > base));
        let last = base + size;
        while addr < last {
            // If we have pages for all four translation levels, then go ahead and
            // invalidate the entry for this page, then advance the indices.
            //
            // Otherwise, we should be finished. Why? Because when we perform any map
            // operation, there are only two options.
            //
            // 1) We succeeded and we had pages in the translation table which cover the
            //    entire region from `[map.start, map.end)`.
            // 2) We failed, and we only have pages covering `[map.start, X)` where
            //    `X < map.end`.
            //
            // In the case of #1 (a successful mapping), we should never hit a point in
            // the translation table where we are missing a page. In the case of #2 (we
            // are cleaning up after a failed mapping), as soon as we hit either a
            // missing page, or a non-valid Page entry, we have to be done.
            //
            // SAFETY: `self.tt_helper` was initialized above and all table pages referenced by it
            // are valid, live 4 KiB translation table pages owned by `self`.
            unsafe {
                if self.tt_helper.current_page_entry_valid() {
                    self.tt_helper.assign_page_entry(0);
                    let advance_res = self.tt_helper.advance(
                        self.aspace_start,
                        self.aspace_len,
                        &self.avail_regions,
                        &mut self.page_cache,
                    );
                    debug_assert_eq!(advance_res, Ok(())); // Advance during an unmap can never fail.
                    addr += PAGE_SIZE;
                } else {
                    break;
                }
            }
        }

        // Finish the operation (flushing the CPU cache where needed), then invalidate
        // the TLBs for the region we just unmapped. After this, there should be no
        // entries (either in physical memory or in TLB cache) which refer to any of
        // the pages we may have returned to the page cache. We can now trim the page
        // cache and be done.
        //
        // SAFETY: `self.tt_helper` was initialized above and all table pages referenced by it are
        // valid, live 4 KiB translation table pages owned by `self`.
        unsafe {
            self.tt_helper.finish_operation(
                self.aspace_start,
                self.aspace_len,
                &self.avail_regions,
                &mut self.page_cache,
            );
        }
        tlb_inval_op.invalidate(base, size);
        self.page_cache.trim(self.max_cache_pages);
    }

    /// Recursively walks all translation table pages and returns them to the PMM, invalidating all
    /// TLB entries via `tlb_inval_op` and trimming the [`PageCache`] to zero pages.
    ///
    /// This must be done exactly once in the life of a [`DeviceAspace`] object, just before
    /// shutdown.
    pub fn free_translation_tables(&mut self, tlb_inval_op: &mut impl TlbInvalidator) {
        // Recursively zero-out all of the PTE levels.
        let root_page =
            self.root_tt_page.expect("root_tt_page must be non-null in free_translation_tables");
        self.free_translation_tables_helper(root_page, 0);

        // Then invalidate the TLBs and finally return all of the pages from the cache
        // to the PMM.
        tlb_inval_op.invalidate_all();
        self.page_cache.trim(0);
    }

    /// The internal helper which performs the actual recursive walk. Note that
    /// this is true recursion (actual stack levels and everything), but is
    /// considered to be OK in the kernel as the max recursion depth is only 4.
    fn free_translation_tables_helper(&mut self, table_page: SmmuPage, level: u32) {
        // Never recurse past the Table/Block descriptors level of things (levels [0-2]).
        debug_assert!(level < 3);

        // Our page is a table of 512 64-bit entries. Set up an alias to easily
        // access the entries.
        let table_vaddr = SmmuEnv::paddr_to_physmap(table_page.paddr());
        // SAFETY: `table_vaddr` is the valid physmap virtual address of an L0..L2 table page.
        let table = unsafe { table_slice(table_vaddr) };

        // Check all of the entries in this page. If any of them are valid, recurse
        // into the page if we have not hit the leaf-node level of things, and then
        // (either way) return the page to the PMM.
        for (i, slot) in table.iter().enumerate() {
            let e = slot.load(Ordering::Relaxed);
            if is_valid_entry(e) {
                debug_assert!(is_table_entry(e));

                let next_pa = get_table_entry_paddr(e);
                let next_va = SmmuEnv::paddr_to_physmap(next_pa);
                let next_vm_page = SmmuEnv::paddr_to_page(next_pa)
                    .expect("valid table descriptor must point to a valid SmmuPage");

                // Descend if needed.
                if level < 2 {
                    self.free_translation_tables_helper(next_vm_page, level + 1);
                } else {
                    // This is a level 2 entry pointing to a level 3 page. We are not going
                    // to walk every entry in the level 3 page, so explicitly zero out the
                    // level 3 page before proceeding.
                    debug_assert_eq!(level, 2);
                    // SAFETY: `next_va` is the valid physmap virtual address of an L3 page table
                    // page.
                    unsafe {
                        zero_table_page(next_va);
                    }
                }

                // Zero out the entry.
                write_pte(slot, 0, level);

                // Flush the (now zero) page, and add the page to our page cache. We will
                // free the entire page cache all at once when we are done. Note: the
                // fact that we are flushing all of this from leaf -> root is just an
                // artifact of our recursive depth first traversal, it is not important
                // for correctness. Also note, this is rooted in the fact that we will never
                // return translation table pages to the PMM until after we have finished and
                // invalidated the TLBs. Even if a TLB entry, or stale PTE entry
                // references a page, that page will be in the cache, not the PMM pool.
                // SAFETY: `next_vm_page` was checked out from `self.page_cache`, is unlinked, and
                // its 4 KiB contents at `next_va` are zeroed and cleaned to the PoC.
                unsafe {
                    SmmuEnv::clean_dcache_range(next_va, PAGE_SIZE_USIZE);
                    self.page_cache.return_page(next_vm_page);
                }
            } else {
                // If an entry is not valid, we should have stored all zeros there.
                debug_assert_eq!(
                    e, 0,
                    "Bad Invalid Entry {e:#010x} (table {table_vaddr:#x}, level {level}, index {i})"
                );
            }
        }

        // If this is the top level, then the page we were given should be the root
        // page. Flush it out of the CPU cache and add it back to the page cache,
        // then zero out the bookkeeping.
        if level == 0 {
            debug_assert_eq!(self.root_tt_page, Some(table_page));
            // SAFETY: `table_page` is `self.root_tt_page` checked out from `self.page_cache`, is
            // unlinked, and its 4 KiB contents at `table_vaddr` are zeroed and cleaned to the PoC.
            unsafe {
                SmmuEnv::clean_dcache_range(table_vaddr, PAGE_SIZE_USIZE);
                self.page_cache.return_page(table_page);
            }
            self.root_tt_page = None;
        }
    }

    /// Returns the physical address of the root (`L0`) translation table page.
    ///
    /// Must not be called after [`DeviceAspace::free_translation_tables`]: as in the C++
    /// `GetRootPaddr()`, it is a (debug-asserted) error to ask for the root table of an address
    /// space which no longer has one. Release builds return `0` in that case.
    #[inline]
    pub fn root_tt_paddr(&self) -> u64 {
        debug_assert!(self.root_tt_page.is_some());
        self.root_tt_page.map_or(0, |page| page.paddr())
    }

    /// Returns a reference to this address space's [`PageCache`].
    #[inline]
    pub const fn page_cache(&self) -> &PageCache {
        &self.page_cache
    }

    /// Returns a reference to this address space's [`RegionAllocator`].
    #[cfg(any(ktest, not(is_kernel)))]
    #[inline]
    pub fn avail_regions(&self) -> &RegionAllocator {
        &self.avail_regions
    }

    /// Returns the translation granule shift in bits (`12` for 4 KiB pages).
    #[inline]
    pub const fn granule_size_bits(&self) -> u32 {
        PAGE_SHIFT
    }

    /// Returns the first valid IOVA in this device address space (`aspace_start`).
    #[inline]
    pub const fn first_valid_address(&self) -> u64 {
        self.aspace_start
    }

    /// Returns the inclusive last valid IOVA in this device address space
    /// (`aspace_start + aspace_len - 1`).
    #[inline]
    pub const fn last_valid_address(&self) -> u64 {
        debug_assert!(self.aspace_len > 0);
        self.aspace_start + self.aspace_len - 1
    }

    /// Returns the length in bytes of this device address space.
    #[inline]
    pub const fn aspace_len(&self) -> u64 {
        self.aspace_len
    }
}

impl Drop for DeviceAspace {
    fn drop(&mut self) {
        // By the time that we destruct, our translation tables must have already been freed.
        debug_assert!(self.root_tt_page.is_none());
        debug_assert_eq!(self.page_cache.cache_entries(), 0);
        debug_assert_eq!(self.page_cache.in_flight_pages(), 0);
    }
}
