// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! 4-level VMSAv8-64 translation table walk and update cursor.

use core::sync::atomic::{AtomicU64, Ordering};

use region_alloc::{RegionAllocator, RegionSpan, TestRegionSet};
use zx_status::Status;

use super::constants::{PAGE_SHIFT, PAGE_SIZE_USIZE};
use super::env::{SmmuEnv, SmmuPage};
use super::page_cache::PageCache;
use super::vmsav8_64::{
    ENTRIES_PER_PAGE, get_table_entry_paddr, is_table_entry, is_valid_addr, is_valid_entry,
    make_table_entry, write_pte,
};

/// Number of levels in the 48-bit input / 4 KiB granule VMSAv8-64 translation table tree.
pub const LEVELS: usize = 4;

/// Number of address bits resolved at each translation table level (`9` bits = 512 entries).
const ADDR_BITS_PER_LEVEL: u32 = 9;

/// Mask of a single level's table index (`0x1ff`).
const ADDR_BITS_MASK: u64 = (1u64 << ADDR_BITS_PER_LEVEL) - 1;

/// Reinterprets a translation table page's physmap virtual address as a reference to its array of
/// 512 atomic 64-bit descriptors.
///
/// # Safety
///
/// `table_vaddr` must be the non-zero, 8-byte-aligned physmap virtual address of an allocated 4 KiB
/// translation table page that remains valid for atomic 64-bit accesses for lifetime `'a`.
#[inline]
pub(crate) unsafe fn table_slice<'a>(table_vaddr: usize) -> &'a [AtomicU64; ENTRIES_PER_PAGE] {
    debug_assert_ne!(table_vaddr, 0);
    // SAFETY: Caller guarantees `table_vaddr` points to a valid 4 KiB translation table page.
    unsafe { &*core::ptr::with_exposed_provenance::<[AtomicU64; ENTRIES_PER_PAGE]>(table_vaddr) }
}

/// Active operation mode of a [`TranslationTableHelper`] cursor.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
enum Op {
    #[default]
    Invalid,
    Map,
    Unmap,
}

/// Per-level cursor state within a [`TranslationTableHelper`].
#[derive(Copy, Clone, Debug, Default)]
struct Level {
    table_vaddr: usize,
    ndx: u32,
    dirty: bool,
}

/// Stateful 4-level cursor used by [`DeviceAspace`](super::device_aspace::DeviceAspace) during
/// stage-1 `map` and `unmap` operations to traverse, populate, clean, and reclaim intermediate
/// translation tables.
///
/// Rather than storing a back-reference to the owning `DeviceAspace` (which would create a
/// self-referential struct), methods on [`TranslationTableHelper`] take the required `DeviceAspace`
/// fields (`aspace_start`, `aspace_len`, `avail_regions`, `root_tt_page`, and `page_cache`) as
/// explicit parameters so Rust's borrow checker can split borrows of `tt_helper` and `page_cache`
/// without interior mutability.
#[derive(Debug, Default)]
pub(crate) struct TranslationTableHelper {
    op: Op,
    levels: [Level; LEVELS],
}

impl TranslationTableHelper {
    /// Creates an uninitialized [`TranslationTableHelper`].
    pub(crate) const fn new() -> Self {
        Self { op: Op::Invalid, levels: [Level { table_vaddr: 0, ndx: 0, dirty: false }; LEVELS] }
    }

    /// Initializes the cursor to map consecutive 4 KiB pages starting at page-aligned virtual
    /// address `address`, allocating missing intermediate tables from `page_cache`.
    ///
    /// # Safety
    /// `root_tt_page` and all intermediate table pages reachable from it must be valid, live 4 KiB
    /// translation table pages checked out from `page_cache` and exclusively managed by the owning
    /// `DeviceAspace`.
    pub(crate) unsafe fn initialize_for_map(
        &mut self,
        address: u64,
        root_tt_page: SmmuPage,
        page_cache: &mut PageCache,
    ) -> Result<(), Status> {
        // SAFETY: Guaranteed by the caller.
        unsafe { self.initialize(Op::Map, address, root_tt_page, page_cache) }
    }

    /// Initializes the cursor to unmap consecutive 4 KiB pages starting at page-aligned virtual
    /// address `address`.
    ///
    /// # Safety
    /// `root_tt_page` and all intermediate table pages reachable from it must be valid, live 4 KiB
    /// translation table pages checked out from `page_cache` and exclusively managed by the owning
    /// `DeviceAspace`.
    pub(crate) unsafe fn initialize_for_unmap(
        &mut self,
        address: u64,
        root_tt_page: SmmuPage,
        page_cache: &mut PageCache,
    ) -> Result<(), Status> {
        // SAFETY: Guaranteed by the caller.
        unsafe { self.initialize(Op::Unmap, address, root_tt_page, page_cache) }
    }

    /// Advances the cursor to the next 4 KiB page address, flushing and (on unmap) reclaiming any
    /// completed intermediate tables before entering the next intermediate tables.
    ///
    /// # Safety
    /// `self` must have been initialized by [`Self::initialize_for_map`] or
    /// [`Self::initialize_for_unmap`], and all table pages referenced by `self` and reachable from
    /// the root table must remain valid, live 4 KiB translation table pages checked out from
    /// `page_cache`.
    pub(crate) unsafe fn advance(
        &mut self,
        aspace_start: u64,
        aspace_len: u64,
        avail_regions: &RegionAllocator,
        page_cache: &mut PageCache,
    ) -> Result<(), Status> {
        // Starting from level 3 and moving up to level 0, increment each index. If
        // we roll over an index for a given level, flush the page to the PoC and
        // clear our internal bookkeeping. We will obtain new pages as needed after
        // we are done advancing our indexes.
        let mut last_finished_level = LEVELS;
        for i in (0..LEVELS).rev() {
            // We can stop as soon as we don't wrap an index.
            //
            // Note: it should be safe to advance this index right now. During an
            // unmap operation, `finish_level` is going to test to see if the level we
            // are finishing is no longer in use, which requires reconstructing the
            // device virtual address of the entry we are flushing, which depends only
            // on the indices of the levels before this one, not on the index for this
            // level.
            self.levels[i].ndx += 1;
            if (self.levels[i].ndx as u64) <= ADDR_BITS_MASK {
                break;
            }

            // We have moved on to the next page. Flush any changes we have made out
            // to the PoC with the SMMU hardware. Additionally, if this is an unmap
            // operation, `finish_level` will also check the region allocator to see if
            // we can return the page to the PMM.
            //
            // Finally, `finish_level` will also move our index back to 0 and reset the
            // table pointer for us, as long as we are not at the root level.
            last_finished_level = i;
            // SAFETY: Guaranteed by the caller.
            unsafe {
                self.finish_level(i, aspace_start, aspace_len, avail_regions, page_cache);
            }

            // We should never be in a position where we need a new root page.
            if i == 0 {
                return Err(Status::INTERNAL);
            }
        }

        // Re-populate pages as needed.
        for i in last_finished_level..LEVELS {
            // SAFETY: Guaranteed by the caller.
            unsafe {
                self.find_page_for_level(i, page_cache)?;
            }
        }

        Ok(())
    }

    /// Writes `page_entry` into the current level-3 leaf descriptor slot and marks the level-3
    /// table page dirty so its CPU data cache will be cleaned when the level finishes.
    ///
    /// # Safety
    /// The current level-3 `table_vaddr` must point to a valid, live 4 KiB translation table page.
    pub(crate) unsafe fn assign_page_entry(&mut self, page_entry: u64) {
        let l = &mut self.levels[LEVELS - 1];
        assert_ne!(l.table_vaddr, 0);
        debug_assert!((l.ndx as u64) <= ADDR_BITS_MASK);

        // SAFETY: Caller guarantees `l.table_vaddr` is a non-zero physmap virtual address of a
        // valid L3 page table.
        let table = unsafe { table_slice(l.table_vaddr) };
        write_pte(&table[l.ndx as usize], page_entry, (LEVELS - 1) as u32);
        l.dirty = true;
    }

    /// Finishes the current map or unmap operation by flushing any remaining dirty table pages and
    /// (for unmap) reclaiming any intermediate table pages whose covered region is now completely
    /// unallocated.
    ///
    /// # Safety
    /// `self` must have been initialized by [`Self::initialize_for_map`] or
    /// [`Self::initialize_for_unmap`], and all table pages referenced by `self` must be valid, live
    /// 4 KiB translation table pages checked out from `page_cache`.
    pub(crate) unsafe fn finish_operation(
        &mut self,
        aspace_start: u64,
        aspace_len: u64,
        avail_regions: &RegionAllocator,
        page_cache: &mut PageCache,
    ) {
        debug_assert_ne!(self.op, Op::Invalid, "Bad State {:?}", self.op);

        // Finish each level, flushing any dirty pages and reclaiming any now-unused
        // pages if this is an `Op::Unmap`.
        for i in (0..LEVELS).rev() {
            // SAFETY: Guaranteed by the caller.
            unsafe {
                self.finish_level(i, aspace_start, aspace_len, avail_regions, page_cache);
            }
        }

        self.op = Op::Invalid;
        self.levels = [Level::default(); LEVELS];
    }

    /// Returns `true` if the current level-3 leaf descriptor exists and has its valid bit set.
    ///
    /// # Safety
    /// If non-zero, the current level-3 `table_vaddr` must point to a valid, live 4 KiB
    /// translation table page.
    pub(crate) unsafe fn current_page_entry_valid(&self) -> bool {
        let last_level = &self.levels[LEVELS - 1];
        if last_level.table_vaddr == 0 {
            return false;
        }

        debug_assert!((last_level.ndx as u64) <= ADDR_BITS_MASK);
        // SAFETY: Caller guarantees `last_level.table_vaddr` is a non-zero physmap virtual address
        // of a valid L3 page table.
        let table = unsafe { table_slice(last_level.table_vaddr) };
        let entry = table[last_level.ndx as usize].load(Ordering::Relaxed);
        let valid = is_valid_entry(entry);
        debug_assert!(valid || entry == 0);
        valid
    }

    unsafe fn initialize(
        &mut self,
        op: Op,
        address: u64,
        root_tt_page: SmmuPage,
        page_cache: &mut PageCache,
    ) -> Result<(), Status> {
        // We expect only page aligned addresses.
        debug_assert!(is_valid_addr(address));

        // The requested operation must either be a map or unmap operation.
        debug_assert_ne!(op, Op::Invalid);

        // Reset our level state, then compute the initial indices for each level of
        // the translation table and stash our root page.
        self.op = op;
        self.levels = [Level::default(); LEVELS];
        self.levels[0].table_vaddr = SmmuEnv::paddr_to_physmap(root_tt_page.paddr());

        for (i, level) in self.levels.iter_mut().enumerate() {
            let shift = PAGE_SHIFT + (ADDR_BITS_PER_LEVEL * ((LEVELS - i - 1) as u32));
            level.ndx = ((address >> shift) & ADDR_BITS_MASK) as u32;
        }

        // Attempt to find translation table pages for each of our levels. We'll
        // first look in the existing translation table structure for pages already
        // allocated. Failing that, if this is a Map operation, we'll try to get some
        // pages from the cache.
        for i in 1..LEVELS {
            // SAFETY: Guaranteed by the caller.
            unsafe {
                self.find_page_for_level(i, page_cache)?;
            }
        }

        Ok(())
    }

    unsafe fn finish_level(
        &mut self,
        level_ndx: usize,
        aspace_start: u64,
        aspace_len: u64,
        avail_regions: &RegionAllocator,
        page_cache: &mut PageCache,
    ) {
        debug_assert!(level_ndx < LEVELS);

        // Nothing to do if we have no page for this level (something which should
        // only be possible during Unmap operations).
        if self.levels[level_ndx].table_vaddr == 0 {
            debug_assert_eq!(self.op, Op::Unmap);
            return;
        }

        // If the page is dirty, flush the cache.
        if self.levels[level_ndx].dirty {
            // SAFETY: `table_vaddr` is the non-zero physmap address of this level's table page.
            unsafe {
                SmmuEnv::clean_dcache_range(self.levels[level_ndx].table_vaddr, PAGE_SIZE_USIZE);
            }
            self.levels[level_ndx].dirty = false;
        }

        // If this is not the root level, return the page to the page pool if this is
        // an unmap operation and we don't need it anymore, and reset the table and
        // index bookkeeping no matter what.
        if level_ndx != 0 {
            if self.op == Op::Unmap {
                // Compute the base device virtual address for the level we are finishing.
                // This is made from the indices of all the levels before us.
                let mut r_base: u64 = 0;
                for (i, level) in self.levels.iter().enumerate().take(level_ndx) {
                    let shift = PAGE_SHIFT + (((LEVELS - i - 1) as u32) * ADDR_BITS_PER_LEVEL);
                    r_base |= (level.ndx as u64) << shift;
                }

                // Figure out the size of this level. The granule is 4k, and every level
                // adds `ADDR_BITS_PER_LEVEL`.
                let mut r_size: u64 =
                    1u64 << (PAGE_SHIFT + (((LEVELS - level_ndx) as u32) * ADDR_BITS_PER_LEVEL));

                // Limit the coverage of this PTE level to just the address space that our
                // `DeviceAspace` was created to handle. Otherwise, really large regions
                // covered by things like level 1 might never get reclaimed as they are not
                // "available for allocation" from our region allocator.
                if aspace_start > r_base {
                    let delta = aspace_start - r_base;

                    // We should never be considering whether or not we need to reclaim the
                    // page for a level which does not even intersect our address space.
                    // There is no reason we should have ever allocated a page for that slice
                    // of the address space in the first place.
                    debug_assert!(r_size > delta);
                    r_base = aspace_start;
                    r_size -= delta;
                }

                let aspace_end = aspace_start + aspace_len;
                let region_end = r_base + r_size;
                if region_end > aspace_end {
                    let delta = region_end - aspace_end;
                    debug_assert!(r_size > delta);
                    r_size -= delta;
                }

                // Finally, test to see if we should return the page for this level to the
                // PMM. If the virtual address range controlled by this level of the
                // translation tables is entirely available for allocation in the region
                // allocator, then the page is no longer needed. We can invalidate the
                // Table Entry which points to us, and then add the page to the
                // `page_cache`. We'll trim the cache as a batch operation at the end of
                // the Unmap operation, after the TLBs have been invalidated.
                let test_result = avail_regions.test_region_contained_by(
                    RegionSpan { base: r_base, size: r_size },
                    TestRegionSet::Available,
                );
                debug_assert!(test_result.is_ok());

                if test_result == Ok(true) {
                    let prev_vaddr = self.levels[level_ndx - 1].table_vaddr;
                    let prev_ndx = self.levels[level_ndx - 1].ndx as usize;
                    assert_ne!(prev_vaddr, 0);
                    debug_assert!((prev_ndx as u64) <= ADDR_BITS_MASK);

                    // SAFETY: `prev_vaddr` is a non-zero physmap virtual address of a valid parent
                    // translation table page.
                    let prev_table = unsafe { table_slice(prev_vaddr) };
                    write_pte(&prev_table[prev_ndx], 0, (level_ndx - 1) as u32);
                    self.levels[level_ndx - 1].dirty = true;

                    let return_me_phys =
                        SmmuEnv::physmap_to_paddr(self.levels[level_ndx].table_vaddr);
                    let return_me = SmmuEnv::paddr_to_page(return_me_phys);

                    // Return the page to the page cache. It will linger there full of
                    // zeros until the TLBs have been invalidated and it can finally be
                    // safely returned to the PMM.
                    debug_assert!(
                        return_me.is_some(),
                        "Failed to recover vm_page {:#x}, phys {:#018x}",
                        self.levels[level_ndx].table_vaddr,
                        return_me_phys
                    );
                    // SAFETY: All entries in this level's table page are zero (`r_base..r_base +
                    // r_size` is entirely unallocated), its cache lines were cleaned above if
                    // dirty, and the page was originally checked out from `page_cache`.
                    unsafe {
                        page_cache.return_page(return_me.unwrap());
                    }
                }
            }

            // We are finished with this level. Reset the table pointer and the index.
            self.levels[level_ndx].table_vaddr = 0;
            self.levels[level_ndx].ndx = 0;
            debug_assert!(!self.levels[level_ndx].dirty);
        }
    }

    unsafe fn find_page_for_level(
        &mut self,
        level: usize,
        page_cache: &mut PageCache,
    ) -> Result<(), Status> {
        debug_assert!(level > 0 && level < LEVELS);
        if self.levels[level].table_vaddr != 0 {
            return Ok(());
        }

        // If we have not found a page for this level, start by checking the level
        // before this one to see if there is a valid Table entry there. If there
        // is, just translate the physical address to the physmap address and we
        // should be good to go.
        //
        // Note: If we are performing an Unmap operation as a result of a cleanup
        // after a failed Map operation, it is possible that we don't _have_ a
        // previous table level. We need to handle that edge case here.
        let prev = level - 1;
        if self.op == Op::Unmap && self.levels[prev].table_vaddr == 0 {
            return Ok(());
        }

        let prev_vaddr = self.levels[prev].table_vaddr;
        let prev_ndx = self.levels[prev].ndx as usize;
        assert_ne!(prev_vaddr, 0);
        debug_assert!((prev_ndx as u64) <= ADDR_BITS_MASK);

        // SAFETY: `prev_vaddr` is a non-zero physmap virtual address of a valid translation table
        // page at level `prev`.
        let prev_table = unsafe { table_slice(prev_vaddr) };
        let existing_entry = prev_table[prev_ndx].load(Ordering::Relaxed);
        if is_valid_entry(existing_entry) {
            debug_assert!(is_table_entry(existing_entry));
            let phys = get_table_entry_paddr(existing_entry);
            self.levels[level].table_vaddr = SmmuEnv::paddr_to_physmap(phys);
            return Ok(());
        }

        // We have not found a page for this level because we have not allocated one
        // yet. If this is an Unmap operation, that's OK. We are in the process of
        // removing entries and empty levels anyway.
        if self.op == Op::Unmap {
            return Ok(());
        }

        // Try to grab a page, and if we succeed, create the table entry at
        // the level before this one pointing at our new page.
        let page = page_cache.get_page()?;

        // Make the new entry. Be sure to flag the previous level's page as dirty
        // so we can flush it to physical memory when we are done.
        //
        // Note: It is assumed that all pages which come out of the page cache are
        // already zeroed (meaning that all of the entries are already invalid), and
        // that it is already flushed to the PoC with the SMMU. If any of this is an
        // invalid assumption, this is the place to fix it (by explicitly zeroing the
        // new page, and flagging it as dirty so it will be flushed at the proper
        // time).
        let page_phys = page.paddr();
        write_pte(&prev_table[prev_ndx], make_table_entry(page_phys), prev as u32);
        self.levels[level].table_vaddr = SmmuEnv::paddr_to_physmap(page_phys);
        self.levels[prev].dirty = true;
        Ok(())
    }
}
