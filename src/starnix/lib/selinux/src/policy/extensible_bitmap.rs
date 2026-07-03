// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::error::ValidateError;
use super::parser::PolicyCursor;
use super::{
    Array, Counted, PolicyValidationContext, Validate, ValidateArray, array_type,
    array_type_validate_deref_both,
};

use std::cmp::Ordering;
use zerocopy::{FromBytes, Immutable, KnownLayout, Unaligned, little_endian as le};

/// Maximum number of [`MapItem`] objects in a single [`ExtensibleBitmap`].
pub(super) const MAX_BITMAP_ITEMS: u32 = 0x40;

/// Fixed expectation for number of bits per [`MapItem`] in every [`ExtensibleBitmap`].
pub(super) const MAP_NODE_BITS: u32 = 8 * std::mem::size_of::<u64>() as u32;

array_type!(ExtensibleBitmap, Metadata, MapItem);

array_type_validate_deref_both!(ExtensibleBitmap);

impl ExtensibleBitmap {
    /// Returns whether the `index`'th bit in this bitmap is a 1-bit.
    pub fn is_set(&self, index: u32) -> bool {
        if index > self.high_bit() {
            return false;
        }

        let map_items = &self.data;
        if let Ok(i) = map_items.binary_search_by(|map_item| self.item_ordering(map_item, index)) {
            let map_item = &map_items[i];
            let item_index = index - map_item.start_bit.get();
            return map_item.map.get() & (1 << item_index) != 0;
        }

        false
    }

    /// Returns an iterator that yields the indices of this [`ExtensibleBitmap`]'s set bits.
    pub fn indices_of_set_bits<'a>(&'a self) -> impl Iterator<Item = u32> + Clone {
        ExtensibleBitmapSpansIterator::<'a> { bitmap: self, next_map_item: 0, map: 0, start_bit: 0 }
            .flat_map(|span| span.low..=span.high)
    }

    /// Returns an iterator that returns a set of spans of continuous set bits.
    /// Each span consists of inclusive low and high bit indexes (i.e. zero-based).
    pub fn spans<'a>(&'a self) -> ExtensibleBitmapSpansIterator<'a> {
        ExtensibleBitmapSpansIterator::<'a> { bitmap: self, next_map_item: 0, map: 0, start_bit: 0 }
    }

    /// Returns the next bit after the bits in this [`ExtensibleBitmap`]. That is, the bits in this
    /// [`ExtensibleBitmap`] may be indexed by the range `[0, Self::high_bit())`.
    fn high_bit(&self) -> u32 {
        self.metadata.high_bit.get()
    }

    fn item_ordering(&self, map_item: &MapItem, index: u32) -> Ordering {
        let map_item_start_bit = map_item.start_bit.get();
        if map_item_start_bit > index {
            Ordering::Greater
        } else if map_item_start_bit + self.metadata.map_item_size_bits.get() <= index {
            Ordering::Less
        } else {
            Ordering::Equal
        }
    }
}

/// Describes the indexes of a span of "true" bits in an `ExtensibleBitmap`.
/// Low and high values are inclusive, such that when `low==high`, the span consists
/// of a single bit.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct ExtensibleBitmapSpan {
    pub low: u32,
    pub high: u32,
}

/// Iterator returned by `ExtensibleBitmap::spans()`.
#[derive(Clone)]
pub(super) struct ExtensibleBitmapSpansIterator<'a> {
    bitmap: &'a ExtensibleBitmap,
    // Zero-based `Vec<MapItem>` index of the next MapItem to read.
    next_map_item: usize,
    // The not yet iterated bits of the most recently read MapItem.
    map: u64,
    // The index of the LSB of map.
    start_bit: u32,
}

impl Iterator for ExtensibleBitmapSpansIterator<'_> {
    type Item = ExtensibleBitmapSpan;

    /// Returns the next span of at least one bit set in the bitmap.
    fn next(&mut self) -> Option<Self::Item> {
        // If we've finished our current MapItem, move onto the next one.
        if self.map == 0 {
            let Some(&MapItem { start_bit, map }) = self.bitmap.data.get(self.next_map_item) else {
                return None;
            };
            self.start_bit = start_bit.get();
            self.map = map.get();
            self.next_map_item += 1;
        }

        // Shift away any zeros and begin our span.
        let zero_bits = self.map.trailing_zeros();
        self.map >>= zero_bits;
        self.start_bit += zero_bits;
        let low = self.start_bit;

        // A span may bridge multiple MapItems. Continue to read 1s until either we don't reach the
        // end of the current MapItem, or the next MapItem isn't contiguous.
        loop {
            let one_bits = self.map.trailing_ones();
            // This map could be all 1s, in which case a shift by 64 would overflow. That's fine,
            // we want to shift away all the bits anyways in that case.
            self.map = self.map.checked_shr(one_bits).unwrap_or(0);
            self.start_bit += one_bits;

            if self.start_bit % 64 != 0 {
                // We didn't reach the end of the current MapItem.
                break;
            }

            if let Some(&MapItem { start_bit, map }) = self.bitmap.data.get(self.next_map_item)
                && start_bit == self.start_bit
                && (map & 1) == 1
            {
                self.map = map.get();
                self.next_map_item += 1;
            } else {
                break;
            };
        }

        Some(Self::Item { low, high: self.start_bit - 1 })
    }
}

impl Validate for Metadata {
    type Error = ValidateError;

    /// Validates that [`ExtensibleBitmap`] metadata is internally consistent with data
    /// representation assumptions.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        // Only one size for `MapItem` instances is supported.
        let found_size = self.map_item_size_bits.get();
        if found_size != MAP_NODE_BITS {
            return Err(ValidateError::InvalidExtensibleBitmapItemSize { found_size });
        }

        // High bit must be `MapItem` size-aligned.
        let found_high_bit = self.high_bit.get();
        if found_high_bit % found_size != 0 {
            return Err(ValidateError::MisalignedExtensibleBitmapHighBit {
                found_size,
                found_high_bit,
            });
        }

        // Count and high bit must be consistent.
        let found_count = self.count.get();
        if found_count * found_size > found_high_bit {
            return Err(ValidateError::InvalidExtensibleBitmapHighBit {
                found_size,
                found_high_bit,
                found_count,
            });
        }
        if found_count > MAX_BITMAP_ITEMS {
            return Err(ValidateError::InvalidExtensibleBitmapCount { found_count });
        }
        if found_high_bit != 0 && found_count == 0 {
            return Err(ValidateError::ExtensibleBitmapNonZeroHighBitAndZeroCount);
        }

        Ok(())
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct Metadata {
    /// How many bits on each `MapItem`.
    map_item_size_bits: le::U32,
    /// Highest bit, non-inclusive.
    high_bit: le::U32,
    /// The number of map items.
    count: le::U32,
}

impl Counted for Metadata {
    /// The number of [`MapItem`] objects that follow a [`Metadata`] is the value stored in the
    /// `metadata.count` field.
    fn count(&self) -> u32 {
        self.count.get()
    }
}

#[derive(Clone, Debug, KnownLayout, FromBytes, Immutable, PartialEq, Unaligned)]
#[repr(C, packed)]
pub(super) struct MapItem {
    /// The first bit that this [`MapItem`] stores, relative to its [`ExtensibleBitmap`] range:
    /// `[0, extensible_bitmap.high_bit())`.
    start_bit: le::U32,
    /// The bitmap data for this [`MapItem`].
    map: le::U64,
}

impl Validate for MapItem {
    type Error = anyhow::Error;

    /// All [`MapItem`] validation requires access to [`Metadata`]; validation performed in
    /// `ExtensibleBitmap<PS>::validate()`.
    fn validate(&self, _context: &PolicyValidationContext) -> Result<(), Self::Error> {
        if self.map == 0 {
            return Err(ValidateError::InvalidExtensibleBitmapItem.into());
        }
        Ok(())
    }
}

impl ValidateArray<Metadata, MapItem> for ExtensibleBitmap {
    type Error = anyhow::Error;

    /// Validates that `metadata` and `data` are internally consistent. [`MapItem`] objects are
    /// expected to be stored in ascending order (by `start_bit`), and their bit ranges must fall
    /// within the range `[0, metadata.high_bit())`.
    fn validate_array(
        _context: &PolicyValidationContext,
        metadata: &Metadata,
        items: &[MapItem],
    ) -> Result<(), Self::Error> {
        let found_size = metadata.map_item_size_bits.get();
        let found_high_bit = metadata.high_bit.get();

        // `MapItem` objects must be in sorted order, each with a `MapItem` size-aligned starting bit.
        //
        // Note: If sorted order assumption is violated `ExtensibleBitmap::binary_search_items()` will
        // misbehave and `ExtensibleBitmap` will need to be refactored accordingly.
        let mut min_start: u32 = 0;
        for map_item in items {
            let found_start_bit = map_item.start_bit.get();
            if found_start_bit % found_size != 0 {
                return Err(ValidateError::MisalignedExtensibleBitmapItemStartBit {
                    found_start_bit,
                    found_size,
                }
                .into());
            }
            if found_start_bit < min_start {
                return Err(ValidateError::OutOfOrderExtensibleBitmapItems {
                    found_start_bit,
                    min_start,
                }
                .into());
            }
            min_start = found_start_bit + found_size;
        }

        // Last `MapItem` object may not include bits beyond (and including) high bit value.
        if min_start > found_high_bit {
            return Err(ValidateError::ExtensibleBitmapItemOverflow {
                found_items_end: min_start,
                found_high_bit,
            }
            .into());
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::error::ParseError;
    use super::super::parser::{PolicyCursor, PolicyData};
    use super::super::testing::{as_parse_error, as_validate_error};
    use super::super::{Parse, PolicyValidationContext};
    use super::*;

    use std::borrow::Borrow;
    use std::sync::Arc;

    macro_rules! parse_test {
        ($parse_output:ident, $data:expr, $result:tt, $policy_data:tt, $check_impl:block) => {{
            let data: PolicyData = Arc::from($data);
            fn check_by_value<'a>(
                $result: Result<($parse_output, PolicyCursor<'a>), <$parse_output as Parse>::Error>,
                $policy_data: &PolicyData,
            ) -> Option<($parse_output, PolicyCursor<'a>)> {
                $check_impl
            }

            let by_value_result = $parse_output::parse(PolicyCursor::new(&data));
            let _ = check_by_value(by_value_result, &data);
        }};
    }

    struct ExtensibleBitmapIterator<B: Borrow<ExtensibleBitmap>> {
        extensible_bitmap: B,
        i: u32,
    }

    impl<B: Borrow<ExtensibleBitmap>> Iterator for ExtensibleBitmapIterator<B> {
        type Item = bool;

        fn next(&mut self) -> Option<Self::Item> {
            if self.i >= self.extensible_bitmap.borrow().high_bit() {
                return None;
            }
            let value = self.extensible_bitmap.borrow().is_set(self.i);
            self.i = self.i + 1;
            Some(value)
        }
    }

    impl ExtensibleBitmap {
        fn iter(&self) -> ExtensibleBitmapIterator<&ExtensibleBitmap> {
            ExtensibleBitmapIterator { extensible_bitmap: self, i: 0 }
        }
    }

    #[test]
    fn extensible_bitmap_simple() {
        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                MAP_NODE_BITS.to_le_bytes().as_slice(), // high bit for 1-item bitmap
                (1 as u32).to_le_bytes().as_slice(), // count of `MapItem` entries in 1-item bitmap
                (0 as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                (1 as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
            ]
            .concat(),
            result,
            _policy_data,
            {
                let (extensible_bitmap, tail) = result.expect("parse");
                assert_eq!(24, tail.offset());
                let mut count: u32 = 0;
                for (i, bit) in extensible_bitmap.iter().enumerate() {
                    assert!((i == 0 && bit) || (i > 0 && !bit));
                    count = count + 1;
                }
                assert_eq!(MAP_NODE_BITS, count);
                Some((extensible_bitmap, tail))
            }
        );
    }

    #[test]
    fn extensible_bitmap_sparse_two_item() {
        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for 2-item bitmap
                (2 as u32).to_le_bytes().as_slice(), // count of `MapItem` entries  in 2-item bitmap
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            _policy_data,
            {
                let (extensible_bitmap, tail) = result.expect("parse");
                assert_eq!(36, tail.offset());
                for i in 0..(MAP_NODE_BITS * 10) {
                    let expected = i == ((MAP_NODE_BITS * 2) + 2) || i == ((MAP_NODE_BITS * 7) + 7);
                    assert_eq!(expected, extensible_bitmap.is_set(i));
                }

                let mut count: u32 = 0;
                for (i, bit) in extensible_bitmap.iter().enumerate() {
                    let expected = i == (((MAP_NODE_BITS * 2) + 2) as usize)
                        || i == (((MAP_NODE_BITS * 7) + 7) as usize);
                    assert_eq!(expected, bit);
                    count = count + 1;
                }
                assert_eq!(MAP_NODE_BITS * 10, count);
                Some((extensible_bitmap, tail))
            }
        );
    }

    #[test]
    fn extensible_bitmap_sparse_malformed() {
        parse_test!(
            ExtensibleBitmap,
            [
                (MAP_NODE_BITS - 1).to_le_bytes().as_slice(), // invalid bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for 2-item bitmap
                (2 as u32).to_le_bytes().as_slice(), // count of `MapItem` entries in 2-item bitmap
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            policy_data,
            {
                let (parsed, tail) = result.expect("parsed");
                assert_eq!(36, tail.offset());
                let context =
                    PolicyValidationContext { data: policy_data.clone(), need_init_sid: false };
                assert_eq!(
                    Err(ValidateError::InvalidExtensibleBitmapItemSize {
                        found_size: MAP_NODE_BITS - 1
                    }),
                    parsed.validate(&context).map_err(as_validate_error)
                );
                Some((parsed, tail))
            }
        );

        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                (((MAP_NODE_BITS * 10) + 1) as u32).to_le_bytes().as_slice(), // invalid high bit for 2-item bitmap
                (2 as u32).to_le_bytes().as_slice(), // count of `MapItem` entries in 2-item bitmap
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            policy_data,
            {
                let (parsed, tail) = result.expect("parsed");
                assert_eq!(36, tail.offset());
                let context =
                    PolicyValidationContext { data: policy_data.clone(), need_init_sid: false };
                assert_eq!(
                    Err(ValidateError::MisalignedExtensibleBitmapHighBit {
                        found_size: MAP_NODE_BITS,
                        found_high_bit: (MAP_NODE_BITS * 10) + 1
                    }),
                    parsed.validate(&context).map_err(as_validate_error),
                );
                Some((parsed, tail))
            }
        );

        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for 2-item bitmap
                (11 as u32).to_le_bytes().as_slice(), // invalid count of `MapItem` entries in 2-item bitmap
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            _policy_data,
            {
                match result.err().map(Into::<anyhow::Error>::into).map(as_parse_error) {
                    // `PolicyCursor` attempts to read `Vec` one item at a time.
                    Some(ParseError::MissingData { type_name, type_size, num_bytes: 0 }) => {
                        assert_eq!(std::any::type_name::<MapItem>(), type_name);
                        assert_eq!(std::mem::size_of::<MapItem>(), type_size);
                    }
                    v => {
                        panic!(
                            "Expected Some({:?}), but got {:?}",
                            ParseError::MissingData {
                                type_name: std::any::type_name::<MapItem>(),
                                type_size: std::mem::size_of::<MapItem>(),
                                num_bytes: 0,
                            },
                            v
                        );
                    }
                };
                None
            }
        );

        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for 2-item bitmap
                (2 as u32).to_le_bytes().as_slice(), // count of `MapItem` entries in 2-item bitmap
                (((MAP_NODE_BITS * 2) + 1) as u32).to_le_bytes().as_slice(), // invalid start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            policy_data,
            {
                let (parsed, tail) = result.expect("parsed");
                assert_eq!(36, tail.offset());
                let context =
                    PolicyValidationContext { data: policy_data.clone(), need_init_sid: false };
                match parsed.validate(&context).map_err(as_validate_error) {
                    Err(ValidateError::MisalignedExtensibleBitmapItemStartBit {
                        found_start_bit,
                        ..
                    }) => {
                        assert_eq!((MAP_NODE_BITS * 2) + 1, found_start_bit);
                    }
                    parse_err => {
                        assert!(
                            false,
                            "Expected Err(MisalignedExtensibleBitmapItemStartBit...), but got {:?}",
                            parse_err
                        );
                    }
                }
                Some((parsed, tail))
            }
        );

        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for 2-item bitmap
                (2 as u32).to_le_bytes().as_slice(), // count of `MapItem` entries in 2-item bitmap
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // out-of-order start bit for `MapItem` 0
                ((1 << 7) as u64).to_le_bytes().as_slice(),            // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // out-of-order start bit for `MapItem` 1
                ((1 << 2) as u64).to_le_bytes().as_slice(),            // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            policy_data,
            {
                let (parsed, tail) = result.expect("parsed");
                assert_eq!(36, tail.offset());
                let context =
                    PolicyValidationContext { data: policy_data.clone(), need_init_sid: false };
                assert_eq!(
                    parsed.validate(&context).map_err(as_validate_error),
                    Err(ValidateError::OutOfOrderExtensibleBitmapItems {
                        found_start_bit: MAP_NODE_BITS * 2,
                        min_start: (MAP_NODE_BITS * 7) + MAP_NODE_BITS,
                    })
                );
                Some((parsed, tail))
            }
        );

        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for 2-item bitmap
                (3 as u32).to_le_bytes().as_slice(), // invalid count of `MapItem` entries in 2-item bitmap
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            _policy_data,
            {
                match result.err().map(Into::<anyhow::Error>::into).map(as_parse_error) {
                    // `PolicyCursor` attempts to read `Vec` one item at a time.
                    Some(ParseError::MissingData { type_name, type_size, num_bytes: 0 }) => {
                        assert_eq!(std::any::type_name::<MapItem>(), type_name);
                        assert_eq!(std::mem::size_of::<MapItem>(), type_size);
                    }
                    parse_err => {
                        assert!(
                            false,
                            "Expected Some({:?}), but got {:?}",
                            ParseError::MissingData {
                                type_name: std::any::type_name::<MapItem>(),
                                type_size: std::mem::size_of::<MapItem>(),
                                num_bytes: 0
                            },
                            parse_err
                        );
                    }
                };
                None
            }
        );
    }

    #[test]
    fn extensible_bitmap_spans_iterator() {
        type Span = ExtensibleBitmapSpan;

        // Single- and multi-bit spans.
        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for bitmap
                (2 as u32).to_le_bytes().as_slice(),    // count of `MapItem` entries in bitmap
                ((MAP_NODE_BITS * 2) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 << 2) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 7) | (1 << 8) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            _policy_data,
            {
                let (extensible_bitmap, tail) = result.expect("parse");
                assert_eq!(36, tail.offset());

                let mut iterator = extensible_bitmap.spans();
                assert_eq!(
                    iterator.next(),
                    Some(Span { low: (MAP_NODE_BITS * 2) + 2, high: (MAP_NODE_BITS * 2) + 2 })
                );
                assert_eq!(
                    iterator.next(),
                    Some(Span { low: (MAP_NODE_BITS * 7) + 7, high: (MAP_NODE_BITS * 7) + 8 })
                );
                assert_eq!(iterator.next(), None);

                Some((extensible_bitmap, tail))
            }
        );

        // Multi-bit span that straddles two `MapItem`s.
        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for bitmap
                (2 as u32).to_le_bytes().as_slice(),    // count of `MapItem` entries in bitmap
                ((MAP_NODE_BITS * 6) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                ((1 as u64) << 63).to_le_bytes().as_slice(), // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                ((1 << 0) | (1 << 1) as u64).to_le_bytes().as_slice(), // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            _policy_data,
            {
                let (extensible_bitmap, tail) = result.expect("parse");
                assert_eq!(36, tail.offset());

                let mut iterator = extensible_bitmap.spans();
                assert_eq!(
                    iterator.next(),
                    Some(Span { low: (MAP_NODE_BITS * 6) + 63, high: (MAP_NODE_BITS * 7) + 1 })
                );
                assert_eq!(iterator.next(), None);

                Some((extensible_bitmap, tail))
            }
        );

        // Multi-bit spans of full `MapItem`s, separated by an implicit span of false bits,
        // and with further implicit spans of false bits at the end.
        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for bitmap
                (2 as u32).to_le_bytes().as_slice(),    // count of `MapItem` entries in bitmap
                ((MAP_NODE_BITS * 5) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                (u64::MAX).to_le_bytes().as_slice(),    // bit values for `MapItem` 0
                ((MAP_NODE_BITS * 7) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 1
                (u64::MAX).to_le_bytes().as_slice(),    // bit values for `MapItem` 1
            ]
            .concat(),
            result,
            _policy_data,
            {
                let (extensible_bitmap, tail) = result.expect("parse");
                assert_eq!(36, tail.offset());

                let mut iterator = extensible_bitmap.spans();
                assert_eq!(
                    iterator.next(),
                    Some(Span { low: (MAP_NODE_BITS * 5), high: (MAP_NODE_BITS * 6) - 1 })
                );
                assert_eq!(
                    iterator.next(),
                    Some(Span { low: (MAP_NODE_BITS * 7), high: (MAP_NODE_BITS * 8) - 1 })
                );
                assert_eq!(iterator.next(), None);

                Some((extensible_bitmap, tail))
            }
        );

        // Span reaching the end of the bitmap is handled correctly.
        parse_test!(
            ExtensibleBitmap,
            [
                MAP_NODE_BITS.to_le_bytes().as_slice(), // bits per node
                ((MAP_NODE_BITS * 10) as u32).to_le_bytes().as_slice(), // high bit for bitmap
                (1 as u32).to_le_bytes().as_slice(),    // count of `MapItem` entries  in bitmap
                ((MAP_NODE_BITS * 9) as u32).to_le_bytes().as_slice(), // start bit for `MapItem` 0
                (u64::MAX).to_le_bytes().as_slice(),    // bit values for `MapItem` 0
            ]
            .concat(),
            result,
            _policy_data,
            {
                let (extensible_bitmap, tail) = result.expect("parse");
                assert_eq!(24, tail.offset());

                let mut iterator = extensible_bitmap.spans();
                assert_eq!(
                    iterator.next(),
                    Some(Span { low: (MAP_NODE_BITS * 9), high: (MAP_NODE_BITS * 10) - 1 })
                );
                assert_eq!(iterator.next(), None);

                Some((extensible_bitmap, tail))
            }
        );
    }
}
