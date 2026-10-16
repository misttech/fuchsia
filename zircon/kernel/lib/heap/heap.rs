// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use heap_bindings as bindings;

/// Tracks an allocation of the given `size` in the heap profiler. This can be used to make visible
/// allocations that, while not from the general kernel heap, are notionally heap-like or from
/// separate slab / other non-general heap allocators.
/// The return value is a cookie that, along with `size` can be passed to `profile_track_free` to
/// indicate deallocation.
pub fn profile_track_alloc(size: usize) -> u32 {
    // SAFETY: There are no obligations.
    unsafe { bindings::cpp_profile_track_alloc(size) }
}

pub fn profile_track_free(cookie: u32, size: usize) {
    // SAFETY: There are no obligations.
    unsafe { bindings::cpp_profile_track_free(cookie, size) }
}

/// Gets stats about the heap.
/// Returns `(total_bytes, free_bytes)`, where `total_bytes` is the total size of the heap (the sum
/// of all pages allocated from the PMM) and `free_bytes` is the free portion.
pub fn get_info() -> (usize, usize) {
    let mut total_bytes = 0;
    let mut free_bytes = 0;
    // SAFETY: Passing valid, aligned, non-aliasing mutable pointers to local stack variables.
    unsafe { bindings::cpp_heap_get_info(&mut total_bytes, &mut free_bytes) };
    (total_bytes, free_bytes)
}
