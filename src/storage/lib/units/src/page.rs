// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{BlockSize, BlockSizeSpec, GenericBlockSize};
use std::sync::atomic::{AtomicU64, Ordering};

// Cached system page mask (page_size - 1), initialized on first access.
static PAGE_MASK: AtomicU64 = AtomicU64::new(0);

#[cold]
#[inline(never)]
fn initialize_page_mask() -> u64 {
    let page_size = zx::system_get_page_size();
    debug_assert!(page_size.is_power_of_two());
    let mask = (page_size - 1) as u64;
    PAGE_MASK.store(mask, Ordering::Relaxed);
    mask
}

// Returns the system page mask, initializing it if not already cached.
#[inline(always)]
fn page_mask() -> u64 {
    let mask = PAGE_MASK.load(Ordering::Relaxed);
    if mask != 0 { mask } else { initialize_page_mask() }
}

/// [`BlockSizeSpec`] implementation representing the Fuchsia system memory page size.
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PageSizeSpec;

impl BlockSizeSpec for PageSizeSpec {
    #[inline(always)]
    fn mask(self) -> u64 {
        page_mask()
    }
}

/// The system memory page size.
pub const PAGE_SIZE: GenericBlockSize<PageSizeSpec> = GenericBlockSize(PageSizeSpec);

impl From<GenericBlockSize<PageSizeSpec>> for BlockSize {
    fn from(value: GenericBlockSize<PageSizeSpec>) -> Self {
        BlockSize::from_u64(value.get()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_page_size() {
        let expected = zx::system_get_page_size() as u64;
        assert_eq!(PAGE_SIZE.get(), expected);
        assert_eq!(PAGE_SIZE.mask(), expected - 1);
        assert_eq!(BlockSize::from(PAGE_SIZE).get(), expected);
        assert!(PAGE_SIZE.is_aligned(expected));
        assert!(!PAGE_SIZE.is_aligned(expected - 1));
    }
}
