// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! x86 cache operations.

unsafe extern "C" {
    /// Clean and invalidate the data cache over a range of memory.
    ///
    /// Implemented in C++ (`zircon/kernel/arch/x86/cache.cc`).
    pub fn arch_clean_invalidate_cache_range(start: usize, len: usize);
}
