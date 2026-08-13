// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use attribution_bindings as bindings;
use core::mem::MaybeUninit;

pub use bindings::vm_AttributionCounts as AttributionCounts;

/// Returns an `AttributionCounts` initialized to zero.
pub fn zero() -> AttributionCounts {
    let mut counts = MaybeUninit::uninit();
    // SAFETY: `cpp_attribution_counts_zero` initializes `counts`.
    unsafe {
        bindings::cpp_attribution_counts_zero(counts.as_mut_ptr());
        counts.assume_init()
    }
}

/// Returns the sum of uncompressed and compressed bytes.
pub fn total_bytes(counts: &AttributionCounts) -> u64 {
    (counts.uncompressed_bytes + counts.compressed_bytes) as u64
}
