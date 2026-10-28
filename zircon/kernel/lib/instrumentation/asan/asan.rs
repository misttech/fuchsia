// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel Address Sanitizer (KASAN) poison constants, shadow FFI bindings, and heap quarantine.

mod asan_ffi;

use core::mem::{align_of, size_of};
use core::ptr;

#[cfg(sanitize = "address")]
pub use asan_ffi::{asan_heap_redzone_size, asan_poison_shadow, asan_unpoison_shadow};

/// KASAN poison byte for C++ `operator new[]` array cookies (`kAsanArrayCookie`).
pub const ASAN_ARRAY_COOKIE: u8 = 0xac;
/// KASAN poison byte for internal heap bookkeeping metadata (`kAsanInternalHeapMagic`).
pub const ASAN_INTERNAL_HEAP_MAGIC: u8 = 0xf0;
/// KASAN poison byte for global variable redzones (`kAsanGlobalRedzoneMagic`).
pub const ASAN_GLOBAL_REDZONE_MAGIC: u8 = 0xf9;
/// KASAN poison byte for heap allocation headers and redzones (`kAsanHeapLeftRedzoneMagic`).
pub const ASAN_HEAP_LEFT_REDZONE_MAGIC: u8 = 0xfa;
/// KASAN poison byte for free PMM pages (`kAsanPmmFreeMagic`).
pub const ASAN_PMM_FREE_MAGIC: u8 = 0xfb;
/// KASAN poison byte for quarantined objects (`kAsanQuarantineMagic`).
pub const ASAN_QUARANTINE_MAGIC: u8 = 0xfc;
/// KASAN poison byte for freed heap payloads (`kAsanHeapFreeMagic`).
pub const ASAN_HEAP_FREE_MAGIC: u8 = 0xfd;
/// KASAN poison byte for allocation headers (`kAsanAllocHeader`).
pub const ASAN_ALLOC_HEADER: u8 = 0xff;

/// Fixed capacity of [`Quarantine`] (`asan::Quarantine::kQuarantineElements`).
pub const QUARANTINE_ELEMENTS: usize = 65536;

/// Fixed-size FIFO ring buffer delaying reuse of freed heap allocations under KASAN.
#[repr(C)]
pub struct Quarantine<T> {
    queue: [*mut T; QUARANTINE_ELEMENTS],
    pos: usize,
}

zr::static_assert!(size_of::<Quarantine<u8>>() == 524296);
zr::static_assert!(align_of::<Quarantine<u8>>() == 8);

impl<T> Quarantine<T> {
    /// Creates an empty `Quarantine`.
    ///
    /// Note: `Quarantine` is 512 KiB and must only be constructed in `static` initializers, never
    /// on an 8 KiB kernel thread stack.
    pub const fn new() -> Self {
        Self { queue: [ptr::null_mut(); QUARANTINE_ELEMENTS], pos: 0 }
    }

    /// Pushes `allocation` that was going to be freed into the quarantine, increasing reuse
    /// distance. Returns null if the quarantine was not yet full, or the oldest pushed allocation
    /// if a slot was evicted (which the caller is then responsible for freeing).
    #[cfg_attr(sanitize = "address", sanitize(address = "off"))]
    #[inline]
    pub fn push(&mut self, allocation: *mut T) -> *mut T {
        let idx = self.pos % QUARANTINE_ELEMENTS;
        self.pos = self.pos.wrapping_add(1);
        core::mem::replace(&mut self.queue[idx], allocation)
    }
}
