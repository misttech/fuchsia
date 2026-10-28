// Copyright 2016 The Fuchsia Authors
// Copyright (c) 2015 Google, Inc. All rights reserved
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Malloc implementation tuned for space.
//!
//! Allocation strategy takes place with a global mutex.  Freelist entries are
//! kept in linked lists with 8 different sizes per binary order of magnitude
//! and the header size is two words with eager coalescing on free.
//!
//! ## Concepts ##
//!
//! OS allocation:
//!   A contiguous range of pages allocated from the OS using `heap_page_alloc()`,
//!   typically via `heap_grow()`. Initial layout:
//!
//!   Low addr =>
//!     `Header` `left_sentinel` -- Marked as allocated, `left` pointer NULL.
//!     `Free` `memory_area` -- Marked as free, with appropriate size,
//!                             and pointed to by a free bucket.
//!     [bulk of usable memory]
//!     `Header` `right_sentinel` -- Marked as allocated, size zero
//!   <= High addr
//!
//!   For a normal allocation, the free memory area is added to the
//!   appropriate free bucket and picked up later in the `cmpct_alloc()`
//!   logic. For a large allocation, the area skips the primary free buckets
//!   and is returned directly via a `*mut *mut Free` bucket param.
//!
//!   `cmpctmalloc` does not keep a list of OS allocations; each is meant to free
//!   itself to the OS when all of its memory areas become free.
//!
//! Memory area:
//!   A sub-range of an OS allocation. Used to satisfy
//!   `cmpct_alloc()`/`cmpct_memalign()` calls. Can be free and live in a free
//!   bucket, or can be allocated and managed by the user.
//!
//!   Memory areas, both free and allocated, always begin with a `Header`,
//!   followed by the area's usable memory. `Header.size` includes the size of
//!   the header. `untag(Header.left)` points to the preceding area's `Header`.
//!
//!   The low bits of `Header.left` hold additional flags about the area:
//!   - `FREE_BIT`: The area is free, and lives in a free bucket.
//!
//!   These bits shouldn't be checked directly; use the `is_tagged_as_*()`
//!   functions.
//!
//!   If the area is free (`is_tagged_as_free(*const Header)`), the area's header
//!   includes the doubly-linked free list pointers defined by `Free` (which is a
//!   `Header` overlay). Those pointers are used to chain the free area off of
//!   the appropriately-sized free bucket.
//!
//! Normal (small/non-large) allocation:
//!   An allocation of less than or equal to (`HEAP_LARGE_ALLOC_BYTES - size_of::<Header>()`),
//!   which can fit in a free bucket.
//!
//! Large allocation:
//!   An allocation of more than (`HEAP_LARGE_ALLOC_BYTES - size_of::<Header>()`). This
//!   is no longer allowed.
//!
//! Free buckets:
//!   Freelist entries are kept in linked lists with 8 different sizes per binary
//!   order of magnitude: `heap.free_lists[NUMBER_OF_BUCKETS]`
//!
//!   Allocations are always rounded up to the nearest bucket size. This would
//!   appear to waste memory, but in fact it avoids some fragmentation.
//!
//!   Consider two buckets with size 512 and 576 (512 + 64). Perhaps the program
//!   often allocates 528 byte objects for some reason. When we need to allocate
//!   528 bytes, we round that up to 576 bytes. When it is freed, it goes in the
//!   576 byte bucket, where it is available for the next of the common 528 byte
//!   allocations.
//!
//!   If we did not round up allocations, then (assuming no coalescing is
//!   possible) we would have to place the freed 528 bytes in the 512 byte
//!   bucket, since only memory areas greater than or equal to 576 bytes can go
//!   in the 576 byte bucket. The next time we need to allocate a 528 byte object
//!   we do not look in the 512 byte bucket, because we want to be sure the first
//!   memory area we look at is big enough, to avoid searching a long chain of
//!   just-too-small memory areas on the free list. We would not find the 528
//!   byte space and would have to carve out a new 528 byte area from a large
//!   free memory area, making fragmentation worse.
//!
//! `cmpct_free()` behavior:
//!   Freed memory areas are eagerly coalesced with free left/right neighbors. If
//!   the new free area covers an entire OS allocation (i.e., its left and right
//!   neighbors are both sentinels), the OS allocation is returned to the OS.
//!
//!   Exception: to avoid OS free/alloc churn when right on the edge, the heap
//!   will try to hold onto one entirely-free, non-large OS allocation instead of
//!   returning it to the OS. See `cached_os_alloc`.

use core::cell::UnsafeCell;
use core::cmp::{max, min};
use core::ffi::c_void;
use core::mem::{align_of, size_of};
use core::ptr;
use core::sync::atomic::{AtomicUsize, Ordering};

use bitflags::bitflags;
use ksync::lock;

use super::cmpctmalloc_ffi;

pub(crate) use platform::*;

#[cfg(not(feature = "userspace"))]
mod platform {
    pub(crate) use debug::{dprintf, ltrace_entry, ltracef};
    #[cfg(feature = "heap_enable_tests")]
    pub(crate) use kprint::kprintln;

    pub(crate) const LOCAL_TRACE: u32 = 0;

    macro_rules! local_trace_duration {
        ($label:tt, $name:ident $(, $key:tt => $val:expr)*) => {
            let $name = {
                if false {
                    let _ = ($label $(, $key, $val)*);
                }
            };
        };
    }
    pub(crate) use local_trace_duration;

    macro_rules! local_trace_duration_end {
        ($name:ident) => {
            let _ = $name;
        };
    }
    pub(crate) use local_trace_duration_end;

    macro_rules! ktrace_duration_begin {
        ($category:expr, $label:expr $(, $key:expr => $val:expr)*) => {
            $crate::ktrace_rs::duration_begin!($category, $label $(, $key => $val)*);
        };
    }
    pub(crate) use ktrace_duration_begin;

    macro_rules! ktrace_duration_end {
        ($category:expr, $label:expr $(, $key:expr => $val:expr)*) => {
            $crate::ktrace_rs::duration_end!($category, $label $(, $key => $val)*);
        };
    }
    pub(crate) use ktrace_duration_end;

    ksync::declare_singleton_critical_mutex!(
        /// This is a popular lock.  Critical sections protected by this lock are
        /// *usually* short-lived, so we'd like to minimize lock thrash by disabling
        /// preemption while holding the lock.  However, in degenerate cases (e.g. with
        /// excessive PMM fragmentation), the lock can be held for long periods of time.
        /// In those cases, we don't want to overly delay the scheduling of a higher
        /// priority task.  To strike a balance between these competing concerns, we use
        /// a critical mutex which employs "time-limited preemption deferral".
        ///
        /// For more information, see https://fxbug.dev/399694236.
        pub(crate) TheHeapLock
    );

    crate::counters::define_kcounter!(MALLOC_SIZE_LE_64, "malloc.size_le_64", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_96, "malloc.size_le_96", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_128, "malloc.size_le_128", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_256, "malloc.size_le_256", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_384, "malloc.size_le_384", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_512, "malloc.size_le_512", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_1024, "malloc.size_le_1024", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_LE_2048, "malloc.size_le_2048", Sum);
    crate::counters::define_kcounter!(MALLOC_SIZE_OTHER, "malloc.size_other", Sum);
    // The number of failed attempts at growing the heap.
    crate::counters::define_kcounter!(MALLOC_HEAP_GROW_FAIL, "malloc.heap_grow_fail", Sum);

    #[inline]
    pub(crate) fn record_malloc_size(size: usize) {
        let counter = if size <= 64 {
            &MALLOC_SIZE_LE_64
        } else if size <= 96 {
            &MALLOC_SIZE_LE_96
        } else if size <= 128 {
            &MALLOC_SIZE_LE_128
        } else if size <= 256 {
            &MALLOC_SIZE_LE_256
        } else if size <= 384 {
            &MALLOC_SIZE_LE_384
        } else if size <= 512 {
            &MALLOC_SIZE_LE_512
        } else if size <= 1024 {
            &MALLOC_SIZE_LE_1024
        } else if size <= 2048 {
            &MALLOC_SIZE_LE_2048
        } else {
            &MALLOC_SIZE_OTHER
        };
        counter.add(1);
    }

    #[inline]
    pub(crate) fn record_heap_grow_fail() {
        MALLOC_HEAP_GROW_FAIL.add(1);
    }
}

#[cfg(feature = "userspace")]
mod platform {
    // When built in a host (aka non-kernel) environment it is assumed to be for testing and so we
    // want to make sure that debug assertions are correctly enabled. This is to prevent any
    // regressions due to libraries and build system changes.
    zr::static_assert!(cfg!(debug_assertions), "Expect debug assertions in host builds");

    macro_rules! dprintf {
        ($level:ident, $($arg:tt)*) => {
            let _ = format_args!($($arg)*);
        };
    }
    pub(crate) use dprintf;

    macro_rules! ltracef {
        ($($arg:tt)*) => {
            let _ = format_args!($($arg)*);
        };
    }
    pub(crate) use ltracef;

    macro_rules! ltrace_entry {
        () => {};
    }
    pub(crate) use ltrace_entry;

    #[cfg(feature = "heap_enable_tests")]
    macro_rules! kprintln {
        ($($arg:tt)*) => {
            let _ = format_args!($($arg)*);
        };
    }
    #[cfg(feature = "heap_enable_tests")]
    pub(crate) use kprintln;

    macro_rules! local_trace_duration {
        ($label:tt, $name:ident $(, $key:tt => $val:expr)*) => {
            let $name = {
                if false {
                    let _ = ($label $(, $key, $val)*);
                }
            };
        };
    }
    pub(crate) use local_trace_duration;

    macro_rules! local_trace_duration_end {
        ($name:ident) => {
            let _ = $name;
        };
    }
    pub(crate) use local_trace_duration_end;

    macro_rules! ktrace_duration_begin {
        ($category:expr, $label:expr $(, $key:expr => $val:expr)*) => {
            let _ = ($category, $label $(, $key, $val)*);
        };
    }
    pub(crate) use ktrace_duration_begin;

    macro_rules! ktrace_duration_end {
        ($category:expr, $label:expr $(, $key:expr => $val:expr)*) => {
            let _ = ($category, $label $(, $key, $val)*);
        };
    }
    pub(crate) use ktrace_duration_end;

    ksync::declare_singleton_mutex!(pub(crate) TheHeapLock);

    #[inline]
    pub(crate) fn record_malloc_size(_size: usize) {}

    #[inline]
    pub(crate) fn record_heap_grow_fail() {}
}

bitflags! {
    /// Options controlling `cmpct_dump` output.
    #[repr(transparent)]
    #[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
    pub struct CmpctDumpOptions: u8 {
        /// Standard dump output.
        const NONE = 0x0;
        /// Dump called at panic time; avoids blocking locks.
        const PANIC_TIME = 0x1;
        /// Include verbose free-block details.
        const VERBOSE = 0x2;
    }
}

zr::static_assert!(size_of::<CmpctDumpOptions>() == 1);
zr::static_assert!(align_of::<CmpctDumpOptions>() == 1);

use debug_fill::*;

#[cfg(all(not(feature = "userspace"), any(feature = "cmpct_debug", lk_debuglevel_gt2)))]
mod debug_fill {
    use core::ffi::c_void;
    use core::mem::size_of;
    use core::ptr;

    use kprint::kprintln;

    use super::{Free, Header};

    /// TODO(https://fxbug.dev/404291515): Enable on riscv to track down heap corruption.
    const ALLOC_FILL: u8 = 0x99;
    const FREE_FILL: u8 = 0x77;
    const PADDING_FILL: u8 = 0x55;

    /// # Safety
    ///
    /// The caller must ensure that `ptr` is readable for `size` bytes.
    #[sanitize(address = "off")]
    unsafe fn hexdump8(ptr: *const c_void, size: usize) {
        // SAFETY: Caller guarantees `ptr` is readable for `size` bytes; `hexdump8_very_ex_raw`
        // avoids creating a safe Rust slice over potentially KASAN-poisoned memory.
        let _ = unsafe {
            pretty::hexdump8_very_ex_raw(
                &mut debug::dprintf::KernelConsoleWriter,
                ptr.cast::<u8>(),
                size,
                ptr.addr() as u64,
            )
        };
    }

    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is held and `ptr` is readable for `size` bytes.
    #[sanitize(address = "off")]
    unsafe fn check_free_fill(ptr: *const c_void, size: usize) {
        // The first 16 bytes of the region won't have free fill due to overlap
        // with the allocator bookkeeping.
        // SAFETY: Caller guarantees `ptr` is readable for `size` bytes.
        unsafe {
            let start = size_of::<Free>() - size_of::<Header>();
            for i in start..size {
                let byte = *ptr.cast::<u8>().add(i);
                if byte != FREE_FILL {
                    crate::platform_rs::power::platform_panic_start();
                    kprintln!("Heap free fill check fail.  Allocated region:");
                    hexdump8(ptr, size);
                    panic!("allocating {} bytes, fill was {:02x}, offset {}", size, byte, i);
                }
            }
        }
    }

    /// Fills the newly created free area with the debug free pattern.
    ///
    /// # Safety
    ///
    /// The caller must ensure `free_area` is valid for writes of `size` bytes.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn fill_free_area(free_area: *mut Free, size: usize) {
        // SAFETY: Caller guarantees `free_area` is valid for `size` bytes.
        unsafe {
            ptr::write_bytes(free_area.add(1).cast::<u8>(), FREE_FILL, size - size_of::<Free>());
        }
    }

    /// Fills the newly allocated area with the debug fill pattern.
    ///
    /// # Safety
    ///
    /// The caller must ensure `result` is valid for writes of
    /// `rounded_up - size_of::<Header>()` bytes.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn fill_alloc_area(result: *mut c_void, size: usize, rounded_up: usize) {
        // SAFETY: Caller guarantees `result` is valid for `rounded_up - size_of::<Header>()` bytes.
        unsafe {
            check_free_fill(result, size);
            ptr::write_bytes(result.cast::<u8>(), ALLOC_FILL, size);
            ptr::write_bytes(
                result.cast::<u8>().add(size),
                PADDING_FILL,
                rounded_up - size - size_of::<Header>(),
            );
        }
    }

    #[inline]
    pub(super) fn on_init() {
        kprintln!("CMPCT: Enabling debug");
    }
}

#[cfg(not(all(not(feature = "userspace"), any(feature = "cmpct_debug", lk_debuglevel_gt2))))]
mod debug_fill {
    use core::ffi::c_void;

    use super::Free;

    /// Fills the newly created free area with the debug free pattern.
    ///
    /// # Safety
    ///
    /// The caller must ensure `free_area` is valid for writes of `size` bytes.
    #[inline]
    pub(super) unsafe fn fill_free_area(_free_area: *mut Free, _size: usize) {}

    /// Fills the newly allocated area with the debug fill pattern.
    ///
    /// # Safety
    ///
    /// The caller must ensure `result` is valid for writes of
    /// `rounded_up - size_of::<Header>()` bytes.
    #[inline]
    pub(super) unsafe fn fill_alloc_area(_result: *mut c_void, _size: usize, _rounded_up: usize) {}

    #[inline]
    pub(super) fn on_init() {}
}

/// `HEAP_GROW_SIZE` is minimum size by which the heap is grown.
///
/// A larger value can provide some performance improvement at the cost of wasted
/// memory.
///
/// See also [`HEAP_LARGE_ALLOC_BYTES`].
const HEAP_GROW_SIZE: usize = 256 * 1024;

zr::static_assert!(page::is_page_rounded(HEAP_GROW_SIZE));

/// `HEAP_ALLOC_VIRTUAL_BITS` defines the largest allocation bucket.
///
/// The requirements on virtual bits is that the largest allocation (including
/// header), must roundup to not more `2**HEAP_ALLOC_VIRTUAL_BITS` than this
/// alignment, and similarly the heap cannot grow by amounts that would not round
/// down to `2**HEAP_ALLOC_VIRTUAL_BITS` or less. As such the heap can grow by more
/// than this many bits at once, but not so many as it must fall into the next
/// bucket.
const HEAP_ALLOC_VIRTUAL_BITS: usize = 21;

/// `HEAP_LARGE_ALLOC_BYTES` limits size of any single allocation.
///
/// A larger value will, on average, "waste" more memory. Why is that? When
/// freeing memory the heap may hold on to a block before returning it to the
/// underlying allocator (see `THE_HEAP.cached_os_alloc`). The size of the cached
/// block is limited by `HEAP_LARGE_ALLOC_BYTES` so reducing this value limits the
/// size of the cached block.
///
/// Note that `HEAP_LARGE_ALLOC_BYTES` is the largest internal allocation that the
/// heap can do, and includes any headers. The largest allocation `cmpct_alloc`
/// could theoretically (it may be artificially limited) provide is therefore
/// slightly less than this.
///
/// See also [`HEAP_GROW_SIZE`].
const HEAP_LARGE_ALLOC_BYTES: usize = (1usize << HEAP_ALLOC_VIRTUAL_BITS) - HEAP_GROW_OVERHEAD;

/// Buckets for allocations.  The smallest 15 buckets are 8, 16, 24, etc. up to
/// 120 bytes.  After that we round up to the nearest size that can be written
/// `/^0*1...0*$/`, giving 8 buckets per order of binary magnitude.  The freelist
/// entries in a given bucket have at least the given size, plus the header
/// size.  On 64 bit, the 8 byte bucket is useless, since the freelist header
/// is 16 bytes larger than the header, but we have it for simplicity.
const NUMBER_OF_BUCKETS: usize = 1 + 15 + (HEAP_ALLOC_VIRTUAL_BITS - 7) * 8;

/// If a header's `left` field has this bit set, it is free and lives in
/// a free bucket.
const FREE_BIT: usize = 1 << 0;

const HEADER_LEFT_BIT_MASK: usize = FREE_BIT;

/// All individual memory areas on the heap start with this.
#[repr(C)]
struct Header {
    /// Pointer to the previous area in memory order. The lower bit is used
    /// to store extra state: see [`FREE_BIT`]. The left sentinel will have
    /// NULL in the address portion of this field. Left and right sentinels
    /// will always be marked as "allocated" to avoid coalescing.
    // Safety invariant: `untag(left)` is either null (for the left sentinel of an OS allocation)
    // or points to the preceding `Header` within the same OS allocation; bit 0 (`FREE_BIT`) is
    // set iff this memory area is currently free and linked in `THE_HEAP.free_lists`.
    left: *mut Header,
    /// The size of the memory area in bytes, including this header.
    /// The right sentinel will have 0 in this field.
    /// A 32-bit value is used for the size to leave room for the cookie, and the
    /// maximum size of our allowed allocations fits easily within this.
    // Safety invariant: Either `0` (for the right sentinel of an OS allocation) or the byte size
    // (`>= size_of::<Header>()`, 8-byte aligned) extending to the adjacent right `Header` within
    // the same OS allocation (or the total page-aligned OS allocation size when stored in
    // `THE_HEAP.cached_os_alloc`).
    size: u32,
    /// User defined cookie that can be associated with an allocation.
    cookie: u32,
}

zr::static_assert!(size_of::<Header>() == 16);
zr::static_assert!(align_of::<Header>() == 8);

/// When the heap is grown the requested internal usable size will be increased
/// by this amount before allocating from the OS. This can be factored into
/// any `heap_grow` requested to precisely control the OS allocation amount.
const HEAP_GROW_OVERHEAD: usize = size_of::<Header>() * 2;

/// Precalculated version of [`HEAP_GROW_SIZE`] that takes into account the grow
/// overhead.
const HEAP_USABLE_GROW_SIZE: usize = HEAP_GROW_SIZE - HEAP_GROW_OVERHEAD;

// When we grow the heap we have to have somewhere in the freelist to put the
// resulting freelist entry, so the freelist has to have a certain number of
// buckets.
zr::static_assert!(HEAP_GROW_SIZE <= HEAP_LARGE_ALLOC_BYTES);
// We use a 32-byte value to store sizes, so ensure the largest allocation fits
// inside such a value.
zr::static_assert!(HEAP_LARGE_ALLOC_BYTES < u32::MAX as usize);

#[repr(C)]
struct Free {
    // Safety invariant: `header.size as usize >= size_of::<Free>()` and `header.left` has
    // `FREE_BIT` set while this node is in a free list.
    header: Header,
    // Safety invariant: Either null or points to the next valid `Free` node in the same bucket's
    // doubly-linked list under `TheHeapLock`.
    next: *mut Free,
    // Safety invariant: Either null (when at the head of the bucket) or points to the previous
    // valid `Free` node in the same bucket's doubly-linked list under `TheHeapLock`.
    prev: *mut Free,
}

zr::static_assert!(size_of::<Free>() == 32);
zr::static_assert!(align_of::<Free>() == 8);

/// Bitmask that tracks whether a given `free_lists` entry has any elements.
/// See [`set_free_list_bit`], [`clear_free_list_bit`].
const BUCKET_WORDS: usize = (NUMBER_OF_BUCKETS + 31) >> 5;

#[cfg_attr(not(feature = "userspace"), cfg(not(sanitize = "address")))]
mod asan_stub {
    use core::ffi::c_void;

    use super::{Header, Heap};

    pub(super) struct Quarantine;

    impl Quarantine {
        pub(super) const fn new() -> Self {
            Self
        }

        /// Pushes `header` into the ASAN quarantine (no-op when ASAN is disabled).
        #[inline]
        pub(super) fn push(&mut self, header: *mut Header) -> *mut Header {
            header
        }
    }

    #[inline]
    pub(super) const fn adjust_alloc_size(size: usize, _alloc_size: usize) -> usize {
        size
    }

    /// Poisons the allocation's header and trailing right redzone, while unpoisoning its usable
    /// payload (no-op when ASAN is disabled).
    ///
    /// # Safety
    ///
    /// The caller must ensure that the header and redzone regions are backed by KASAN shadow
    /// memory.
    #[inline]
    pub(super) unsafe fn poison_allocation(
        _head_addr: usize,
        _payload_addr: usize,
        _alloc_size: usize,
    ) {
    }

    /// Poisons the `memalign` allocation and its padding (no-op when ASAN is disabled).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `[unaligned_addr, unaligned_addr + padded_size)` is backed by
    /// KASAN shadow memory.
    #[inline]
    pub(super) unsafe fn poison_memalign_padding(_unaligned_addr: usize, _padded_size: usize) {}

    /// Unpoisons the `memalign` allocation payload (no-op when ASAN is disabled).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `[payload_addr, payload_addr + size)` is backed by KASAN
    /// shadow memory.
    #[inline]
    pub(super) unsafe fn unpoison_memalign_payload(_payload_addr: usize, _size: usize) {}

    /// Quarantines the freed allocation if quarantine is enabled (returns `header` directly when
    /// ASAN is disabled).
    ///
    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is held and `header` is a valid allocated
    /// `Header`.
    #[inline]
    pub(super) unsafe fn quarantine_or_free(
        theheap: *mut Heap,
        _payload: *mut c_void,
        header: *mut Header,
        _header_size: usize,
    ) -> *mut Header {
        // SAFETY: Caller guarantees `TheHeapLock` is held and `theheap` is valid.
        unsafe { (*theheap).asan_quarantine.push(header) }
    }
}

#[cfg(not(feature = "userspace"))]
#[cfg(sanitize = "address")]
mod kernel_asan {
    use core::ffi::c_void;
    use core::mem::size_of;

    use super::{HEAP_LARGE_ALLOC_BYTES, HEAP_MAX_ALLOC_SIZE, Header, Heap, SizeToIndexRet};
    use crate::instrumentation::asan::{
        ASAN_HEAP_FREE_MAGIC, ASAN_HEAP_LEFT_REDZONE_MAGIC, Quarantine as AsanQuarantine,
        asan_heap_redzone_size, asan_poison_shadow, asan_unpoison_shadow,
    };

    // Safety invariant: Every non-null element stored in `THE_HEAP`'s quarantine is a valid,
    // quarantined `Header` pointer within an OS allocation that has not yet been returned to a
    // free list or the OS.
    pub(super) type Quarantine = AsanQuarantine<Header>;

    /// Adds space at the end of the allocation for an ASAN right redzone.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) fn adjust_alloc_size(size: usize, alloc_size: usize) -> usize {
        debug_assert!(alloc_size <= HEAP_MAX_ALLOC_SIZE);
        // Add space at the end of the allocation for a redzone.
        // A redzone is used to detect buffer overflows by oversizing the buffer and poisoning
        // the excess memory. The redzone is after the buffer - before the buffer is a
        // Header, which is also poisoned.
        // SAFETY: `alloc_size <= HEAP_MAX_ALLOC_SIZE`, so `asan_heap_redzone_size` cannot
        // overflow.
        let (redzone, max_redzone) = unsafe {
            (asan_heap_redzone_size(alloc_size), asan_heap_redzone_size(HEAP_MAX_ALLOC_SIZE))
        };
        let size = size + redzone;
        // When we validated the max allocation size above, we did not take into account the
        // asan redzone. Unfortunately this cannot presently be checked statically due to the
        // asan_heap_redzone_size not being a constexpr, and so we check it here instead.
        assert!(
            SizeToIndexRet::size_to_index_allocating(max_redzone + HEAP_MAX_ALLOC_SIZE).rounded_up
                + size_of::<Header>()
                <= HEAP_LARGE_ALLOC_BYTES
        );
        size
    }

    /// Poisons the allocation's header and trailing right redzone, while
    /// unpoisoning its usable payload.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `head` and `redzone` regions are backed by
    /// KASAN shadow memory.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn poison_allocation(
        head_addr: usize,
        payload_addr: usize,
        alloc_size: usize,
    ) {
        let redzone_start = payload_addr + alloc_size;
        // SAFETY: Caller guarantees the header, payload, and redzone regions are backed by KASAN
        // shadow memory, and `alloc_size <= HEAP_MAX_ALLOC_SIZE`.
        unsafe {
            asan_poison_shadow(head_addr, size_of::<Header>(), ASAN_HEAP_LEFT_REDZONE_MAGIC);
            let redzone_size = asan_heap_redzone_size(alloc_size);
            asan_poison_shadow(redzone_start, redzone_size, ASAN_HEAP_LEFT_REDZONE_MAGIC);
            asan_unpoison_shadow(payload_addr, alloc_size);
        }
    }

    /// Poisons the memalign allocation and its padding.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `[unaligned_addr, unaligned_addr + padded_size)`
    /// is backed by KASAN shadow memory.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn poison_memalign_padding(unaligned_addr: usize, padded_size: usize) {
        // TODO(https://fxbug.dev/42104852): Separately poison padding and the post-buffer
        // redzone.
        // SAFETY: Caller guarantees `[unaligned_addr, unaligned_addr + padded_size)` is backed by
        // KASAN shadow memory.
        unsafe {
            asan_poison_shadow(unaligned_addr, padded_size, ASAN_HEAP_LEFT_REDZONE_MAGIC);
        }
    }

    /// Unpoisons the memalign allocation payload.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `[payload_addr, payload_addr + size)` is backed
    /// by KASAN shadow memory.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn unpoison_memalign_payload(payload_addr: usize, size: usize) {
        // SAFETY: Caller guarantees `[payload_addr, payload_addr + size)` is backed by KASAN
        // shadow memory.
        unsafe {
            asan_unpoison_shadow(payload_addr, size);
        }
    }

    /// Quarantines the freed allocation if quarantine is enabled. Returns null if quarantined,
    /// or the oldest evicted allocation `Header` if the quarantine was full.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is held and `header` is a valid
    /// allocated `Header`.
    #[sanitize(address = "off")]
    #[inline]
    pub(super) unsafe fn quarantine_or_free(
        theheap: *mut Heap,
        payload: *mut c_void,
        header: *mut Header,
        header_size: usize,
    ) -> *mut Header {
        // SAFETY: Caller holds `TheHeapLock` and `header` is a valid allocated `Header` whose
        // payload is backed by KASAN shadow memory.
        unsafe {
            asan_poison_shadow(
                payload.addr(),
                header_size - size_of::<Header>(),
                ASAN_HEAP_FREE_MAGIC,
            );
            (*theheap).asan_quarantine.push(header)
        }
    }
}

#[cfg(not(feature = "userspace"))]
#[cfg(sanitize = "address")]
use kernel_asan as asan;

#[cfg_attr(not(feature = "userspace"), cfg(not(sanitize = "address")))]
use asan_stub as asan;

use asan::Quarantine;

struct Heap {
    /// Total bytes allocated from the OS for the heap.
    size: usize,

    /// Bytes of usable free space in the heap.
    // Safety invariant: Equals the sum of `(*free_area).header.size` across all nodes in
    // `free_lists`.
    remaining: usize,

    /// A non-large OS allocation that could have been freed to the OS but
    /// wasn't. We will attempt to use this before allocating more memory from
    /// the OS, to reduce churn. May be null. If non-null, `cached_os_alloc.size`
    /// holds the total size allocated from the OS for this block.
    // Safety invariant: Either null or points to a page-aligned OS allocation of
    // `(*cached_os_alloc).size` page-rounded bytes (`>= size_of::<Header>()`).
    cached_os_alloc: *mut Header,

    /// Free lists, bucketed by size. See [`size_to_index_helper`].
    // Safety invariant: Each `free_lists[i]` is either null or the head of a valid doubly-linked
    // list of `Free` nodes whose usable sizes map to bucket `i`.
    free_lists: [*mut Free; NUMBER_OF_BUCKETS],

    /// Bitmask that tracks whether a given `free_lists` entry has any elements.
    /// See [`set_free_list_bit`], [`clear_free_list_bit`].
    // Safety invariant: Bit `31 - (i & 0x1f)` of `free_list_bits[i >> 5]` is 1 iff
    // `free_lists[i]` is non-null.
    free_list_bits: [u32; BUCKET_WORDS],

    asan_quarantine: Quarantine,
}

impl Heap {
    const fn new() -> Self {
        Self {
            size: 0,
            remaining: 0,
            cached_os_alloc: ptr::null_mut(),
            free_lists: [ptr::null_mut(); NUMBER_OF_BUCKETS],
            free_list_bits: [0; BUCKET_WORDS],
            asan_quarantine: Quarantine::new(),
        }
    }
}

struct SyncHeap(UnsafeCell<Heap>);

// SAFETY: Access to `THE_HEAP` is serialized by `TheHeapLock` (except during panic-time
// `cmpct_dump`, when all other CPUs are halted).
unsafe impl Sync for SyncHeap {}

impl SyncHeap {
    const fn new() -> Self {
        Self(UnsafeCell::new(Heap::new()))
    }

    #[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
    #[inline]
    const fn get(&self) -> *mut Heap {
        self.0.get()
    }
}

/// Global `cmpctmalloc` heap state protected by [`TheHeapLock`].
static THE_HEAP: SyncHeap = SyncHeap::new();
/// Maximum allocation size automatically zero-filled on allocation.
///
/// `AtomicUsize` is used because `FILL_ON_ALLOC_THRESHOLD` is read (in `cmpct_alloc` after
/// releasing `TheHeapLock`) and written (in `cmpct_set_fill_on_alloc_threshold`) without holding
/// `TheHeapLock`. `Ordering::Relaxed` is sufficient for all accesses because it is an independent
/// configuration threshold with no dependent memory operations to synchronize.
static FILL_ON_ALLOC_THRESHOLD: AtomicUsize = AtomicUsize::new(0);

/// Asserts that `$cond` is true in debug builds and hints to the optimizer that it is
/// always true in release builds.
///
/// # Safety
///
/// The caller must guarantee that `$cond` holds. Violating this is immediate undefined
/// behavior in release builds.
macro_rules! assume {
    ($cond:expr $(,)?) => {{
        let cond: bool = $cond;
        debug_assert!(cond, "assertion failed: {}", stringify!($cond));
        core::hint::assert_unchecked(cond);
    }};
}

#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
const fn round_up(val: usize, align: usize) -> usize {
    (val + align - 1) & !(align - 1)
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `pages > 0`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn heap_page_alloc(pages: usize) -> *mut c_void {
    // SAFETY: Caller holds `TheHeapLock` and passes `pages > 0`.
    unsafe { cmpctmalloc_ffi::heap_page_alloc(pages) }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `ptr` is a page-aligned pointer previously
/// returned by `heap_page_alloc`, and `pages > 0`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn heap_page_free(ptr: *mut c_void, pages: usize) {
    // SAFETY: Caller holds `TheHeapLock` and passes a valid page-aligned OS allocation.
    unsafe { cmpctmalloc_ffi::heap_page_free(ptr, pages) }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is not held on the current thread.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn heap_report_alloc_failure(size: usize) {
    // SAFETY: Caller has released `TheHeapLock`.
    unsafe { cmpctmalloc_ffi::heap_report_alloc_failure(size) }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held (or all other CPUs are halted in a kernel
/// panic with no concurrent mutation of `THE_HEAP`) and `header` points to a valid `Header`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
unsafe fn dump_free(header: *mut Header) {
    // This function accesses header->size so it has to be NO_ASAN
    // SAFETY: Caller guarantees `header` is a valid `Header` pointer.
    unsafe {
        let size = (*header).size;
        dprintf!(
            INFO,
            "\t\tbase {:p}, end {:#x}, len {:#x} ({})\n",
            header,
            header.addr() + (size as usize),
            size,
            size
        );
    }
}

/// # Safety
///
/// The caller must ensure that either `TheHeapLock` is held or all other CPUs are halted in a
/// kernel panic with no concurrent mutation of `THE_HEAP`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
unsafe fn cmpct_dump_locked(options: CmpctDumpOptions) {
    // This function accesses Free * that are poisoned so it has to be NO_ASAN
    // SAFETY: Caller guarantees `TheHeapLock` is held (or all other CPUs are halted in a kernel
    // panic with no concurrent mutation of `THE_HEAP`) and all nodes on `free_lists` are valid
    // `Free` pointers.
    unsafe {
        let theheap = THE_HEAP.get();
        dprintf!(INFO, "Heap dump (using cmpctmalloc):\n");
        dprintf!(
            INFO,
            "\tsize {}, remaining {}, (used {}) cached free {}\n",
            (*theheap).size,
            (*theheap).remaining,
            (*theheap).size - (*theheap).remaining,
            if !(*theheap).cached_os_alloc.is_null() {
                (*(*theheap).cached_os_alloc).size
            } else {
                0
            }
        );

        if options.contains(CmpctDumpOptions::VERBOSE) {
            dprintf!(INFO, "\tfree list:\n");
            for i in 0..NUMBER_OF_BUCKETS {
                let mut header_printed = false;
                let mut free_area = (*theheap).free_lists[i];
                while !free_area.is_null() {
                    assert!(free_area != (*free_area).next);
                    if !header_printed {
                        dprintf!(INFO, "\tbucket {}\n", i);
                        header_printed = true;
                    }
                    dump_free(&raw mut (*free_area).header);
                    free_area = (*free_area).next;
                }
            }
        } else {
            dprintf!(INFO, "Verbose options not passed. Skipping free list dump\n");
        }
    }
}

#[derive(Copy, Clone)]
struct SizeToIndexRet {
    bucket: i32,
    rounded_up: usize,
}

/// Operates in sizes that don't include the allocation header;
/// i.e., the usable portion of a memory area.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
const fn size_to_index_helper(mut size: usize, adjust: isize, increment: i32) -> SizeToIndexRet {
    let rounded_up: usize;
    // First buckets are simply 8-spaced up to 128.
    if size <= 128 {
        if size_of::<usize>() == 8 && size <= size_of::<Free>() - size_of::<Header>() {
            rounded_up = size_of::<Free>() - size_of::<Header>();
        } else {
            rounded_up = size;
        }
        // No allocation is smaller than 8 bytes, so the first bucket is for 8
        // byte spaces (not including the header).  For 64 bit, the free list
        // struct is 16 bytes larger than the header, so no allocation can be
        // smaller than that (otherwise how to free it), but we have empty 8
        // and 16 byte buckets for simplicity.
        let shifted = if (size >> 3) >= 1 { size >> 3 } else { 1 };
        return SizeToIndexRet { bucket: (shifted as i32) - 1, rounded_up };
    }

    // We are going to go up to the next size to round up, but if we hit a
    // bucket size exactly we don't want to go up. By subtracting 8 here, we
    // will do the right thing (the carry propagates up for the round numbers
    // we are interested in).
    size = size.wrapping_add_signed(adjust);
    // After 128 the buckets are logarithmically spaced, every 16 up to 256,
    // every 32 up to 512 etc.  This can be thought of as rows of 8 buckets.
    // GCC intrinsic count-leading-zeros.
    // Eg. 128-255 has 24 leading zeros and we want row to be 4.
    let row: u32 = (size_of::<usize>() as u32) * 8 - 4 - size.leading_zeros();
    // For row 4 we want to shift down 4 bits.
    let column: u32 = ((size >> row) & 7) as u32;
    let mut row_column: i32 = ((row << 3) | column) as i32;
    row_column += increment;
    size = (8 + ((row_column & 7) as usize)) << (row_column >> 3);
    rounded_up = size;
    // We start with 15 buckets, 8, 16, 24, 32, 40, 48, 56, 64, 72, 80, 88, 96,
    // 104, 112, 120.  Then we have row 4, sizes 128 and up, with the
    // row-column 8 and up.
    let answer: i32 = row_column + 15 - 32;
    debug_assert!((answer as usize) < NUMBER_OF_BUCKETS);
    SizeToIndexRet { bucket: answer, rounded_up }
}

impl SizeToIndexRet {
    /// Round up size to next bucket when allocating.
    #[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
    #[inline]
    const fn size_to_index_allocating(size: usize) -> SizeToIndexRet {
        let rounded = round_up(size, 8);
        size_to_index_helper(rounded, -8, 1)
    }
}

/// Round down size to next bucket when freeing.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
const fn size_to_index_freeing(size: usize) -> i32 {
    size_to_index_helper(size, 0, 0).bucket
}

// Ensure that HEAP_LARGE_ALLOC_BYTES maps to a valid bucket when allocating.
// Given how HEAP_LARGE_ALLOC_BYTES is defined this assert is somewhat excessive
// but included for completeness should HEAP_LARGE_ALLOC_BYTES ever be given a
// more complicated definition. Note that HEAP_LARGE_ALLOC_BYTES is the internal
// size (including header), but we map to a bucket using the size without the
// header, as the header is implicitly included in the final bucket. Or put
// another way, the bucket for size X gives a bucket with a free block of
// X+size_of::<Header>().
zr::static_assert!(
    (SizeToIndexRet::size_to_index_allocating(HEAP_LARGE_ALLOC_BYTES - size_of::<Header>()).bucket
        as usize)
        <= NUMBER_OF_BUCKETS
);

#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
fn tag_as_free(left: *mut c_void) -> *mut Header {
    left.map_addr(|addr| addr | FREE_BIT).cast::<Header>()
}

/// Returns true if this `Header` is marked as free.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `header` points to a valid `Header`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn is_tagged_as_free(header: *const Header) -> bool {
    // The free bit is stashed in the lower bit of header->left.
    // SAFETY: Caller guarantees `header` is a valid `Header` pointer.
    unsafe { ((*header).left.addr() & FREE_BIT) != 0 }
}

#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
fn untag(left: *const c_void) -> *mut Header {
    left.cast_mut().map_addr(|addr| addr & !HEADER_LEFT_BIT_MASK).cast::<Header>()
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `header` points to a valid `Header` within
/// an OS allocation, and `header.cast::<u8>().add((*header).size as usize)` points to the adjacent
/// right `Header` within the same OS allocation.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn right_header(header: *mut Header) -> *mut Header {
    // SAFETY: Caller guarantees `header` is a valid `Header` and `header + header->size` points to
    // the adjacent `Header` within the same OS allocation.
    unsafe { header.cast::<u8>().add((*header).size as usize).cast::<Header>() }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `index < NUMBER_OF_BUCKETS`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn set_free_list_bit(index: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `index < NUMBER_OF_BUCKETS`
    // (`index >> 5 < BUCKET_WORDS`).
    unsafe {
        assume!(index < NUMBER_OF_BUCKETS);
        let theheap = THE_HEAP.get();
        (*theheap).free_list_bits[index >> 5] |= 1u32 << (31 - (index & 0x1f));
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `index < NUMBER_OF_BUCKETS`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn clear_free_list_bit(index: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `index < NUMBER_OF_BUCKETS`
    // (`index >> 5 < BUCKET_WORDS`).
    unsafe {
        assume!(index < NUMBER_OF_BUCKETS);
        let theheap = THE_HEAP.get();
        (*theheap).free_list_bits[index >> 5] &= !(1u32 << (31 - (index & 0x1f)));
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `index < NUMBER_OF_BUCKETS`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn find_nonempty_bucket(mut index: usize) -> i32 {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `index < NUMBER_OF_BUCKETS`.
    unsafe {
        assume!(index < NUMBER_OF_BUCKETS);
        let theheap = THE_HEAP.get();
        let mut mask: u32 = (1u32 << (31 - (index & 0x1f))) - 1;
        mask = mask.wrapping_mul(2).wrapping_add(1);
        mask &= (*theheap).free_list_bits[index >> 5];
        if mask != 0 {
            return ((index & !0x1f) as i32) + (mask.leading_zeros() as i32);
        }
        index = round_up(index + 1, 32);
        while index < NUMBER_OF_BUCKETS {
            mask = (*theheap).free_list_bits[index >> 5];
            if mask != 0 {
                return (index as i32) + (mask.leading_zeros() as i32);
            }
            index += 32;
        }
        -1
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `header` points to a valid `Header`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn is_start_of_os_allocation(header: *const Header) -> bool {
    // SAFETY: Caller guarantees `header` is a valid `Header` pointer.
    unsafe { untag((*header).left.cast()) == untag(ptr::null()) }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `address` is 8-byte aligned
/// (`align_of::<Free>()`) and points to a writable region of at least `size` bytes within an OS
/// allocation, `size >= size_of::<Free>()`, and `size - size_of::<Header>()` maps via
/// `size_to_index_freeing` to a bucket index `< NUMBER_OF_BUCKETS`.
#[cfg_attr(
    not(feature = "userspace"),
    expect(inline_no_sanitize, reason = "all callers are also sanitize(address = \"off\")"),
    sanitize(address = "off")
)]
#[inline(always)]
unsafe fn create_free_area(address: *mut c_void, left: *mut c_void, size: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held, `address` is 8-byte aligned and valid for
    // `size >= size_of::<Free>()` bytes, and `index < NUMBER_OF_BUCKETS`.
    unsafe {
        assume!(size >= size_of::<Free>());
        let theheap = THE_HEAP.get();
        let free_area = address.cast::<Free>();
        (*free_area).header.size = size as u32;
        (*free_area).header.cookie = 0;
        (*free_area).header.left = tag_as_free(left);

        let index = size_to_index_freeing(size - size_of::<Header>()) as usize;
        assume!(index < NUMBER_OF_BUCKETS);
        set_free_list_bit(index);
        let bucket = &raw mut (*theheap).free_lists[index];

        let old_head = *bucket;
        if !old_head.is_null() {
            (*old_head).prev = free_area;
        }
        (*free_area).next = old_head;
        (*free_area).prev = ptr::null_mut();
        (*bucket) = free_area;
        (*theheap).remaining += size;
        fill_free_area(free_area, size);
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `address` points to a valid `Header`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn is_end_of_os_allocation(address: *mut u8) -> bool {
    // SAFETY: Caller guarantees `address` points to a valid `Header`.
    unsafe { (*address.cast::<Header>()).size == 0 }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `ptr` is page-aligned and was allocated by
/// `heap_page_alloc`, and `size` is page-aligned.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn free_to_os(ptr: *mut c_void, size: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `(ptr, size)` is a valid OS allocation.
    unsafe {
        let theheap = THE_HEAP.get();
        debug_assert!(page::is_page_rounded(ptr.addr()));
        debug_assert!(page::is_page_rounded(size));
        heap_page_free(ptr, size >> page::SHIFT);
        (*theheap).size -= size;
    }
}

/// May call `free_to_os()`, or may cache the (non-large) OS allocation in
/// `cached_os_alloc`. `left_sentinel` is the start of the OS allocation, and
/// `total_size` is the (page-aligned) number of bytes that were originally
/// allocated from the OS.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `left_sentinel` points to the start of a
/// page-aligned OS allocation of `total_size` bytes (`total_size <= u32::MAX as usize`).
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn possibly_free_to_os(left_sentinel: *mut Header, total_size: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `left_sentinel` is a valid OS allocation
    // of `total_size` bytes.
    unsafe {
        let theheap = THE_HEAP.get();
        if (*theheap).cached_os_alloc.is_null() {
            ltracef!("Keeping {:#x}-byte OS alloc @{:p}\n", total_size, left_sentinel);
            (*left_sentinel).left = ptr::null_mut();
            (*left_sentinel).size = total_size as u32;
            (*left_sentinel).cookie = 0;
            (*theheap).cached_os_alloc = left_sentinel;
        } else {
            ltracef!("Returning {:#x} bytes @{:p} to OS\n", total_size, left_sentinel);
            free_to_os(left_sentinel.cast(), total_size);
        }
    }
}

#[cold]
#[inline(never)]
fn panic_unexpected_left_sentinel_size(left_size: u32) -> ! {
    panic!("Unexpected left sentinel size {} != header size {}", left_size, size_of::<Header>());
}

#[cold]
#[inline(never)]
fn panic_remaining_underflow(remaining: usize, free_size: u32) -> ! {
    panic!("{} >= {}", remaining, free_size);
}

#[cold]
#[inline(never)]
fn panic_unrounded_os_alloc(size: usize, ptr: *mut c_void) -> ! {
    panic!("{:#x} bytes @{:p}", size, ptr);
}

#[cold]
#[inline(never)]
fn panic_header_size_too_small(header_size: u32) -> ! {
    panic!("got {} min {}", header_size, size_of::<Header>());
}

#[cold]
#[inline(never)]
fn panic_sized_free_too_small(header_size: u32, s: usize) -> ! {
    panic!("expected {} got {}", header_size, s);
}

/// Frees `size` bytes starting at `address`, either to a free bucket or to the
/// OS (in which case the left/right sentinels are freed as well). `address`
/// should point to what would be the `Header` of the memory area to free, and
/// `left` and `size` should be set to the values that the `Header` would have
/// contained. This is broken out because the `Header` will not contain the
/// proper size when coalescing neighboring areas.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `address` is 8-byte aligned and points to a
/// writable region of `size` bytes (`size >= size_of::<Free>()`) within an OS allocation whose
/// valid adjacent right `Header` is at `address.cast::<u8>().add(size)`, `untag(left)` points to
/// the valid preceding `Header` in the same OS allocation, and `size - size_of::<Header>()` maps
/// via `size_to_index_freeing` to a bucket `< NUMBER_OF_BUCKETS`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn free_memory(address: *mut c_void, left: *mut c_void, size: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `address`, `left`, and `size` describe a
    // valid memory area within an OS allocation.
    unsafe {
        let left = untag(left);
        if page::is_page_rounded(left.addr())
            && is_start_of_os_allocation(left)
            && is_end_of_os_allocation(address.cast::<u8>().add(size))
        {
            // Assert that it's safe to do a simple 2*size_of::<Header>() below.
            if cfg!(debug_assertions) && ((*left).size as usize) != size_of::<Header>() {
                panic_unexpected_left_sentinel_size((*left).size);
            }
            possibly_free_to_os(left, size + 2 * size_of::<Header>());
        } else {
            create_free_area(address, left.cast(), size);
        }
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `0 <= bucket < NUMBER_OF_BUCKETS as i32`,
/// and `free_area` is a valid `Free` node currently linked in `theheap.free_lists[bucket]`.
#[cfg_attr(
    not(feature = "userspace"),
    expect(inline_no_sanitize, reason = "all callers are also sanitize(address = \"off\")"),
    sanitize(address = "off")
)]
#[inline(always)]
unsafe fn unlink_free(free_area: *mut Free, bucket: i32) {
    // SAFETY: Caller guarantees `TheHeapLock` is held, `0 <= bucket < NUMBER_OF_BUCKETS as i32`
    // (`bucket_idx < NUMBER_OF_BUCKETS`), and `free_area` is linked in `bucket`.
    unsafe {
        let theheap = THE_HEAP.get();
        let free_size = (*free_area).header.size as usize;
        if (*theheap).remaining < free_size {
            panic_remaining_underflow((*theheap).remaining, (*free_area).header.size);
        }
        (*theheap).remaining -= free_size;
        let next = (*free_area).next;
        let prev = (*free_area).prev;
        let bucket_idx = bucket as usize;
        assume!(bucket_idx < NUMBER_OF_BUCKETS);
        if (*theheap).free_lists[bucket_idx] == free_area {
            (*theheap).free_lists[bucket_idx] = next;
            if next.is_null() {
                clear_free_list_bit(bucket_idx);
            }
        }
        if !prev.is_null() {
            (*prev).next = next;
        }
        if !next.is_null() {
            (*next).prev = prev;
        }
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `free_area` is a valid node currently
/// linked in its size-corresponding free list bucket.
#[cfg_attr(
    not(feature = "userspace"),
    expect(inline_no_sanitize, reason = "all callers are also sanitize(address = \"off\")"),
    sanitize(address = "off")
)]
#[inline(always)]
unsafe fn unlink_free_unknown_bucket(free_area: *mut Free) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `free_area` is a valid free node whose
    // `header.size >= size_of::<Free>()` and maps to its free list bucket `< NUMBER_OF_BUCKETS`.
    unsafe {
        let size = (*free_area).header.size as usize;
        assume!(size >= size_of::<Free>());
        unlink_free(free_area, size_to_index_freeing(size - size_of::<Header>()));
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held, `address.cast::<u8>().add(offset)` is within
/// the same OS allocation, 8-byte aligned (`align_of::<Header>()`), and writable for
/// `size_of::<Header>()` bytes, and `size <= u32::MAX as usize`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn create_allocation_header(
    address: *mut c_void,
    offset: usize,
    size: usize,
    left: *mut c_void,
) -> *mut c_void {
    // SAFETY: Caller guarantees `address + offset` is an 8-byte aligned writable `Header` location
    // within the OS allocation.
    unsafe {
        let standalone = address.cast::<u8>().add(offset).cast::<Header>();
        (*standalone).left = untag(left);
        (*standalone).size = size as u32;
        (*standalone).cookie = 0;
        standalone.wrapping_add(1).cast::<c_void>()
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `right` points to a valid `Header`.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn fix_left_pointer(right: *mut Header, new_left: *mut Header) {
    // SAFETY: Caller guarantees `right` is a valid `Header` pointer.
    unsafe {
        let tag = (*right).left.addr() & 1;
        (*right).left = new_left.map_addr(|addr| (addr & !1) | tag);
    }
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `new_area` points to a page-aligned
/// writable OS allocation of `size` bytes (`2 * size_of::<Header>() + size_of::<Free>() <= size <=
/// HEAP_LARGE_ALLOC_BYTES + HEAP_GROW_OVERHEAD`).
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
unsafe fn add_to_heap(new_area: *mut c_void, size: usize) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `new_area` is a page-aligned OS
    // allocation of `size` bytes with room for left/right sentinels and a `Free` area.
    unsafe {
        let top = new_area.cast::<u8>().add(size);
        // Set up the left sentinel. Its |left| field will not have FREE_BIT set,
        // stopping attempts to coalesce left.
        let left_sentinel = new_area.cast::<Header>();
        create_allocation_header(left_sentinel.cast(), 0, size_of::<Header>(), ptr::null_mut());

        // Set up the usable memory area, which will be marked free.
        let new_header = left_sentinel.add(1);
        let free_size = size - 2 * size_of::<Header>();
        create_free_area(new_header.cast(), left_sentinel.cast(), free_size);

        // Set up the right sentinel. Its |left| field will not have FREE_BIT bit
        // set, stopping attempts to coalesce right.
        let right_sentinel = top.sub(size_of::<Header>()).cast::<Header>();
        create_allocation_header(right_sentinel.cast(), 0, 0, new_header.cast());
    }
}

/// Create a new free-list entry of at least size bytes (including the
/// allocation header).  Called with the lock, apart from during init.
///
/// Returns true on success, false on failure.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
unsafe fn heap_grow(mut size: usize) -> bool {
    // This function accesses field members of Header which are poisoned so it
    // has to be NO_ASAN.

    // We expect to never have been asked to grow by more than the maximum
    // allocation
    debug_assert!(size <= HEAP_LARGE_ALLOC_BYTES);

    // Ensure that after performing the size manipulations below we do not end up
    // overflowing the maximum bucket. This check is useful since an obvious
    // setting of HEAP_LARGE_ALLOC_BYTES == 1<<HEAP_ALLOC_VIRTUAL_BITS will
    // actually result in us growing the heap by *more* than 1<<HEAP_ALLOC_VIRTUAL_BITS
    // However this is typically safe since growing by an extra partial page should
    // not send us into the next bucket. However if pages are large enough and
    // HEAP_ALLOC_VIRTUAL_BITS small enough this could happen, and so this assert
    // exists to prevent choosing such sizes.
    zr::static_assert!(
        (size_to_index_freeing(
            page::round_up(HEAP_LARGE_ALLOC_BYTES + HEAP_GROW_OVERHEAD)
                - HEAP_GROW_OVERHEAD
                - size_of::<Header>(),
        ) as usize)
            <= NUMBER_OF_BUCKETS
    );

    // The new free list entry will have a header on each side (the
    // sentinels) so we need to grow the gross heap size by this much more.
    size += HEAP_GROW_OVERHEAD;
    size = page::round_up(size);

    let mut ptr: *mut c_void = ptr::null_mut();

    // SAFETY: Caller guarantees `TheHeapLock` is held.
    unsafe {
        let theheap = THE_HEAP.get();
        let os_alloc = (*theheap).cached_os_alloc;
        if !os_alloc.is_null() {
            let os_alloc_size = (*os_alloc).size as usize;
            if os_alloc_size >= size {
                ltracef!(
                    "Using saved {:#x}-byte OS alloc @{:p} (>={:#x} bytes)\n",
                    (*os_alloc).size,
                    os_alloc,
                    size
                );
                ptr = os_alloc.cast();
                size = os_alloc_size;
                if cfg!(debug_assertions)
                    && (!page::is_page_rounded(ptr.addr()) || !page::is_page_rounded(size))
                {
                    panic_unrounded_os_alloc(size, ptr);
                }
            } else {
                // We need to allocate more from the OS. Return the cached OS
                // allocation, in case we're holding an unusually-small block
                // that's unlikely to satisfy future calls to heap_grow().
                ltracef!(
                    "Returning too-small saved {:#x}-byte OS alloc @{:p} (<{:#x} bytes)\n",
                    (*os_alloc).size,
                    os_alloc,
                    size
                );
                free_to_os(os_alloc.cast(), os_alloc_size);
            }
            (*theheap).cached_os_alloc = ptr::null_mut();
        }
        if ptr.is_null() {
            ptr = heap_page_alloc(size >> page::SHIFT);
            if ptr.is_null() {
                record_heap_grow_fail();
                return false;
            }
            ltracef!("Growing heap by {:#x} bytes, new ptr {:p}\n", size, ptr);
            (*theheap).size += size;
        }

        add_to_heap(ptr, size);
    }

    true
}

/****************************************************
 *
 * Public API
 *
 ****************************************************/

/// Factors in the header for an allocation. Value chosen here is hard coded and could be less than
/// the actual largest allocation that cmpct_alloc could provide. This is done so that larger
/// buckets can exist in order to allow the heap to grow by amounts larger than what we would like
/// to allow clients to allocate.
pub const HEAP_MAX_ALLOC_SIZE: usize = (1usize << 20) - size_of::<Header>();

// Ensure that the maximum allocation is actually satisfiable. Note that since
// HEAP_LARGE_ALLOC_BYTES is an internal allocation limit we have to add the header on. We have
// already checked previously that the HEAP_LARGE_ALLOC_BYTES is a valid amount to grow the heap by.
zr::static_assert!(
    SizeToIndexRet::size_to_index_allocating(HEAP_MAX_ALLOC_SIZE).rounded_up + size_of::<Header>()
        <= HEAP_LARGE_ALLOC_BYTES
);

/// Allocates `size` bytes from the compact heap, returning null if `size == 0`,
/// `size > HEAP_MAX_ALLOC_SIZE`, or the heap cannot grow.
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is not held on the current thread and the heap has
/// been initialized via [`cmpct_init`].
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
pub unsafe fn cmpct_alloc(size: usize) -> *mut c_void {
    local_trace_duration!("cmpct_alloc", _trace, "size" => size as u64);

    if size == 0 {
        return ptr::null_mut();
    }

    record_malloc_size(size);
    // Large allocations are no longer allowed. See https://fxbug.dev/42106180 for details.
    if size > HEAP_MAX_ALLOC_SIZE {
        return ptr::null_mut();
    }

    let alloc_size = size;
    let size = asan::adjust_alloc_size(size, alloc_size);

    let SizeToIndexRet { bucket, mut rounded_up } = SizeToIndexRet::size_to_index_allocating(size);
    let start_bucket = bucket as usize;

    rounded_up += size_of::<Header>();

    lock!(let mut guard = TheHeapLock::get().lock());
    local_trace_duration!("locked", _trace_lock);
    let theheap = THE_HEAP.get();
    // SAFETY: `TheHeapLock` is held and `start_bucket < NUMBER_OF_BUCKETS`.
    let mut bucket = unsafe { find_nonempty_bucket(start_bucket) };
    if bucket == -1 {
        // The maximum amount the heap should grow by is the maximum of:
        // * 12% of its total size,
        // * The HEAP_USABLE_GROW_SIZE, which is the HEAP_GROW_SIZE minus growth overhead.
        // * The size of the desired allocation rounded up to the nearest bucket.
        // SAFETY: `TheHeapLock` is held, granting exclusive access to `THE_HEAP`.
        let heap_size = unsafe { (*theheap).size };
        let max_growth_threshold = max(max(heap_size >> 3, HEAP_USABLE_GROW_SIZE), rounded_up);
        // The actual size we choose to grow by is the minimum of the max_growth_threshold and
        // the maximum allocation size cmpctmalloc can handle, which is stored in
        // HEAP_LARGE_ALLOC_BYTES.
        let mut growby = min(HEAP_LARGE_ALLOC_BYTES, max_growth_threshold);
        // Validate that our growby calculation is correct, and that if we grew the heap by
        // this amount we would actually satisfy our allocation.
        debug_assert!(growby >= rounded_up);
        // Try to add a new OS allocation to the heap, reducing the size until
        // we succeed or get too small.
        ktrace_duration_begin!(
            "kernel:memory",
            "heap_grow_with_retries",
            "allocation_size" => size as u64,
            "starting_search_size" => growby as u64
        );
        let mut attempts: i32 = 1;
        // SAFETY: `TheHeapLock` is held during each `heap_grow` call.
        while !unsafe { heap_grow(growby) } {
            if growby <= rounded_up {
                guard.as_mut().release();
                // SAFETY: `TheHeapLock` was just released above.
                unsafe { heap_report_alloc_failure(rounded_up) };
                ktrace_duration_end!(
                    "kernel:memory",
                    "heap_grow_with_retries",
                    "num_attempts" => attempts,
                    "status" => "failed"
                );
                return ptr::null_mut();
            }
            attempts += 1;
            growby = max(growby >> 1, rounded_up);
        }
        ktrace_duration_end!(
            "kernel:memory",
            "heap_grow_with_retries",
            "num_attempts" => attempts,
            "status" => "success"
        );
        // SAFETY: `TheHeapLock` is held and `start_bucket < NUMBER_OF_BUCKETS`.
        bucket = unsafe { find_nonempty_bucket(start_bucket) };
        // It should be the case that, since we hold the heap lock, after growing the heap
        // there should be something in our target bucket. However, if there was any confusion
        // in calculating the |growby| amount, then it's possible we still do not have
        // something. As this could only happen due to a systemic configuration error, and this
        // should get caught in tests, this only needs to be a DEBUG_ASSERT and not a always
        // enabled ASSERT. Further, it should not be possible for the assertion of the growby
        // amount above to succeed and then this assertion to fail.
        debug_assert!(bucket != -1);
    }
    let bucket_idx = bucket as usize;
    // SAFETY: `TheHeapLock` is held. Either `find_nonempty_bucket(start_bucket)` found an existing
    // non-empty bucket in `start_bucket..NUMBER_OF_BUCKETS`, or `heap_grow(growby)` succeeded with
    // `growby >= rounded_up` and populated a bucket `>= start_bucket`, so `0 <= bucket <
    // NUMBER_OF_BUCKETS`, `(*theheap).free_lists[bucket_idx]` is a valid non-null `Free` node with
    // `header.size >= rounded_up`, and `head`, `result`, and the trailing redzone are backed by
    // KASAN shadow memory.
    let result = unsafe {
        assume!(bucket_idx < NUMBER_OF_BUCKETS);
        let head = (*theheap).free_lists[bucket_idx];
        let left_over = ((*head).header.size as usize) - rounded_up;
        // We can't carve off the rest for a new free space if it's smaller than the
        // free-list linked structure.  We also don't carve it off if it's less than
        // 1.6% the size of the allocation.  This is to avoid small long-lived
        // allocations being placed right next to large allocations, hindering
        // coalescing and returning pages to the OS.
        if left_over >= size_of::<Free>() && left_over > (size >> 6) {
            let right = right_header(&raw mut (*head).header);
            unlink_free(head, bucket);
            let free = head.cast::<u8>().add(rounded_up).cast::<c_void>();
            create_free_area(free, head.cast(), left_over);
            fix_left_pointer(right, free.cast::<Header>());
            (*head).header.size -= left_over as u32;
        } else {
            unlink_free(head, bucket);
        }
        let result = create_allocation_header(
            head.cast(),
            0,
            (*head).header.size as usize,
            (*head).header.left.cast(),
        );
        fill_alloc_area(result, size, rounded_up);
        asan::poison_allocation(head.addr(), result.addr(), alloc_size);
        result
    };

    guard.as_mut().release();
    if alloc_size < FILL_ON_ALLOC_THRESHOLD.load(Ordering::Relaxed) {
        // SAFETY: `result` points to a newly allocated, unaliased writable payload of at least
        // `alloc_size` bytes, and `Ordering::Relaxed` is sufficient for reading the independent
        // `FILL_ON_ALLOC_THRESHOLD` configuration value.
        unsafe { ptr::write_bytes(result.cast::<u8>(), 0, alloc_size) };
    }
    result
}

/// # Safety
///
/// The caller must ensure that `TheHeapLock` is held and `header` (`payload - 1`) is a valid
/// allocated `Header` that has not yet been freed.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
unsafe fn cmpct_free_internal(payload: *mut c_void, mut header: *mut Header) {
    // SAFETY: Caller guarantees `TheHeapLock` is held and `header` is a valid allocated `Header`.
    let header_size = unsafe {
        debug_assert!(!is_tagged_as_free(header)); // Double free!
        (*header).size
    };
    if (header_size as usize) <= size_of::<Header>() {
        panic_header_size_too_small(header_size);
    }

    // SAFETY: `TheHeapLock` is held and `header` is a valid allocated `Header`.
    header =
        unsafe { asan::quarantine_or_free(THE_HEAP.get(), payload, header, header_size as usize) };
    if header.is_null() {
        return;
    }

    // SAFETY: `TheHeapLock` is held and `header` is a valid allocated (or quarantine-evicted)
    // `Header` within an OS allocation bounded by non-free left and right sentinels.
    unsafe {
        let size = (*header).size as usize;
        let left = (*header).left;
        if !left.is_null() && is_tagged_as_free(left) {
            // Coalesce with left free object.
            unlink_free_unknown_bucket(left.cast::<Free>());
            let right = right_header(header);
            if is_tagged_as_free(right) {
                // Coalesce both sides.
                unlink_free_unknown_bucket(right.cast::<Free>());
                let right_right = right_header(right);
                fix_left_pointer(right_right, left);
                free_memory(
                    left.cast(),
                    (*left).left.cast(),
                    ((*left).size as usize) + size + ((*right).size as usize),
                );
            } else {
                // Coalesce only left.
                fix_left_pointer(right, left);
                free_memory(left.cast(), (*left).left.cast(), ((*left).size as usize) + size);
            }
        } else {
            let right = right_header(header);
            if is_tagged_as_free(right) {
                // Coalesce only right.
                let right_right = right_header(right);
                unlink_free_unknown_bucket(right.cast::<Free>());
                fix_left_pointer(right_right, header);
                free_memory(header.cast(), left.cast(), size + ((*right).size as usize));
            } else {
                free_memory(header.cast(), left.cast(), size);
            }
        }
    }
}

/// Associates a user-defined `cookie` with an allocation `ptr` returned from [`cmpct_alloc`] or
/// [`cmpct_memalign`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null and points to a live allocation returned by
/// [`cmpct_alloc`] or [`cmpct_memalign`], with no concurrent access to its cookie.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
pub unsafe fn cmpct_set_cookie(ptr: *mut c_void, cookie: u32) {
    // SAFETY: Caller guarantees `ptr` is a valid non-null allocation payload pointer preceded by
    // its `Header`.
    unsafe {
        let header = ptr.cast::<Header>().sub(1);
        (*header).cookie = cookie;
    }
}

/// Retrieves the user-defined cookie associated with an allocation `ptr` returned from
/// [`cmpct_alloc`] or [`cmpct_memalign`].
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null and points to a live allocation returned by
/// [`cmpct_alloc`] or [`cmpct_memalign`], with no concurrent mutation of its cookie.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
pub unsafe fn cmpct_get_cookie(ptr: *mut c_void) -> u32 {
    // SAFETY: Caller guarantees `ptr` is a valid non-null allocation payload pointer preceded by
    // its `Header`.
    unsafe {
        let header = ptr.cast::<Header>().sub(1);
        (*header).cookie
    }
}

/// Returns the allocation size (including header and rounding) for `ptr`.
///
/// # Safety
///
/// The caller must ensure that `ptr` is non-null and points to a live allocation returned by
/// [`cmpct_alloc`] or [`cmpct_memalign`].
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
pub unsafe fn cmpct_get_size(ptr: *mut c_void) -> u32 {
    // SAFETY: Caller guarantees `ptr` is a valid non-null allocation payload pointer preceded by
    // its `Header`.
    unsafe {
        let header = ptr.cast::<Header>().sub(1);
        (*header).size
    }
}

/// Frees `payload` previously returned by [`cmpct_alloc`] or [`cmpct_memalign`].
///
/// # Safety
///
/// The caller must ensure that `payload` is null or a live allocation pointer returned by
/// [`cmpct_alloc`] or [`cmpct_memalign`], and `TheHeapLock` is not held on the current thread.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
pub unsafe fn cmpct_free(payload: *mut c_void) {
    local_trace_duration!("cmpct_free", _trace);
    if payload.is_null() {
        return;
    }

    lock!(let _guard = TheHeapLock::get().lock());
    local_trace_duration!("locked", _trace_locked);
    // SAFETY: Caller guarantees `payload` is a valid non-null allocation pointer and `TheHeapLock`
    // is held.
    unsafe {
        let header = payload.cast::<Header>().sub(1);
        cmpct_free_internal(payload, header);
    }
}

/// Frees `payload` of expected size `s` previously returned by [`cmpct_alloc`] or
/// [`cmpct_memalign`].
///
/// # Safety
///
/// The caller must ensure that `payload` is null or a live allocation pointer returned by
/// [`cmpct_alloc`] or [`cmpct_memalign`] of at least `s` bytes, and `TheHeapLock` is not held on
/// the current thread.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
#[inline]
pub unsafe fn cmpct_sized_free(payload: *mut c_void, s: usize) {
    local_trace_duration!("cmpct_free", _trace);
    if payload.is_null() {
        return;
    }

    lock!(let _guard = TheHeapLock::get().lock());
    local_trace_duration!("locked", _trace_locked);
    // SAFETY: Caller guarantees `payload` is a valid non-null allocation pointer and `TheHeapLock`
    // is held.
    unsafe {
        let header = payload.cast::<Header>().sub(1);
        // header->size is the size of the heap block |payload| is in, plus size_of::<Header>(),
        // plus the difference between the block size and the requested allocation size. If kernel
        // ASAN is enabled, it also includes an ASAN redzone.
        let header_size = (*header).size;
        if (header_size as usize) < s {
            panic_sized_free_too_small(header_size, s);
        }
        cmpct_free_internal(payload, header);
    }
}

/// Allocates `size` bytes aligned to `alignment` bytes from the compact heap.
///
/// # Safety
///
/// The caller must ensure that `alignment` is a power of two (or `< 8`), `TheHeapLock` is not
/// held on the current thread, and the heap has been initialized via [`cmpct_init`].
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
pub unsafe fn cmpct_memalign(alignment: usize, size: usize) -> *mut c_void {
    local_trace_duration!(
        "cmpct_memalign",
        _trace,
        "alignment" => alignment as u64,
        "size" => size as u64
    );
    if size == 0 {
        return ptr::null_mut();
    }
    if alignment < 8 {
        // SAFETY: Caller guarantees the heap is initialized and `TheHeapLock` is not held.
        return unsafe { cmpct_alloc(size) };
    }

    let padded_size = size.saturating_add(alignment).saturating_add(size_of::<Free>());

    // SAFETY: Caller guarantees the heap is initialized and `TheHeapLock` is not held.
    let unaligned = unsafe { cmpct_alloc(padded_size).cast::<u8>() };
    if unaligned.is_null() {
        return ptr::null_mut();
    }

    lock!(let mut guard = TheHeapLock::get().lock());
    local_trace_duration!("locked", trace_lock);
    // SAFETY: `unaligned` points to a live heap allocation of at least `padded_size` bytes backed
    // by KASAN shadow memory.
    unsafe {
        asan::poison_memalign_padding(unaligned.addr(), padded_size);
    }

    let mask = alignment - 1;
    let payload_int = unaligned.addr() + size_of::<Free>() + mask;
    let payload = unaligned.with_addr(payload_int & !mask);
    if unaligned != payload {
        // SAFETY: `TheHeapLock` is held, `unaligned` is a valid allocation of at least
        // `size + alignment + size_of::<Free>()` bytes, and `payload.addr() - unaligned.addr() >=
        // size_of::<Free>()`, so `unaligned_header` and `header` are valid non-overlapping
        // `Header` locations within the same OS allocation.
        unsafe {
            let unaligned_header = unaligned.cast::<Header>().sub(1);
            let header = payload.cast::<Header>().sub(1);
            let left_over = payload.addr() - unaligned.addr();
            create_allocation_header(
                header.cast(),
                0,
                ((*unaligned_header).size as usize) - left_over,
                unaligned_header.cast(),
            );
            let right = right_header(unaligned_header);
            (*unaligned_header).size = left_over as u32;
            (*unaligned_header).cookie = 0;
            fix_left_pointer(right, header);
        }
        local_trace_duration_end!(trace_lock);
        guard.as_mut().release();
        // SAFETY: `TheHeapLock` was just released above, and `unaligned` now has a valid
        // standalone allocation header of size `left_over >= size_of::<Free>()`.
        unsafe { cmpct_free(unaligned.cast()) };
    }

    // TODO: Free the part after the aligned allocation.
    // SAFETY: `payload` points to `size` bytes within the allocation backed by KASAN shadow
    // memory.
    unsafe {
        asan::unpoison_memalign_payload(payload.addr(), size);
    }
    payload.cast::<c_void>()
}

/// Zero-fill allocations smaller than `size`.
#[inline]
pub fn cmpct_set_fill_on_alloc_threshold(size: usize) {
    FILL_ON_ALLOC_THRESHOLD.store(size, Ordering::Relaxed);
}

/// Initializes the compact heap, growing it by [`HEAP_USABLE_GROW_SIZE`].
///
/// # Safety
///
/// The caller must ensure that `TheHeapLock` is not held and no concurrent heap operations are in
/// flight.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
pub unsafe fn cmpct_init() {
    ltrace_entry!();
    lock!(let _guard = TheHeapLock::get().lock());

    // SAFETY: `TheHeapLock` is held, granting exclusive access to `THE_HEAP`.
    unsafe {
        let theheap = THE_HEAP.get();
        // Initialize the free lists.
        for i in 0..NUMBER_OF_BUCKETS {
            (*theheap).free_lists[i] = ptr::null_mut();
        }
        for i in 0..BUCKET_WORDS {
            (*theheap).free_list_bits[i] = 0;
        }

        (*theheap).size = 0;
        (*theheap).remaining = 0;
        (*theheap).cached_os_alloc = ptr::null_mut();
    }

    // SAFETY: `TheHeapLock` is held and `THE_HEAP` metadata is initialized.
    let success = unsafe { heap_grow(HEAP_USABLE_GROW_SIZE) };
    assert!(success);

    debug_fill::on_init();
}

/// Dumps heap state to the debug log.
///
/// # Safety
///
/// If `options` includes [`CmpctDumpOptions::PANIC_TIME`], the caller must ensure that all other
/// CPUs are halted in a kernel panic with no concurrent mutation of `THE_HEAP`. Otherwise, the
/// caller must ensure that `TheHeapLock` is not held on the current thread.
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
pub unsafe fn cmpct_dump(options: CmpctDumpOptions) {
    if options.contains(CmpctDumpOptions::PANIC_TIME) {
        // If we are panic'ing, just skip the lock.  All bets are off anyway.
        // SAFETY: Caller guarantees that when `PANIC_TIME` is set, all other CPUs are halted in a
        // kernel panic with no concurrent mutation of `THE_HEAP`.
        unsafe { cmpct_dump_locked(options) };
    } else {
        lock!(let _guard = TheHeapLock::get().lock());
        // SAFETY: `TheHeapLock` is held.
        unsafe { cmpct_dump_locked(options) };
    }
}

/// Heap byte totals returned by [`cmpct_get_info`].
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub struct CmpctInfo {
    /// Total bytes allocated from the OS for the heap.
    pub used_bytes: usize,
    /// Bytes of usable free space in the heap.
    pub free_bytes: usize,
    /// Bytes held in `cached_os_alloc`.
    pub cached_bytes: usize,
}

/// Queries heap byte totals (`used_bytes`, `free_bytes`, `cached_bytes`).
#[cfg_attr(not(feature = "userspace"), sanitize(address = "off"))]
pub fn cmpct_get_info() -> CmpctInfo {
    lock!(let _guard = TheHeapLock::get().lock());
    // SAFETY: `TheHeapLock` is held, granting exclusive access to `THE_HEAP`.
    unsafe {
        let theheap = THE_HEAP.get();
        let used_bytes = (*theheap).size;
        let free_bytes = (*theheap).remaining;
        let cached_bytes = if !(*theheap).cached_os_alloc.is_null() {
            (*(*theheap).cached_os_alloc).size as usize
        } else {
            0
        };
        CmpctInfo { used_bytes, free_bytes, cached_bytes }
    }
}

pub use tests::cmpct_test;

#[cfg(feature = "heap_enable_tests")]
mod tests {
    use core::ffi::c_void;
    use core::mem::size_of;
    use core::ptr;
    use ksync::lock;

    use super::{
        CmpctDumpOptions, Free, Header, SizeToIndexRet, THE_HEAP, TheHeapLock, cmpct_alloc,
        cmpct_dump, cmpct_free, cmpct_memalign, kprintln, round_up, size_to_index_freeing,
    };

    /// Use `HEAP_ENABLE_TESTS` to enable internal testing. The tests are not useful
    /// when the target system is up. By that time we have done hundreds of allocations
    /// already.
    #[inline]
    fn cmpct_heap_remaining() -> usize {
        lock!(let _guard = TheHeapLock::get().lock());
        // SAFETY: `TheHeapLock` is held.
        unsafe { (*THE_HEAP.get()).remaining }
    }

    fn cmpct_test_buckets() {
        // Check for the 8-spaced buckets up to 128.
        for i in 1..=128usize {
            // Round up when allocating.
            let SizeToIndexRet { bucket, rounded_up: rounded } =
                SizeToIndexRet::size_to_index_allocating(i);
            let bucket = bucket as u32;
            let expected = ((round_up(i, 8) >> 3) - 1) as u32;
            assert_eq!(bucket, expected);
            assert_eq!(rounded & 7, 0);
            assert!(rounded >= i);
            if i >= size_of::<Free>() - size_of::<Header>() {
                // Once we get above the size of the free area struct (4 words), we
                // won't round up much for these small size.
                assert!(rounded - i < 8);
            }
            // Only rounded sizes are freed.
            if (i & 7) == 0 {
                // Up to size 128 we have exact buckets for each multiple of 8.
                assert_eq!(bucket, size_to_index_freeing(i) as u32);
            }
        }
        let mut bucket_base: i32 = 7;
        let mut j: usize = 16;
        while j < 1024 {
            // Note the "<=", which ensures that we test the powers of 2 twice to
            // ensure that both ways of calculating the bucket number match.
            for i in (j * 8)..=(j * 16) {
                // Round up to j multiple in this range when allocating.
                let SizeToIndexRet { bucket, rounded_up: rounded } =
                    SizeToIndexRet::size_to_index_allocating(i);
                let bucket = bucket as u32;
                let expected = (bucket_base as u32) + ((round_up(i, j) / j) as u32);
                assert_eq!(bucket, expected);
                assert_eq!(rounded % j, 0);
                assert!(rounded >= i);
                assert!(rounded - i < j);
                // Only 8-rounded sizes are freed or chopped off the end of a free
                // area when allocating.
                if (i & 7) == 0 {
                    // When freeing, if we don't hit the size of the bucket
                    // precisely, we have to put the free space into a smaller
                    // bucket, because the buckets have entries that will always
                    // be big enough for the corresponding allocation size (so we
                    // don't have to traverse the free chains to find a big enough
                    // one).
                    if (i % j) == 0 {
                        assert_eq!(bucket as i32, size_to_index_freeing(i));
                    } else {
                        assert_eq!((bucket as i32) - 1, size_to_index_freeing(i));
                    }
                }
            }
            j *= 2;
            bucket_base += 8;
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is not held and the heap is initialized.
    unsafe fn cmpct_test_get_back_newly_freed_helper(size: usize) {
        // SAFETY: Test helper exercises `cmpct_alloc` and `cmpct_free`.
        unsafe {
            let allocated = cmpct_alloc(size);
            if allocated.is_null() {
                return;
            }
            let allocated2 = cmpct_alloc(8).cast::<u8>();
            let expected_position = allocated.cast::<u8>().wrapping_add(size);
            if allocated2 < expected_position || allocated2 > expected_position.wrapping_add(128) {
                // If the allocated2 allocation is not in the same OS allocation as the
                // first allocation then the test may not work as expected (the memory
                // may be returned to the OS when we free the first allocation, and we
                // might not get it back).
                cmpct_free(allocated);
                cmpct_free(allocated2.cast());
                return;
            }

            cmpct_free(allocated);
            let allocated3 = cmpct_alloc(size);
            // To avoid churn and fragmentation we would want to get the newly freed
            // memory back again when we allocate the same size shortly after.
            assert_eq!(allocated3, allocated);
            cmpct_free(allocated2.cast());
            cmpct_free(allocated3);
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is not held and the heap is initialized.
    unsafe fn cmpct_test_get_back_newly_freed() {
        // SAFETY: Calls `cmpct_test_get_back_newly_freed_helper`.
        unsafe {
            let mut increment = 16usize;
            let mut i = 128usize;
            while i <= 0x8000000 {
                let mut j = i;
                while j < i * 2 {
                    cmpct_test_get_back_newly_freed_helper(i - 8);
                    cmpct_test_get_back_newly_freed_helper(i);
                    cmpct_test_get_back_newly_freed_helper(i + 1);
                    j += increment;
                }
                i *= 2;
                increment *= 2;
            }
            for k in 1024..=2048 {
                cmpct_test_get_back_newly_freed_helper(k);
            }
        }
    }

    /// # Safety
    ///
    /// The caller must ensure that `TheHeapLock` is not held and the heap is initialized.
    unsafe fn cmpct_test_return_to_os() {
        // SAFETY: Test helper exercises `cmpct_alloc` and `cmpct_free`.
        unsafe {
            let remaining = cmpct_heap_remaining();
            // This goes in a new OS allocation since the trim above removed any free
            // area big enough to contain it.
            let a = cmpct_alloc(5000);
            let b = cmpct_alloc(2500);
            cmpct_free(a);
            cmpct_free(b);
            // If things work as expected the new allocation is at the start of an OS
            // allocation.  There's just one sentinel and one header to the left of it.
            // It that's not the case then the allocation was met from some space in
            // the middle of an OS allocation, and our test won't work as expected, so
            // bail out.
            if (a.addr() & (page::SIZE - 1)) != size_of::<Header>() * 2 {
                return;
            }
            // No trim needed when the entire OS allocation is free.
            assert_eq!(remaining, cmpct_heap_remaining());
        }
    }

    /// Runs internal `cmpctmalloc` self-tests when `heap_enable_tests` is enabled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the heap has been initialized and `TheHeapLock`
    /// is not held on the current thread.
    pub unsafe fn cmpct_test() {
        let mut rng_state: u32 = 1;
        let mut rand = || -> u32 {
            rng_state = rng_state.wrapping_mul(1103515245).wrapping_add(12345);
            (rng_state >> 16) & 0x7fff
        };
        cmpct_test_buckets();
        // SAFETY: Caller guarantees the heap is initialized and `TheHeapLock` is not held.
        unsafe {
            cmpct_test_get_back_newly_freed();
            cmpct_test_return_to_os();
            cmpct_dump(CmpctDumpOptions::NONE);
        }
        let mut ptr: [*mut c_void; 16] = [ptr::null_mut(); 16];

        // SAFETY: Exercises `cmpct_alloc`, `cmpct_free`, and `cmpct_dump` with valid sizes and
        // pointers while `TheHeapLock` is not held.
        unsafe {
            ptr[0] = cmpct_alloc(8);
            ptr[1] = cmpct_alloc(32);
            ptr[2] = cmpct_alloc(7);
            ptr[3] = cmpct_alloc(0);
            ptr[4] = cmpct_alloc(98713);
            ptr[5] = cmpct_alloc(16);

            cmpct_free(ptr[5]);
            cmpct_free(ptr[1]);
            cmpct_free(ptr[3]);
            cmpct_free(ptr[0]);
            cmpct_free(ptr[4]);
            cmpct_free(ptr[2]);

            cmpct_dump(CmpctDumpOptions::NONE);
        }

        ptr.fill(ptr::null_mut());

        for i in 0..32768 {
            let index = (rand() as usize) % 16;

            if (i % (16 * 1024)) == 0 {
                kprintln!("pass {}", i);
            }

            // printf("index 0x%x\n", index);
            if !ptr[index].is_null() {
                // printf("freeing ptr[0x%x] = %p\n", index, ptr[index]);
                // SAFETY: `ptr[index]` is a valid non-null allocation returned by `cmpct_memalign`.
                unsafe { cmpct_free(ptr[index]) };
                ptr[index] = ptr::null_mut();
            }
            let align = 1usize << (rand() % 8);
            // SAFETY: `align` is a power of two and the heap is initialized.
            ptr[index] = unsafe { cmpct_memalign(align, (rand() as usize) % 32768) };
            // printf("ptr[0x%x] = %p, align 0x%x\n", index, ptr[index], align);

            debug_assert_eq!(ptr[index].addr() % align, 0);
            // cmpct_dump(CmpctDumpOptions::NONE);
        }

        for slot in ptr {
            if !slot.is_null() {
                // SAFETY: `slot` is a valid non-null allocation returned by `cmpct_memalign`.
                unsafe { cmpct_free(slot) };
            }
        }

        // SAFETY: `TheHeapLock` is not held.
        unsafe { cmpct_dump(CmpctDumpOptions::NONE) };
    }
}

#[cfg(not(feature = "heap_enable_tests"))]
mod tests {
    /// Runs internal `cmpctmalloc` self-tests when `heap_enable_tests` is enabled.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the heap has been initialized and `TheHeapLock`
    /// is not held on the current thread.
    pub unsafe fn cmpct_test() {}
}
