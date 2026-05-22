// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::lsm_tree::types::{OrdLowerBound, OrdUpperBound};
use crate::round::{round_down, round_up};
use crate::serialized_types::serialized_key::{KeyDeserializer, KeySerializer, SerializeKey};
use crate::serialized_types::varint::Buffer;
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use std::cmp::{max, min};
use std::hash::Hash;
use std::ops::Range;

/// Extent represents a physical or logical range of bytes, aligned to a 512-byte boundary.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Serialize, Deserialize, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub struct Extent(pub Range<u64>);

impl Extent {
    /// Returns the range of bytes common between this extent and |other|.
    pub fn overlap(&self, other: &Extent) -> Option<Range<u64>> {
        if self.end <= other.start || self.start >= other.end {
            None
        } else {
            Some(max(self.start, other.start)..min(self.end, other.end))
        }
    }

    /// Returns the search key for this extent; that is, a key which is <= this key under Ord and
    /// OrdLowerBound.
    /// This would be used when searching for an extent with |find| (when we want to find any
    /// overlapping extent, which could include extents that start earlier).
    /// For example, if the tree has extents 50..150 and 150..200 and we wish to read 100..200,
    /// we'd search for 0..101 which would set the iterator to 50..150.
    pub fn search_key(&self) -> Self {
        assert_ne!(self.start, self.end);
        Extent::search_key_from_offset(self.start)
    }

    /// Similar to previous, but from an offset.  Returns a search key that will find the first
    /// extent that touches offset..
    pub fn search_key_from_offset(offset: u64) -> Self {
        Self(0..offset + 1)
    }

    /// Returns the merge key for this extent; that is, a key which is <= this extent and any other
    /// possibly overlapping extent, under Ord. This would be used to set the hint for |merge_into|.
    ///
    /// For example, if the tree has extents 0..50, 50..150 and 150..200 and we wish to insert
    /// 100..150, we'd use a merge hint of 0..100 which would set the iterator to 50..150 (the first
    /// element > 100..150 under Ord).
    pub fn key_for_merge_into(&self) -> Self {
        Self(0..self.start)
    }

    /// Returns an iterator over the Extent partitions which overlap this key (see `FuzzyHash`).
    pub fn fuzzy_hash_partition(&self) -> ExtentPartitionIterator {
        ExtentPartitionIterator {
            range: round_down(self.start, EXTENT_HASH_BUCKET_SIZE)
                ..round_up(self.end, EXTENT_HASH_BUCKET_SIZE).unwrap_or(u64::MAX),
        }
    }
}

impl SerializeKey for Extent {
    fn serialize_key_to<B: Buffer>(&self, serializer: &mut KeySerializer<'_, B>) {
        assert_eq!(self.0.end % 512, 0, "Extent end must be 512-byte aligned");
        assert_eq!(self.0.start % 512, 0, "Extent start must be 512-byte aligned");
        serializer.write_u64(self.0.end / 512);
        serializer.write_u64(self.0.start / 512);
    }

    fn deserialize_key_from(deserializer: &mut KeyDeserializer<'_>) -> Result<Self, anyhow::Error> {
        let end =
            deserializer.read_u64()?.checked_mul(512).ok_or_else(|| anyhow::anyhow!("Overflow"))?;
        let start =
            deserializer.read_u64()?.checked_mul(512).ok_or_else(|| anyhow::anyhow!("Overflow"))?;
        Ok(Self(start..end))
    }
}

impl std::ops::Deref for Extent {
    type Target = Range<u64>;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for Extent {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

impl From<Range<u64>> for Extent {
    fn from(range: Range<u64>) -> Self {
        Self(range)
    }
}

impl From<Extent> for Range<u64> {
    fn from(key: Extent) -> Self {
        key.0
    }
}

const EXTENT_HASH_BUCKET_SIZE: u64 = 1 * 1024 * 1024;

pub struct ExtentPartitionIterator {
    range: Range<u64>,
}

impl Iterator for ExtentPartitionIterator {
    type Item = Range<u64>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.range.start >= self.range.end {
            None
        } else {
            let start = self.range.start;
            self.range.start = start.saturating_add(EXTENT_HASH_BUCKET_SIZE);
            let end = std::cmp::min(self.range.start, self.range.end);
            Some(start..end)
        }
    }
}

// The normal comparison uses the end of the range before the start of the range. This makes
// searching for records easier because it's easy to find K.. (where K is the key you are searching
// for), which is what we want since our search routines find items with keys >= a search key.
// OrdLowerBound orders by the start of an extent.
impl OrdUpperBound for Extent {
    fn cmp_upper_bound(&self, other: &Extent) -> std::cmp::Ordering {
        // The comparison uses the end of the range so that we can more easily do queries. Ties
        // are broken by comparing the range start to provide a total ordering consistent with
        // serialization. Since we do not support overlapping keys within the same layer, ties can
        // always be broken using layer index. Insertions into the mutable layer should always be
        // done using merge_into, which will ensure keys don't end up overlapping.
        self.end.cmp(&other.end).then(self.start.cmp(&other.start))
    }
}

impl OrdLowerBound for Extent {
    // Orders by the start of the range rather than the end, and doesn't include the end in the
    // comparison. This is used when merging, where we want to merge keys in lower-bound order.
    fn cmp_lower_bound(&self, other: &Extent) -> std::cmp::Ordering {
        self.start.cmp(&other.start)
    }
}

impl Ord for Extent {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        // We expect cmp_upper_bound and cmp_lower_bound to be used mostly, but ObjectKey needs an
        // Ord method in order to compare other enum variants, and Transaction requires an ObjectKey
        // to implement Ord.
        self.start.cmp(&other.start).then(self.end.cmp(&other.end))
    }
}

impl PartialOrd for Extent {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

#[cfg(test)]
mod tests {
    use super::Extent;
    use crate::lsm_tree::types::{OrdLowerBound, OrdUpperBound};
    use crate::serialized_types::serialized_key::{KeyDeserializer, SerializeKey};
    use std::cmp::Ordering;

    #[test]
    fn test_extent_key_serialization() {
        let key = Extent(1024..2048);
        let mut buf = Vec::new();

        // Serialize
        {
            let mut ser =
                crate::serialized_types::serialized_key::KeySerializer::new(&mut buf, Some(0));
            key.serialize_key_to(&mut ser);
            ser.finalize();
        }

        let (mut deser, length) = KeyDeserializer::new(&buf, Some(0)).unwrap();
        assert_eq!(length, buf.len());
        let decoded_key = Extent::deserialize_key_from(&mut deser).unwrap();

        assert_eq!(key, decoded_key);

        // Verify bytes:
        // 2048 / 512 = 4.
        // 1024 / 512 = 2.
        // Delta encoding applies to first field (end = 4). Base is 0. 4 - 0 = 4.
        // Second field is start = 2. Base is None (taken). So writes 2.
        // Buffer should be [0, 2, 4, 2].
        assert_eq!(buf, vec![0, 2, 4, 2]);
    }

    #[test]
    fn test_extent_key_deserialization_overflow() {
        let mut buf = Vec::new();
        {
            let mut ser =
                crate::serialized_types::serialized_key::KeySerializer::new(&mut buf, None);
            ser.write_u64(u64::MAX);
            ser.write_u64(u64::MAX);
            ser.finalize();
        }
        let (mut deser, length) = KeyDeserializer::new(&buf, None).unwrap();
        assert_eq!(length, buf.len());
        let result = Extent::deserialize_key_from(&mut deser);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().to_string(), "Overflow");
    }

    #[test]
    #[should_panic(expected = "Extent end must be 512-byte aligned")]
    fn test_extent_key_serialization_unaligned_end_panics() {
        let key = Extent(1024..2049);
        let mut buf = Vec::new();
        let mut ser = crate::serialized_types::serialized_key::KeySerializer::new(&mut buf, None);
        key.serialize_key_to(&mut ser);
    }

    #[test]
    #[should_panic(expected = "Extent start must be 512-byte aligned")]
    fn test_extent_key_serialization_unaligned_start_panics() {
        let key = Extent(1025..2048);
        let mut buf = Vec::new();
        let mut ser = crate::serialized_types::serialized_key::KeySerializer::new(&mut buf, None);
        key.serialize_key_to(&mut ser);
    }

    #[test]
    fn test_extent_cmp() {
        let extent = Extent(100..150);
        assert_eq!(extent.cmp_upper_bound(&Extent(0..100)), Ordering::Greater);
        assert_eq!(extent.cmp_upper_bound(&Extent(0..110)), Ordering::Greater);
        assert_eq!(extent.cmp_upper_bound(&Extent(0..150)), Ordering::Greater);
        assert_eq!(extent.cmp_upper_bound(&Extent(99..150)), Ordering::Greater);
        assert_eq!(extent.cmp_upper_bound(&Extent(100..150)), Ordering::Equal);
        assert_eq!(extent.cmp_upper_bound(&Extent(0..151)), Ordering::Less);
        assert_eq!(extent.cmp_upper_bound(&Extent(100..151)), Ordering::Less);
        assert_eq!(extent.cmp_upper_bound(&Extent(150..1000)), Ordering::Less);
        assert_eq!(extent.cmp_upper_bound(&Extent(101..150)), Ordering::Less);
    }

    #[test]
    fn test_extent_cmp_lower_bound() {
        let extent = Extent(100..150);
        assert_eq!(extent.cmp_lower_bound(&Extent(0..100)), Ordering::Greater);
        assert_eq!(extent.cmp_lower_bound(&Extent(0..110)), Ordering::Greater);
        assert_eq!(extent.cmp_lower_bound(&Extent(0..150)), Ordering::Greater);
        assert_eq!(extent.cmp_lower_bound(&Extent(0..1000)), Ordering::Greater);
        assert_eq!(extent.cmp_lower_bound(&Extent(99..1000)), Ordering::Greater);
        assert_eq!(extent.cmp_lower_bound(&Extent(100..150)), Ordering::Equal);
        // cmp_lower_bound does not check the upper bound of the range
        assert_eq!(extent.cmp_lower_bound(&Extent(100..1000)), Ordering::Equal);
        assert_eq!(extent.cmp_lower_bound(&Extent(101..102)), Ordering::Less);
    }

    #[test]
    fn test_extent_search_and_insertion_key() {
        let extent = Extent(100..150);
        assert_eq!(extent.search_key(), Extent(0..101));
        assert_eq!(extent.cmp_lower_bound(&extent.search_key()), Ordering::Greater);
        assert_eq!(extent.cmp_upper_bound(&extent.search_key()), Ordering::Greater);
        assert_eq!(extent.key_for_merge_into(), Extent(0..100));
        assert_eq!(extent.cmp_lower_bound(&extent.key_for_merge_into()), Ordering::Greater);
    }
}
