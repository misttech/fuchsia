// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::marker::PhantomData;

use super::error::{ParseError, ValidateError};
use super::parser::{Array, PolicyCursor};
use super::traits::{Parse, PolicyId, Serialize, Validate};
use super::{NewPolicy, TypeId};

pub use selinux_policy_derive::{Parse, Serialize, Validate};

type MapNode = u64;

/// Number of bits represented by a single node in the extensible bitmap.
pub const MAP_NODE_BITS: u32 = MapNode::BITS;

/// Binary map item in the extensible bitmap.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize)]
pub(super) struct BinaryMapItem {
    start_bit: u32,
    map: MapNode,
}

impl Validate for BinaryMapItem {
    fn validate(&self, _policy: &NewPolicy) -> Result<(), ValidateError> {
        if self.map == 0 {
            return Err(ValidateError::InvalidExtensibleBitmapItem);
        }
        if self.start_bit & (MAP_NODE_BITS - 1) != 0 {
            return Err(ValidateError::MisalignedExtensibleBitmapItemStartBit {
                found_start_bit: self.start_bit,
                found_size: MAP_NODE_BITS,
            });
        }
        Ok(())
    }
}

/// Extensible bitmap for storing potentially large, sparse bitmaps.
///
/// This representation allows memory-efficient storage of sparse bitmaps while
/// providing efficient bit lookup, iteration, and round-trip serialization.
///
/// Under the hood, this is stored as a sorted [`Array`] of [`BinaryMapItem`]s.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize)]
pub struct ExtensibleBitmap {
    map_item_size_bits: u32,
    high_bit: u32,
    items: Array<BinaryMapItem>,
}

impl ExtensibleBitmap {
    /// Returns `true` if the `index`'th bit in this bitmap is a 1-bit.
    pub fn is_set(&self, index: u32) -> bool {
        if index >= self.high_bit {
            return false;
        }
        let start_bit = index & !(MAP_NODE_BITS - 1);
        let bit_index = index & (MAP_NODE_BITS - 1);

        match self.items.binary_search_by_key(&start_bit, |item| item.start_bit) {
            Ok(idx) => {
                let map = self.items[idx].map;
                (map & ((1 as MapNode) << bit_index)) != 0
            }
            Err(_) => false,
        }
    }

    /// Returns `true` if this bitmap is empty (no bits set).
    pub fn is_empty(&self) -> bool {
        self.items.is_empty()
    }

    /// Returns an iterator that yields the indices of this bitmap's set bits.
    pub(super) fn indices_of_set_bits(&self) -> BitIter<'_> {
        let mut items = self.items.iter();
        let current_item = items.next().cloned();
        BitIter { items, current_item }
    }
}

impl Validate for ExtensibleBitmap {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        let map_item_size_bits = self.map_item_size_bits;
        let high_bit = self.high_bit;
        let count = self.items.len() as u32;

        if map_item_size_bits != MAP_NODE_BITS {
            return Err(ValidateError::InvalidExtensibleBitmapItemSize {
                found_size: map_item_size_bits,
            });
        }
        if high_bit & (MAP_NODE_BITS - 1) != 0 {
            return Err(ValidateError::MisalignedExtensibleBitmapHighBit {
                found_size: MAP_NODE_BITS,
                found_high_bit: high_bit,
            });
        }
        if count * MAP_NODE_BITS > high_bit {
            return Err(ValidateError::InvalidExtensibleBitmapHighBit {
                found_size: MAP_NODE_BITS,
                found_high_bit: high_bit,
                found_count: count,
            });
        }
        if high_bit != 0 && count == 0 {
            return Err(ValidateError::ExtensibleBitmapNonZeroHighBitAndZeroCount);
        }

        let mut min_start = 0;
        for item in self.items.iter() {
            item.validate(policy)?;

            let item_start_bit = item.start_bit;
            if item_start_bit < min_start {
                return Err(ValidateError::OutOfOrderExtensibleBitmapItems {
                    found_start_bit: item_start_bit,
                    min_start,
                });
            }
            min_start = item_start_bit + MAP_NODE_BITS;
        }

        if min_start > high_bit {
            return Err(ValidateError::ExtensibleBitmapItemOverflow {
                found_items_end: min_start,
                found_high_bit: high_bit,
            });
        }

        Ok(())
    }
}

/// Iterator over the indices of bits set in [`ExtensibleBitmap`].
pub struct BitIter<'a> {
    items: std::slice::Iter<'a, BinaryMapItem>,
    current_item: Option<BinaryMapItem>,
}

impl<'a> Iterator for BitIter<'a> {
    type Item = u32;

    fn next(&mut self) -> Option<Self::Item> {
        let item = self.current_item.as_mut()?;
        let low_bit_index = item.map.trailing_zeros();
        debug_assert!(low_bit_index < MAP_NODE_BITS);
        let low_bit_mask = (1 as MapNode) << low_bit_index;
        item.map &= !low_bit_mask;
        let result = item.start_bit + low_bit_index;
        if item.map == 0 {
            self.current_item = self.items.next().cloned();
        }
        Some(result)
    }
}

/// Type-safe wrapper around [`ExtensibleBitmap`] for holding a set of strongly-typed IDs.
///
/// If `WITH_ID_ZERO` is `false` (default), the zeroth bit of the bitmap corresponds to
/// identifier value one. If `true` (including a slot for ID 0), the first bit (index 1) corresponds to identifier value one.
#[derive(Debug, Clone, PartialEq, Eq, Parse, Serialize)]
pub struct IdSet<T: PolicyId, const WITH_ID_ZERO: bool = false> {
    bitmap: ExtensibleBitmap,
    _phantom: PhantomData<T>,
}

impl<T: PolicyId, const WITH_ID_ZERO: bool> IdSet<T, WITH_ID_ZERO> {
    /// Returns `true` if the set contains the specified ID.
    pub fn contains(&self, id: T) -> bool {
        self.bitmap.is_set(Self::id_to_index(id))
    }

    /// Returns `true` if this set is empty.
    pub fn is_empty(&self) -> bool {
        self.bitmap.is_empty()
    }

    /// Returns an iterator over the IDs in this set.
    pub fn iter(&self) -> impl Iterator<Item = T> + '_ {
        self.bitmap
            .indices_of_set_bits()
            .map(|index| Self::index_to_id(index).expect("bitmap index must be valid ID"))
    }

    /// Converts a logical ID to a physical bitmap index.
    fn id_to_index(id: T) -> u32 {
        if WITH_ID_ZERO { id.as_u32() } else { id.as_u32() - 1 }
    }

    /// Converts a physical bitmap index to a logical ID.
    fn index_to_id(index: u32) -> Option<T> {
        let id_val = if WITH_ID_ZERO { index } else { index + 1 };
        T::from_u32(id_val)
    }
}

impl<T: PolicyId, const WITH_ID_ZERO: bool> Validate for IdSet<T, WITH_ID_ZERO> {
    fn validate(&self, policy: &NewPolicy) -> Result<(), ValidateError> {
        self.bitmap.validate(policy)?;
        for index in self.bitmap.indices_of_set_bits() {
            if Self::index_to_id(index).is_none() {
                return Err(ValidateError::InvalidIdSetIndex { index });
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse_bitmap(bytes: &[u8]) -> Result<ExtensibleBitmap, ParseError> {
        let mut cursor = PolicyCursor::new(bytes);
        ExtensibleBitmap::parse(&mut cursor)
    }

    #[test]
    fn test_empty_bitmap() {
        let bytes = [
            64, 0, 0, 0, // map_item_size_bits = 64
            0, 0, 0, 0, // high_bit = 0
            0, 0, 0, 0, // count = 0
        ];
        let bitmap = parse_bitmap(&bytes).unwrap();
        assert!(bitmap.is_empty());
        assert_eq!(bitmap.high_bit, 0);
        assert!(!bitmap.is_set(0));
        assert!(!bitmap.is_set(100));

        let bits: Vec<u32> = bitmap.indices_of_set_bits().collect();
        assert!(bits.is_empty());
    }

    #[test]
    fn test_bitmap_with_bits() {
        let bytes = [
            64, 0, 0, 0, // map_item_size_bits = 64
            128, 0, 0, 0, // high_bit = 128
            2, 0, 0, 0, // count = 2
            // Item 1
            0, 0, 0, 0, // start_bit = 0
            5, 0, 0, 0, 0, 0, 0, 0, // map = 5 (bits 0 and 2 set)
            // Item 2
            64, 0, 0, 0, // start_bit = 64
            2, 0, 0, 0, 0, 0, 0, 0, // map = 2 (bit 65 set)
        ];
        let bitmap = parse_bitmap(&bytes).unwrap();
        assert!(!bitmap.is_empty());
        assert_eq!(bitmap.high_bit, 128);

        assert!(bitmap.is_set(0));
        assert!(!bitmap.is_set(1));
        assert!(bitmap.is_set(2));
        assert!(!bitmap.is_set(3));
        assert!(!bitmap.is_set(64));
        assert!(bitmap.is_set(65));
        assert!(!bitmap.is_set(66));
        assert!(!bitmap.is_set(128)); // Out of bounds

        let bits: Vec<u32> = bitmap.indices_of_set_bits().collect();
        assert_eq!(bits, vec![0, 2, 65]);
    }

    #[test]
    fn test_id_set() {
        let bytes = [
            64, 0, 0, 0, // map_item_size_bits = 64
            64, 0, 0, 0, // high_bit = 64
            1, 0, 0, 0, // count = 1
            0, 0, 0, 0, // start_bit = 0
            5, 0, 0, 0, 0, 0, 0, 0, // map = 5 (bits 0 and 2 set)
        ];

        let mut cursor = PolicyCursor::new(&bytes);
        let id_set = IdSet::<TypeId, false>::parse(&mut cursor).unwrap();

        assert!(!id_set.is_empty());
        assert!(id_set.contains(TypeId::from_u32(1).unwrap()));
        assert!(!id_set.contains(TypeId::from_u32(2).unwrap()));
        assert!(id_set.contains(TypeId::from_u32(3).unwrap()));

        let ids: Vec<TypeId> = id_set.iter().collect();
        assert_eq!(ids, vec![TypeId::from_u32(1).unwrap(), TypeId::from_u32(3).unwrap()]);
    }
}
