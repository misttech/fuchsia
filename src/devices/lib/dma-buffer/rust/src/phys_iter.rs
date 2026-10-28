// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physical address and size segment iterator for pinned DMA buffers.

use zx::sys::zx_paddr_t;
use zx::system_get_page_size;

/// An iterator yielding `(zx_paddr_t, usize)` physical address and byte length pairs for a pinned
/// DMA buffer.
///
/// The yielded pairs are suitable for hardware DMA transactions that consume a physical address and
/// byte length. The iterator ensures chunk boundaries are not crossed unless the buffer is backed
/// by a single contiguous chunk (`chunk_list.len() == 1`) or contiguous chunk merging (`merge`) is
/// enabled.
#[derive(Debug, Clone)]
pub struct PhysIter<'a> {
    chunk_list: &'a [zx_paddr_t],
    buf_length: usize,
    max_length: usize,
    chunk_size: usize,
    merge: bool,
    chunk_index: usize,
    offset_in_chunk: usize,
    current: Option<(zx_paddr_t, usize)>,
    iterated_bytes: usize,
}

impl<'a> PhysIter<'a> {
    /// Creates a new `PhysIter` with page-sized chunks, unlimited segment length, and chunk merging
    /// disabled.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_list` is empty, `buf_length == 0`, or
    /// `chunk_list.len() > 1 && vmo_offset >= system_get_page_size() as usize`.
    pub fn new(chunk_list: &'a [zx_paddr_t], vmo_offset: usize, buf_length: usize) -> Self {
        Self::new_with_options(
            chunk_list,
            system_get_page_size() as usize,
            vmo_offset,
            buf_length,
            usize::MAX,
            false,
        )
    }

    /// Creates a new `PhysIter` with explicit chunk size, maximum segment length, and chunk
    /// merging options.
    ///
    /// # Panics
    ///
    /// Panics if `chunk_list` is empty, `buf_length == 0`, `chunk_size` is not a non-zero power
    /// of two, or `chunk_list.len() > 1 && vmo_offset >= chunk_size`.
    pub fn new_with_options(
        chunk_list: &'a [zx_paddr_t],
        chunk_size: usize,
        vmo_offset: usize,
        buf_length: usize,
        max_length: usize,
        merge: bool,
    ) -> Self {
        assert!(!chunk_list.is_empty(), "chunk_list must not be empty");
        assert!(buf_length > 0, "buf_length must be greater than 0");
        assert!(chunk_size.is_power_of_two(), "chunk_size must be a power of 2");
        assert!(
            chunk_list.len() == 1 || vmo_offset < chunk_size,
            "vmo_offset must be less than chunk_size when chunk_list has multiple entries"
        );

        let max_length = if max_length == 0 { usize::MAX } else { max_length };

        let mut iter = Self {
            chunk_list,
            buf_length,
            max_length,
            chunk_size,
            merge,
            chunk_index: 0,
            offset_in_chunk: vmo_offset,
            current: None,
            iterated_bytes: 0,
        };
        let first_addr = chunk_list[0] + vmo_offset;
        let first_size = iter.boundary_truncate();
        iter.current = Some((first_addr, first_size));
        iter
    }

    fn boundary_truncate(&self) -> usize {
        let end_of_buffer = self.buf_length - self.iterated_bytes;
        if self.chunk_list.len() == 1 {
            return self.max_length.min(end_of_buffer);
        }

        let mut end_of_chunk = self.chunk_size - self.offset_in_chunk;

        if self.merge {
            let mut idx = self.chunk_index;
            while end_of_chunk < self.max_length
                && end_of_chunk < end_of_buffer
                && idx + 1 < self.chunk_list.len()
                && self.chunk_list[idx] + self.chunk_size == self.chunk_list[idx + 1]
            {
                end_of_chunk += self.chunk_size;
                idx += 1;
            }
        }

        self.max_length.min(end_of_chunk.min(end_of_buffer))
    }
}

impl Iterator for PhysIter<'_> {
    type Item = (zx_paddr_t, usize);

    fn next(&mut self) -> Option<Self::Item> {
        let (cur_addr, cur_size) = self.current?;
        let item = (cur_addr, cur_size);

        self.iterated_bytes += cur_size;
        if self.iterated_bytes == self.buf_length {
            self.current = None;
            return Some(item);
        }

        let next_addr = if self.chunk_list.len() == 1 {
            cur_addr + cur_size
        } else {
            let next_offset = self.offset_in_chunk + cur_size;
            self.chunk_index += next_offset / self.chunk_size;
            self.offset_in_chunk = next_offset % self.chunk_size;
            self.chunk_list[self.chunk_index] + self.offset_in_chunk
        };

        let next_size = self.boundary_truncate();
        self.current = Some((next_addr, next_size));
        Some(item)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    const PAGE_SIZE: usize = 4096;
    const HALF_PAGE: usize = 2048;

    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (2 * PAGE_SIZE, PAGE_SIZE)];
        "simple_page_boundary"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        3 * PAGE_SIZE,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, PAGE_SIZE)];
        "non_contiguous_pages"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        PAGE_SIZE - 5,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(2 * PAGE_SIZE - 5, 5), (2 * PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE)];
        "partial_first_page_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        PAGE_SIZE - 5,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(2 * PAGE_SIZE - 5, 5), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, PAGE_SIZE)];
        "partial_first_page_non_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (2 * PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, 5)];
        "partial_last_page_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        false,
        &[(PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, 5)];
        "partial_last_page_non_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        HALF_PAGE,
        false,
        &[
            (2 * HALF_PAGE, HALF_PAGE),
            (3 * HALF_PAGE, HALF_PAGE),
            (4 * HALF_PAGE, HALF_PAGE),
            (5 * HALF_PAGE, HALF_PAGE),
        ];
        "sub_chunk_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE],
        PAGE_SIZE,
        0,
        1 << 20,
        usize::MAX,
        false,
        &[(PAGE_SIZE, 1 << 20)];
        "zx_bti_contiguous_large_buffer"
    )]
    #[test_case(
        &[HALF_PAGE, 2 * HALF_PAGE, 3 * HALF_PAGE],
        HALF_PAGE,
        0,
        3 * HALF_PAGE,
        usize::MAX,
        false,
        &[(HALF_PAGE, HALF_PAGE), (2 * HALF_PAGE, HALF_PAGE), (3 * HALF_PAGE, HALF_PAGE)];
        "zx_bti_compress_contiguous"
    )]
    #[test_case(
        &[HALF_PAGE, 3 * HALF_PAGE, 5 * HALF_PAGE],
        HALF_PAGE,
        0,
        3 * HALF_PAGE,
        usize::MAX,
        false,
        &[(HALF_PAGE, HALF_PAGE), (3 * HALF_PAGE, HALF_PAGE), (5 * HALF_PAGE, HALF_PAGE)];
        "zx_bti_compress_non_contiguous"
    )]
    #[test_case(
        &[PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        HALF_PAGE,
        false,
        &[
            (2 * HALF_PAGE, HALF_PAGE),
            (3 * HALF_PAGE, HALF_PAGE),
            (4 * HALF_PAGE, HALF_PAGE),
            (5 * HALF_PAGE, HALF_PAGE),
        ];
        "zx_bti_contiguous_sub_chunk_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        HALF_PAGE,
        false,
        &[
            (2 * HALF_PAGE, HALF_PAGE),
            (3 * HALF_PAGE, HALF_PAGE),
            (6 * HALF_PAGE, HALF_PAGE),
            (7 * HALF_PAGE, HALF_PAGE),
        ];
        "zx_bti_non_contiguous_sub_chunk_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE,
        usize::MAX,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE)];
        "simple_page_boundary_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        3 * PAGE_SIZE,
        usize::MAX,
        true,
        &[(PAGE_SIZE, PAGE_SIZE), (3 * PAGE_SIZE, PAGE_SIZE), (5 * PAGE_SIZE, PAGE_SIZE)];
        "non_contiguous_pages_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 4 * PAGE_SIZE, 5 * PAGE_SIZE, 6 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        5 * PAGE_SIZE,
        usize::MAX,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE), (4 * PAGE_SIZE, 3 * PAGE_SIZE)];
        "mixed_contiguous_and_non_contiguous_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        PAGE_SIZE - 5,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        true,
        &[(2 * PAGE_SIZE - 5, 2 * PAGE_SIZE + 5)];
        "partial_first_page_contiguous_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        2 * PAGE_SIZE + 5,
        usize::MAX,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE + 5)];
        "partial_last_page_contiguous_merge"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE, 4 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        4 * PAGE_SIZE,
        2 * PAGE_SIZE,
        true,
        &[(PAGE_SIZE, 2 * PAGE_SIZE), (3 * PAGE_SIZE, 2 * PAGE_SIZE)];
        "merge_respects_chunk_aligned_max_length"
    )]
    #[test_case(
        &[PAGE_SIZE, 2 * PAGE_SIZE, 3 * PAGE_SIZE, 5 * PAGE_SIZE],
        PAGE_SIZE,
        0,
        4 * PAGE_SIZE,
        PAGE_SIZE + HALF_PAGE,
        true,
        &[
            (PAGE_SIZE, PAGE_SIZE + HALF_PAGE),
            (2 * PAGE_SIZE + HALF_PAGE, PAGE_SIZE + HALF_PAGE),
            (5 * PAGE_SIZE, PAGE_SIZE),
        ];
        "merge_respects_unaligned_max_length"
    )]
    #[test_case(
        &[HALF_PAGE, 2 * HALF_PAGE, 3 * HALF_PAGE],
        HALF_PAGE,
        0,
        3 * HALF_PAGE,
        usize::MAX,
        true,
        &[(HALF_PAGE, 3 * HALF_PAGE)];
        "zx_bti_compress_contiguous_merge"
    )]
    #[test_case(
        &[0x22_0000, 0x35_0000],
        0x10_0000,
        0xf_8000,
        0x1_0000,
        0x1_0000,
        true,
        &[(0x31_8000, 0x8000), (0x35_0000, 0x8000)];
        "page_aligned_large_chunks_non_contiguous"
    )]
    #[test_case(
        &[0x22_0000, 0x32_0000],
        0x10_0000,
        0xf_8000,
        0x1_0000,
        0x1_0000,
        true,
        &[(0x31_8000, 0x1_0000)];
        "page_aligned_large_chunks_contiguous_merge"
    )]
    #[test_case(
        &[0x22_0000, 0x35_0000],
        0x10_0000,
        0xd_8000,
        0x3_0000,
        0x1_0000,
        true,
        &[(0x2f_8000, 0x1_0000), (0x30_8000, 0x1_0000), (0x31_8000, 0x8000), (0x35_0000, 0x8000)];
        "page_aligned_large_chunks_multi_step_across_boundary"
    )]
    fn test_phys_iter(
        chunk_list: &[zx_paddr_t],
        chunk_size: usize,
        vmo_offset: usize,
        buf_length: usize,
        max_length: usize,
        merge: bool,
        expected: &[(zx_paddr_t, usize)],
    ) {
        let iter = PhysIter::new_with_options(
            chunk_list, chunk_size, vmo_offset, buf_length, max_length, merge,
        );
        let actual: Vec<_> = iter.collect();
        assert_eq!(actual, expected);
    }
}
