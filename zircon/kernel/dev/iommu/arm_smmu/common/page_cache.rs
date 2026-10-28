// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Translation table page cache.
//!
//! [`PageCache`] maintains a LIFO free-list of zeroed, cache-cleaned 4 KiB physical pages for
//! stage-1 translation tables using the intrusive `queue_node` links embedded in `vm_page_t`
//! ([`SmmuPage`]). Because pages returned to the cache during an unmap operation may still be
//! speculatively walked by the SMMU hardware until TLB invalidation completes, [`PageCache`]
//! never modifies the 4 KiB physical page contents of cached pages and expects callers returning
//! pages via [`PageCache::return_page`] to have already zeroed and flushed them to the Point of
//! Coherency.

use zx_status::Status;

use super::constants::PAGE_SIZE_USIZE;
use super::env::{SmmuEnv, SmmuPage};
use super::vmsav8_64::zero_table_page;

/// Default PMM allocation flags (`PMM_ALLOC_FLAG_ANY = 0`).
const PMM_ALLOC_FLAG_ANY: u32 = 0;

/// Per-[`DeviceAspace`](super::device_aspace::DeviceAspace) cache of zeroed, cache-cleaned 4 KiB
/// translation table pages.
///
/// [`SmmuEnv`] abstracts `vm_page_t` as [`SmmuPage`] across kernel builds and `PmmMock` host tests,
/// so [`PageCache`] manages the doubly-linked free list via `head`/`tail` [`SmmuPage`] pointers and
/// intrusive `queue_node` accessors.
#[derive(Debug)]
pub struct PageCache {
    head: Option<SmmuPage>,
    tail: Option<SmmuPage>,
    cache_entries: u32,
    in_flight_pages: u32,
}

impl Default for PageCache {
    fn default() -> Self {
        Self::new()
    }
}

impl PageCache {
    /// Creates an empty [`PageCache`] with no cached or in-flight pages.
    pub const fn new() -> Self {
        Self { head: None, tail: None, cache_entries: 0, in_flight_pages: 0 }
    }

    /// Returns the number of pages currently held in the free-list cache.
    #[inline]
    pub const fn cache_entries(&self) -> u32 {
        self.cache_entries
    }

    /// Returns the number of pages currently checked out of this [`PageCache`] for active
    /// translation tables.
    #[inline]
    pub const fn in_flight_pages(&self) -> u32 {
        self.in_flight_pages
    }

    /// Obtains a zeroed, cache-cleaned 4 KiB page from the cache (popping in LIFO order from the
    /// front of the list) or allocates, zeroes, and cache-cleans a fresh page from the PMM if the
    /// cache is empty.
    pub fn get_page(&mut self) -> Result<SmmuPage, Status> {
        if let Some(page) = self.head {
            // SAFETY: `page` is owned by `self`'s intrusive doubly-linked list, and `&mut self`
            // guarantees exclusive access to the list pointers.
            unsafe {
                let next = page.get_next();
                self.head = next;
                if let Some(next_page) = next {
                    next_page.set_prev(None);
                } else {
                    self.tail = None;
                }
                page.set_next(None);
                page.set_prev(None);
            }
            debug_assert!(self.cache_entries > 0);
            self.cache_entries -= 1;
            self.in_flight_pages += 1;
            return Ok(page);
        }

        debug_assert_eq!(self.cache_entries, 0);
        let page = SmmuEnv::alloc_page(PMM_ALLOC_FLAG_ANY)?;
        let page_vaddr = SmmuEnv::paddr_to_physmap(page.paddr());
        debug_assert_ne!(page_vaddr, 0);

        // Pages from the PMM are not zeroed automatically for us. Explicitly zero
        // the page now. Note, we do not need to zero pages which come from the
        // cache. All pages which came from the PMM are zeroed before being
        // returned from `get_page`, and all pages returned to the cache need to be
        // zeroed by the user before going back into the cache. Host tests of the
        // page cache and device aspace code currently verify this behavior.
        //
        // SAFETY: `page_vaddr` is the kernel physmap (or host mock physmap) virtual address of a
        // freshly allocated, exclusively owned 4 KiB physical page.
        unsafe {
            zero_table_page(page_vaddr);
        }
        // SAFETY: `page_vaddr` is the physmap address of the page we just allocated.
        unsafe {
            SmmuEnv::clean_dcache_range(page_vaddr, PAGE_SIZE_USIZE);
        }

        self.in_flight_pages += 1;
        Ok(page)
    }

    /// Returns a previously checked-out translation table page to the front of the cache.
    ///
    /// # Safety
    ///
    /// - `page` must be a live, uniquely owned physical page previously checked out from `self` via
    ///   [`PageCache::get_page`].
    /// - `page` must not currently be linked into any intrusive list (its `next` and `prev` links
    ///   must both be `None`).
    /// - All 4 KiB of `page` must already be zeroed and cleaned to the Point of Coherency.
    pub unsafe fn return_page(&mut self, page: SmmuPage) {
        debug_assert!(self.in_flight_pages > 0);
        // SAFETY: Caller guarantees `page` is a live page checked out from this `PageCache` and is
        // not currently linked into any list.
        unsafe {
            debug_assert!(page.get_next().is_none() && page.get_prev().is_none());
            page.set_prev(None);
            page.set_next(self.head);
            if let Some(old_head) = self.head {
                old_head.set_prev(Some(page));
            } else {
                self.tail = Some(page);
            }
        }
        self.head = Some(page);
        self.in_flight_pages -= 1;
        self.cache_entries += 1;
    }

    /// Trims the cache down to at most `max_pages` cached pages by splitting the oldest pages off
    /// of the back of the list and freeing them back to the PMM in a single batch.
    pub fn trim(&mut self, max_pages: u32) {
        if max_pages < self.cache_entries {
            // Retain the `max_pages` most recently returned pages at the front of the list. Split
            // the excess pages off of the back of the list, and return them to the PMM all at
            // once via `SmmuEnv::free_page_chain`, so that the PMM lock is taken once rather than
            // once per page. This matters: trims happen at the end of every unmap, and
            // `free_translation_tables` trims every page table page of an address space, all while
            // the owning BTI's `pmt_lock` is held. Find the first of the excess pages to free by
            // walking `max_pages` steps forward from `self.head` (or starting at `self.head`
            // directly when `max_pages == 0`, such as in `DeviceAspace::free_translation_tables`).
            let mut first = self.head.expect("cache_entries > 0 implies head is Some");
            for _ in 0..max_pages {
                // SAFETY: `first` is in `self`'s intrusive doubly-linked list, and `&mut self`
                // guarantees exclusive access to the list pointers.
                first = unsafe { first.get_next() }
                    .expect("cache_entries pages must be linked into the list");
            }

            // SAFETY: `first` is in `self`'s intrusive doubly-linked list, and `&mut self`
            // guarantees exclusive access to the list pointers. Once `first` has been unlinked
            // from its predecessor, the chain running from `first` to the old tail is uniquely
            // owned, null-terminated at both ends, and safe to return to the PMM.
            unsafe {
                let new_tail = first.get_prev();
                first.set_prev(None);
                if let Some(new_tail_page) = new_tail {
                    new_tail_page.set_next(None);
                } else {
                    self.head = None;
                }
                self.tail = new_tail;
                SmmuEnv::free_page_chain(first);
            }
            self.cache_entries = max_pages;
        }
    }
}

impl Drop for PageCache {
    fn drop(&mut self) {
        debug_assert_eq!(self.cache_entries, 0);
        debug_assert_eq!(self.in_flight_pages, 0);
        debug_assert!(self.head.is_none());
        debug_assert!(self.tail.is_none());
    }
}
