// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::BLOCK_SIZE;
use std::ops::Range;

const TYPE_MASK: u64 = 0xc0000000_00000000;
const REGULAR: u64 = 0x00000000_00000000;
const SPARSE: u64 = 0x80000000_00000000;

/// Represents a logical extent and its optional physical device starting offset.
/// Both `logical_range` boundaries and `device_offset` must always be a multiple of
/// `BLOCK_SIZE` (4096 bytes). The physical device range length is identical to `logical_range`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extent {
    pub logical_range: Range<u64>,
    pub device_offset: Option<u64>,
}

impl Extent {
    /// Returns true if this extent is sparse (unbacked by physical storage).
    pub fn is_sparse(&self) -> bool {
        self.device_offset.is_none()
    }

    /// Returns the logical length of this extent in bytes.
    pub fn len(&self) -> u64 {
        self.logical_range.end - self.logical_range.start
    }

    /// Creates a new `Extent`.
    ///
    /// # Panics
    ///
    /// Panics if `logical_range.start`, `logical_range.end`, or `device_offset`
    /// (when `Some`) is not a multiple of `BLOCK_SIZE` (4096 bytes), or if
    /// `logical_range.start > logical_range.end`.
    pub fn new(logical_range: Range<u64>, device_offset: Option<u64>) -> Self {
        assert!(
            logical_range.start % BLOCK_SIZE == 0 && logical_range.end % BLOCK_SIZE == 0,
            "logical_range boundaries must be a multiple of BLOCK_SIZE (4096 bytes), \
             got {logical_range:?}"
        );
        assert!(
            logical_range.start <= logical_range.end,
            "logical_range.start must be <= logical_range.end, got {logical_range:?}"
        );
        if let Some(dev_offset) = device_offset {
            assert!(
                dev_offset % BLOCK_SIZE == 0,
                "device_offset must be a multiple of BLOCK_SIZE (4096 bytes), got {dev_offset}"
            );
        }
        Self { logical_range, device_offset }
    }
}

/// Compact in-memory representation of a single extent boundary (16 bytes).
/// By storing the ending logical offset (`end_logical_offset`), the start of extent `i`
/// is `0` for `i == 0` or `entries[i - 1].end_logical_offset` for `i > 0`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ExtentEntry {
    end_logical_offset: u64,
    device_offset: u64, // u64::MAX sentinel indicates a SPARSE (unbacked) extent.
}

impl ExtentEntry {
    const SPARSE_DEVICE_OFFSET: u64 = u64::MAX;

    fn is_sparse(&self) -> bool {
        self.device_offset == Self::SPARSE_DEVICE_OFFSET
    }
}

fn encode_regular(length_blocks: u32, target_block: u32) -> u64 {
    REGULAR | ((length_blocks as u64 & 0x3fff_ffff) << 32) | (target_block as u64)
}

fn encode_sparse(length_blocks: u64) -> u64 {
    SPARSE | (length_blocks & !TYPE_MASK)
}

/// An iterator over a subset of `Extent`s in an `Extents` container.
#[derive(Debug, Clone)]
pub struct ExtentsIterator<'a> {
    extents: &'a Extents,
    index: usize,
}

impl<'a> Iterator for ExtentsIterator<'a> {
    type Item = Extent;

    fn next(&mut self) -> Option<Self::Item> {
        if self.index < self.extents.entries.len() {
            let res = self.extents.entry_to_result(self.index);
            self.index += 1;
            Some(res)
        } else {
            None
        }
    }
}

/// Container for active mappings associated with a session.
/// Extents are stored in compact form (`ExtentEntry`, 16 bytes) in a boxed slice
/// sorted by ascending `end_logical_offset` to support clean O(log N) binary search lookups.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Extents {
    entries: Box<[ExtentEntry]>,
}

impl Extents {
    /// Encodes a slice of `Extent`s into 64-bit mapping descriptors.
    ///
    /// # Panics
    ///
    /// Panics if any `Extent` in `extents` has a `logical_range` or `device_offset`
    /// that is not a multiple of `BLOCK_SIZE` (4096 bytes).
    pub fn encode_extents(extents: &[Extent]) -> Vec<u64> {
        extents
            .iter()
            .map(|extent| {
                assert!(
                    extent.logical_range.start % BLOCK_SIZE == 0
                        && extent.logical_range.end % BLOCK_SIZE == 0,
                    "Extent logical_range must be a multiple of BLOCK_SIZE (4096 bytes), \
                     got {:?}",
                    extent.logical_range
                );
                match extent.device_offset {
                    Some(dev_offset) => {
                        assert!(
                            dev_offset % BLOCK_SIZE == 0,
                            "Extent device_offset must be a multiple of BLOCK_SIZE (4096 bytes), \
                             got {dev_offset}"
                        );
                        let length_blocks: u32 = (extent.len() / BLOCK_SIZE)
                            .try_into()
                            .expect("Extent length in blocks exceeds u32::MAX");
                        assert!(
                            length_blocks <= 0x3fff_ffff,
                            "Extent block count {length_blocks} exceeds 30-bit regular extent limit"
                        );
                        let target_block: u32 = (dev_offset / BLOCK_SIZE)
                            .try_into()
                            .expect("Extent device_offset block index exceeds u32::MAX");
                        encode_regular(length_blocks, target_block)
                    }
                    None => {
                        let length_blocks = extent.len() / BLOCK_SIZE;
                        assert!(
                            length_blocks <= !TYPE_MASK,
                            "Sparse extent block count {length_blocks} exceeds 62-bit limit"
                        );
                        encode_sparse(length_blocks)
                    }
                }
            })
            .collect()
    }

    /// Decodes a sequence of 64-bit mapping descriptors into a compact `Extents` container.
    /// Returns `None` if an unknown mapping descriptor type is encountered.
    pub fn from_encoded(encoded: &[u64]) -> Option<Self> {
        let mut entries = Vec::with_capacity(encoded.len());
        let mut current_logical_offset = 0;

        for &val in encoded {
            let kind = val & TYPE_MASK;
            if kind == REGULAR {
                let length_blocks = ((val & !TYPE_MASK) >> 32) as u64;
                let target_block = (val & 0xffff_ffff) as u64;
                let length_bytes = length_blocks * BLOCK_SIZE;
                current_logical_offset += length_bytes;
                entries.push(ExtentEntry {
                    end_logical_offset: current_logical_offset,
                    device_offset: target_block * BLOCK_SIZE,
                });
            } else if kind == SPARSE {
                let length_blocks = (val & !TYPE_MASK) as u64;
                let length_bytes = length_blocks * BLOCK_SIZE;
                current_logical_offset += length_bytes;
                entries.push(ExtentEntry {
                    end_logical_offset: current_logical_offset,
                    device_offset: ExtentEntry::SPARSE_DEVICE_OFFSET,
                });
            } else {
                return None;
            }
        }

        Some(Self { entries: entries.into_boxed_slice() })
    }

    /// Returns an iterator over all extents whose logical range ends after `start_offset`,
    /// jumping directly to the first overlapping extent in O(log N) time via binary search.
    pub fn iter_extents(&self, start_offset: u64) -> ExtentsIterator<'_> {
        let index = self.entries.partition_point(|e| e.end_logical_offset <= start_offset);
        ExtentsIterator { extents: self, index }
    }

    /// Maps a logical byte offset to the corresponding `Extent` in O(log N) time
    /// using binary search, translating logical and physical ranges to start at `offset`.
    ///
    /// # Panics
    ///
    /// Panics if `offset` is not a multiple of `BLOCK_SIZE` (4096 bytes).
    pub fn map(&self, offset: u64) -> Option<Extent> {
        assert!(
            offset % BLOCK_SIZE == 0,
            "offset must be a multiple of BLOCK_SIZE (4096 bytes), got {offset}"
        );
        let idx = self.entries.partition_point(|e| e.end_logical_offset <= offset);
        if idx >= self.entries.len() {
            return None;
        }

        let entry = &self.entries[idx];
        let start_logical = self.entry_start_offset(idx);
        let end_logical = entry.end_logical_offset;

        let offset_within = offset - start_logical;
        let device_offset =
            if entry.is_sparse() { None } else { Some(entry.device_offset + offset_within) };

        Some(Extent { logical_range: offset..end_logical, device_offset })
    }

    /// Returns all mappings as full `Extent` structs.
    pub fn mappings(&self) -> Vec<Extent> {
        (0..self.entries.len()).map(|i| self.entry_to_result(i)).collect()
    }

    fn entry_start_offset(&self, idx: usize) -> u64 {
        if idx == 0 { 0 } else { self.entries[idx - 1].end_logical_offset }
    }

    fn entry_to_result(&self, idx: usize) -> Extent {
        let entry = &self.entries[idx];
        let start_logical = self.entry_start_offset(idx);
        let end_logical = entry.end_logical_offset;
        let device_offset = if entry.is_sparse() { None } else { Some(entry.device_offset) };
        Extent { logical_range: start_logical..end_logical, device_offset }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_decode_regular() {
        let extents = vec![
            Extent::new(0..(4 * BLOCK_SIZE), Some(10 * BLOCK_SIZE)),
            Extent::new((4 * BLOCK_SIZE)..(6 * BLOCK_SIZE), Some(100 * BLOCK_SIZE)),
        ];
        let encoded = Extents::encode_extents(&extents);
        assert_eq!(encoded.len(), 2);

        let extents_container = Extents::from_encoded(&encoded).unwrap();

        let decoded = extents_container.mappings();
        assert_eq!(decoded.len(), 2);

        assert_eq!(decoded[0].logical_range, 0..(4 * BLOCK_SIZE));
        assert_eq!(decoded[0].device_offset, Some(10 * BLOCK_SIZE));

        assert_eq!(decoded[1].logical_range, (4 * BLOCK_SIZE)..(6 * BLOCK_SIZE));
        assert_eq!(decoded[1].device_offset, Some(100 * BLOCK_SIZE));
    }

    #[test]
    fn test_encode_decode_sparse() {
        let extents = vec![
            Extent::new(0..(2 * BLOCK_SIZE), Some(50 * BLOCK_SIZE)),
            Extent::new((2 * BLOCK_SIZE)..(5 * BLOCK_SIZE), None),
            Extent::new((5 * BLOCK_SIZE)..(6 * BLOCK_SIZE), Some(200 * BLOCK_SIZE)),
        ];
        let encoded = Extents::encode_extents(&extents);

        let extents_container = Extents::from_encoded(&encoded).unwrap();

        let decoded = extents_container.mappings();
        assert_eq!(decoded.len(), 3);

        assert_eq!(decoded[0].logical_range, 0..(2 * BLOCK_SIZE));
        assert_eq!(decoded[0].device_offset, Some(50 * BLOCK_SIZE));

        assert_eq!(decoded[1].logical_range, (2 * BLOCK_SIZE)..(5 * BLOCK_SIZE));
        assert_eq!(decoded[1].device_offset, None);

        assert_eq!(decoded[2].logical_range, (5 * BLOCK_SIZE)..(6 * BLOCK_SIZE));
        assert_eq!(decoded[2].device_offset, Some(200 * BLOCK_SIZE));
    }

    #[test]
    fn test_binary_search_map_logical_offset() {
        let extents = vec![
            Extent::new(0..(10 * BLOCK_SIZE), Some(100 * BLOCK_SIZE)),
            Extent::new((10 * BLOCK_SIZE)..(20 * BLOCK_SIZE), Some(200 * BLOCK_SIZE)),
            Extent::new((20 * BLOCK_SIZE)..(30 * BLOCK_SIZE), Some(300 * BLOCK_SIZE)),
        ];
        let encoded = Extents::encode_extents(&extents);
        let extents_container = Extents::from_encoded(&encoded).unwrap();

        let mapped = extents_container.map(0).expect("should map at offset 0");
        assert_eq!(mapped.logical_range, 0..(10 * BLOCK_SIZE));
        assert_eq!(mapped.device_offset, Some(100 * BLOCK_SIZE));

        let mapped_mid = extents_container
            .map(12 * BLOCK_SIZE)
            .expect("should map inside second extent via binary search");
        assert_eq!(mapped_mid.logical_range, (12 * BLOCK_SIZE)..(20 * BLOCK_SIZE));
        assert_eq!(mapped_mid.device_offset, Some(202 * BLOCK_SIZE));
    }

    #[test]
    fn test_map_out_of_bounds() {
        let extents = vec![Extent::new(0..(2 * BLOCK_SIZE), Some(10 * BLOCK_SIZE))];
        let encoded = Extents::encode_extents(&extents);
        let extents_container = Extents::from_encoded(&encoded).unwrap();

        assert!(extents_container.map(2 * BLOCK_SIZE).is_none());
        assert!(extents_container.map(100 * BLOCK_SIZE).is_none());
    }

    #[test]
    fn test_binary_search_iter_extents() {
        let extents = vec![
            Extent::new(0..(2 * BLOCK_SIZE), Some(10 * BLOCK_SIZE)),
            Extent::new((2 * BLOCK_SIZE)..(4 * BLOCK_SIZE), Some(20 * BLOCK_SIZE)),
            Extent::new((4 * BLOCK_SIZE)..(6 * BLOCK_SIZE), Some(30 * BLOCK_SIZE)),
        ];
        let encoded = Extents::encode_extents(&extents);
        let extents_container = Extents::from_encoded(&encoded).unwrap();

        let results: Vec<_> = extents_container.iter_extents(3 * BLOCK_SIZE).collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].logical_range, (2 * BLOCK_SIZE)..(4 * BLOCK_SIZE));
        assert_eq!(results[1].logical_range, (4 * BLOCK_SIZE)..(6 * BLOCK_SIZE));
    }

    #[test]
    fn test_exact_boundary_queries() {
        let extents = vec![
            Extent::new(0..(10 * BLOCK_SIZE), Some(100 * BLOCK_SIZE)),
            Extent::new((10 * BLOCK_SIZE)..(20 * BLOCK_SIZE), Some(200 * BLOCK_SIZE)),
        ];
        let encoded = Extents::encode_extents(&extents);
        let extents_container = Extents::from_encoded(&encoded).unwrap();

        let mapped = extents_container.map(10 * BLOCK_SIZE).expect("should map at exact boundary");
        assert_eq!(mapped.logical_range, (10 * BLOCK_SIZE)..(20 * BLOCK_SIZE));
        assert_eq!(mapped.device_offset, Some(200 * BLOCK_SIZE));

        let results: Vec<_> = extents_container.iter_extents(10 * BLOCK_SIZE).collect();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].logical_range, (10 * BLOCK_SIZE)..(20 * BLOCK_SIZE));
    }

    #[test]
    #[should_panic(expected = "multiple of BLOCK_SIZE")]
    fn test_extent_new_unaligned_logical_start_panics() {
        Extent::new(1..(2 * BLOCK_SIZE), Some(10 * BLOCK_SIZE));
    }

    #[test]
    #[should_panic(expected = "multiple of BLOCK_SIZE")]
    fn test_extent_new_unaligned_logical_end_panics() {
        Extent::new(0..(2 * BLOCK_SIZE + 1), Some(10 * BLOCK_SIZE));
    }

    #[test]
    #[should_panic(expected = "multiple of BLOCK_SIZE")]
    fn test_extent_new_unaligned_device_offset_panics() {
        Extent::new(0..(2 * BLOCK_SIZE), Some(10 * BLOCK_SIZE + 500));
    }

    #[test]
    #[should_panic(expected = "multiple of BLOCK_SIZE")]
    fn test_map_unaligned_offset_panics() {
        Extents::default().map(500);
    }

    #[test]
    fn test_iter_extents_unaligned_start_offset() {
        let extents = vec![
            Extent::new(0..(10 * BLOCK_SIZE), Some(100 * BLOCK_SIZE)),
            Extent::new((10 * BLOCK_SIZE)..(20 * BLOCK_SIZE), Some(200 * BLOCK_SIZE)),
        ];
        let encoded = Extents::encode_extents(&extents);
        let extents_container = Extents::from_encoded(&encoded).unwrap();
        let results: Vec<_> = extents_container.iter_extents(500).collect();
        assert_eq!(results.len(), 2);
        assert_eq!(results[0].logical_range, 0..(10 * BLOCK_SIZE));
    }

    #[test]
    #[should_panic(expected = "exceeds u32::MAX")]
    fn test_encode_extents_device_offset_overflow_panics() {
        let extents = vec![Extent::new(0..BLOCK_SIZE, Some((u32::MAX as u64 + 1) * BLOCK_SIZE))];
        Extents::encode_extents(&extents);
    }

    #[test]
    #[should_panic(expected = "exceeds 30-bit regular extent limit")]
    fn test_encode_extents_regular_length_overflow_panics() {
        let extents =
            vec![Extent::new(0..((0x3fff_ffff_u64 + 1) * BLOCK_SIZE), Some(10 * BLOCK_SIZE))];
        Extents::encode_extents(&extents);
    }

    #[test]
    fn test_from_encoded_unknown_kind_returns_none() {
        let unknown_descriptor = 0x40000000_00000000;
        assert!(Extents::from_encoded(&[unknown_descriptor]).is_none());
    }
}
