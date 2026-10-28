// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! FFI declarations for Kernel Address Sanitizer (KASAN) runtime functions.

#[cfg(sanitize = "address")]
unsafe extern "C" {
    /// Marks `[address, round_down(address + size, ASAN_GRANULARITY))` (or `[address, address +
    /// size)` if `address + size` is already poisoned) as invalid in the KASAN shadow map with
    /// poison byte `value`.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `[address, address + size)` lies within a region backed by
    /// KASAN shadow memory and `value` is a valid KASAN poison magic byte.
    pub fn asan_poison_shadow(address: usize, size: usize, value: u8);

    /// Marks `[round_down(address, ASAN_GRANULARITY), address + size)` as addressable in the KASAN
    /// shadow map.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `[address, address + size)` lies within a region backed by
    /// KASAN shadow memory.
    pub fn asan_unpoison_shadow(address: usize, size: usize);

    /// Returns the KASAN right-redzone byte size to add to a heap allocation of `size` bytes.
    ///
    /// # Safety
    ///
    /// The caller must ensure that `size` does not overflow when rounded up to an ASAN granule
    /// plus the heap right-redzone size.
    pub fn asan_heap_redzone_size(size: usize) -> usize;
}
