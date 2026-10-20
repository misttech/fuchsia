// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! ARM64 cache operations.

unsafe extern "C" {
    /// Clean the data cache over a range of memory.
    ///
    /// Implemented in assembly (`zircon/kernel/arch/arm64/cache-ops.S`).
    pub fn arch_clean_cache_range(start: usize, len: usize);

    /// Clean and invalidate the data cache over a range of memory.
    ///
    /// Implemented in assembly (`zircon/kernel/arch/arm64/cache-ops.S`).
    pub fn arch_clean_invalidate_cache_range(start: usize, len: usize);
}
