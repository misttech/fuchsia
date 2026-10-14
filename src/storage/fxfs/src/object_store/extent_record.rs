// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// TODO(https://fxbug.dev/42178223): need validation after deserialization.

use crate::checksum::{Checksum, Checksums, ChecksumsV38};
use bit_vec::BitVec;
use fprint::TypeFingerprint;
use serde::{Deserialize, Serialize};
use static_assertions::const_assert_eq;

/// The mode the extent is operating in. This changes how writes work to this region of the file.
pub type ExtentMode = ExtentModeV38;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TypeFingerprint)]
pub enum ExtentModeV38 {
    /// This extent doesn't have defined write semantics. The writer chooses how to handle the data
    /// here. Notable uses of this are things which have their own separate checksum mechanism,
    /// like the journal file and blobs.
    Raw,
    /// This extent uses copy-on-write semantics. We store the post-encryption checksums for data
    /// validation. New writes to this logical range are written to new extents.
    Cow(ChecksumsV38),
    /// This extent uses overwrite semantics. The bitmap keeps track of blocks which have been
    /// written to at least once. Blocks which haven't been written to at least once are logically
    /// zero, so the bitmap needs to be accounted for while reading. While this extent exists, new
    /// writes to this logical range will go to the same on-disk location.
    OverwritePartial(BitVec),
    /// This extent uses overwrite semantics. Every block in this extent has been written to at
    /// least once, so we don't store the block bitmap anymore. While this extent exists, new
    /// writes to this logical range will go to the same on-disk location.
    Overwrite,
}

#[cfg(fuzz)]
impl<'a> arbitrary::Arbitrary<'a> for ExtentMode {
    fn arbitrary(u: &mut arbitrary::Unstructured<'a>) -> arbitrary::Result<Self> {
        Ok(match u.int_in_range(0..=3)? {
            0 => ExtentMode::Raw,
            1 => ExtentMode::Cow(u.arbitrary()?),
            2 => ExtentMode::OverwritePartial(BitVec::from_bytes(u.arbitrary()?)),
            3 => ExtentMode::Overwrite,
            _ => unreachable!(),
        })
    }
}

/// ExtentValue is the payload for an extent in the object store, which describes where the extent
/// is physically located.
pub type ExtentValue = ExtentValueV38;

/// When barriers are disabled, each block's checksum (`Checksum`) is written to the journal twice:
/// once in the object store's `ExtentMode::Cow(Checksums)` mutation and once in
/// `JournalRecord::DataChecksums`. Because `Checksums` uses `zerocopy_serialization`, each
/// checksum is serialized as raw uncompressed bytes (`std::mem::size_of::<Checksum>()`).
pub const JOURNAL_CHECKSUM_BYTES_PER_BLOCK: usize = 2 * std::mem::size_of::<Checksum>();
const_assert_eq!(JOURNAL_CHECKSUM_BYTES_PER_BLOCK, 16);

// LINT.IfChange
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, TypeFingerprint)]
#[cfg_attr(fuzz, derive(arbitrary::Arbitrary))]
pub enum ExtentValueV38 {
    /// Indicates a deleted extent; that is, the logical range described by the extent key is
    /// considered to be deleted.
    None,
    /// The location of the extent and other related information.  `key_id` identifies which of the
    /// object's keys should be used.  Unencrypted files should use 0 (which can also be used for
    /// encrypted files).  `mode` describes the write pattern for this extent.
    Some { device_offset: u64, mode: ExtentModeV38, key_id: u64 },
}

/// Maximum serialized size of a varint-encoded `u64` in bincode (`0xFF` tag byte + 8-byte `u64`).
const MAX_VARINT_U64_SIZE: usize = 1 + std::mem::size_of::<u64>();
const_assert_eq!(MAX_VARINT_U64_SIZE, 9);

/// Maximum serialized size of an `ObjectKey::extent` record:
/// `object_id` (u64) + `ObjectKeyData::Attribute` tag (1) + `AttributeId` (u64)
/// + `AttributeKey::Extent` tag (1) + `Extent(Range<u64>)` (2 * u64).
const MAX_EXTENT_KEY_SERIALIZED_SIZE: usize = MAX_VARINT_U64_SIZE // object_id
    + 1 // ObjectKeyData::Attribute discriminant
    + MAX_VARINT_U64_SIZE // attribute_id
    + 1 // AttributeKey::Extent discriminant
    + 2 * MAX_VARINT_U64_SIZE; // Extent(Range<u64>)
const_assert_eq!(MAX_EXTENT_KEY_SERIALIZED_SIZE, 38);

/// Maximum serialized size of an `ObjectValue::Extent(ExtentValue::Some)` record (excluding the
/// per-block checksum bytes):
/// `ObjectValue::Extent` tag (1) + `ExtentValue::Some` tag (1) + `device_offset` (u64)
/// + `ExtentMode::Cow` tag (1) + `Checksums` byte-slice length (u64) + `key_id` (u64).
const MAX_EXTENT_VALUE_SERIALIZED_SIZE: usize = 1 // ObjectValue::Extent discriminant
    + 1 // ExtentValue::Some discriminant
    + MAX_VARINT_U64_SIZE // device_offset
    + 1 // ExtentMode::Cow discriminant
    + MAX_VARINT_U64_SIZE // Checksums byte-slice length
    + MAX_VARINT_U64_SIZE; // key_id
const_assert_eq!(MAX_EXTENT_VALUE_SERIALIZED_SIZE, 30);

/// Maximum serialized size of a `JournalRecord::Mutation` wrapping an
/// `AllocatorMutation::Allocate` (or `Deallocate`) record:
/// `JournalRecord::Mutation` tag (1) + `allocator_object_id` (u64) + `Mutation::Allocator` tag (1)
/// + `AllocatorMutation::Allocate` tag (1) + `device_range: Range<u64>` (2 * u64)
/// + `owner_object_id` (u64).
const MAX_ALLOCATOR_JOURNAL_RECORD_SERIALIZED_SIZE: usize = 1 // JournalRecord::Mutation tag
    + MAX_VARINT_U64_SIZE // allocator_object_id
    + 1 // Mutation::Allocator discriminant
    + 1 // AllocatorMutation::Allocate discriminant
    + 2 * MAX_VARINT_U64_SIZE // device_range: Range<u64>
    + MAX_VARINT_U64_SIZE; // owner_object_id
const_assert_eq!(MAX_ALLOCATOR_JOURNAL_RECORD_SERIALIZED_SIZE, 39);

/// Maximum serialized journal overhead per range in `multi_write_internal` (excluding the
/// per-block checksum payload, which is accounted for by `JOURNAL_CHECKSUM_BYTES_PER_BLOCK`, and
/// assuming one contiguous allocation per range):
/// - `JournalRecord::Mutation` (object store extent): tag (1) + `store_object_id` (u64)
///   + `Mutation::ObjectStore` tag (1) + `MAX_EXTENT_KEY_SERIALIZED_SIZE`
///   + `MAX_EXTENT_VALUE_SERIALIZED_SIZE` + `Item::sequence` (u64) + `Operation::Merge` tag (1).
/// - `JournalRecord::DataChecksums`: tag (1) + `device_range: Range<u64>` (2 * u64)
///   + `Checksums` byte-slice length (u64) + `first_write` (bool, 1).
/// - `JournalRecord::Mutation` (allocator): `MAX_ALLOCATOR_JOURNAL_RECORD_SERIALIZED_SIZE`.
pub const MAX_JOURNAL_BYTES_PER_RANGE: usize = (1 // JournalRecord::Mutation discriminant
    + MAX_VARINT_U64_SIZE // store_object_id
    + 1 // Mutation::ObjectStore discriminant
    + MAX_EXTENT_KEY_SERIALIZED_SIZE
    + MAX_EXTENT_VALUE_SERIALIZED_SIZE
    + MAX_VARINT_U64_SIZE // Item::sequence
    + 1) // Operation::Merge discriminant
    + (1 // JournalRecord::DataChecksums discriminant
    + 2 * MAX_VARINT_U64_SIZE // device_range: Range<u64>
    + MAX_VARINT_U64_SIZE // Checksums byte-slice length
    + 1) // first_write: bool
    + MAX_ALLOCATOR_JOURNAL_RECORD_SERIALIZED_SIZE;
const_assert_eq!(MAX_JOURNAL_BYTES_PER_RANGE, 157);
// LINT.ThenChange(//src/storage/fxfs/src/object_store/object_record.rs, //src/storage/fxfs/src/object_store/transaction.rs)

impl ExtentValue {
    pub fn new(device_offset: u64, mode: ExtentMode, key_id: u64) -> ExtentValue {
        ExtentValue::Some { device_offset, mode, key_id }
    }

    pub fn new_raw(device_offset: u64, key_id: u64) -> ExtentValue {
        Self::new(device_offset, ExtentMode::Raw, key_id)
    }

    /// Creates an ExtentValue with a checksum
    pub fn with_checksum(device_offset: u64, checksums: Checksums, key_id: u64) -> ExtentValue {
        Self::new(device_offset, ExtentMode::Cow(checksums), key_id)
    }

    /// Creates an ExtentValue for an overwrite range with no blocks written to yet.
    pub fn blank_overwrite_extent(
        device_offset: u64,
        num_blocks: usize,
        key_id: u64,
    ) -> ExtentValue {
        Self::new(
            device_offset,
            ExtentMode::OverwritePartial(BitVec::from_elem(num_blocks, false)),
            key_id,
        )
    }

    /// Creates an ExtentValue for an overwrite range with all the blocks initialized.
    pub fn initialized_overwrite_extent(device_offset: u64, key_id: u64) -> ExtentValue {
        Self::new(device_offset, ExtentMode::Overwrite, key_id)
    }

    /// Creates an ObjectValue for a deletion of an object extent.
    pub fn deleted_extent() -> ExtentValue {
        ExtentValue::None
    }

    pub fn is_deleted(&self) -> bool {
        if let ExtentValue::None = self { true } else { false }
    }

    /// Returns a new ExtentValue offset by `amount`.  Both `amount` and `extent_len` must be
    /// multiples of the underlying block size.
    pub fn offset_by(&self, amount: u64, extent_len: u64) -> Self {
        match self {
            ExtentValue::None => Self::deleted_extent(),
            ExtentValue::Some { device_offset, mode, key_id } => {
                let mode = match mode {
                    ExtentMode::Raw => ExtentMode::Raw,
                    ExtentMode::Cow(checksums) => {
                        if checksums.len() > 0 {
                            let index = (amount / (extent_len / checksums.len() as u64)) as usize;
                            ExtentMode::Cow(checksums.offset_by(index))
                        } else {
                            ExtentMode::Cow(Checksums::fletcher(Vec::new()))
                        }
                    }
                    ExtentMode::Overwrite => ExtentMode::Overwrite,
                    ExtentMode::OverwritePartial(bitmap) => {
                        debug_assert!(bitmap.len() > 0);
                        let index = (amount / (extent_len / bitmap.len() as u64)) as usize;
                        ExtentMode::OverwritePartial(bitmap.clone().split_off(index))
                    }
                };
                ExtentValue::Some { device_offset: device_offset + amount, mode, key_id: *key_id }
            }
        }
    }

    /// Returns a new ExtentValue after shrinking the extent from |original_len| to |new_len|.
    pub fn shrunk(&self, original_len: u64, new_len: u64) -> Self {
        match self {
            ExtentValue::None => Self::deleted_extent(),
            ExtentValue::Some { device_offset, mode, key_id } => {
                let mode = match mode {
                    ExtentMode::Raw => ExtentMode::Raw,
                    ExtentMode::Cow(checksums) => {
                        if checksums.len() > 0 {
                            let len = (new_len / (original_len / checksums.len() as u64)) as usize;
                            ExtentMode::Cow(checksums.shrunk(len))
                        } else {
                            ExtentMode::Cow(Checksums::fletcher(Vec::new()))
                        }
                    }
                    ExtentMode::Overwrite => ExtentMode::Overwrite,
                    ExtentMode::OverwritePartial(bitmap) => {
                        debug_assert!(bitmap.len() > 0);
                        let len = (new_len / (original_len / bitmap.len() as u64)) as usize;
                        let mut new_bitmap = bitmap.clone();
                        new_bitmap.truncate(len);
                        ExtentMode::OverwritePartial(new_bitmap)
                    }
                };
                ExtentValue::Some { device_offset: *device_offset, mode, key_id: *key_id }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ExtentMode, ExtentValue};
    use crate::checksum::Checksums;
    use crate::object_store::VOLUME_DATA_KEY_ID;
    use bit_vec::BitVec;

    #[test]
    fn extent_value_offset_by() {
        assert_eq!(ExtentValue::None.offset_by(1024, 2048), ExtentValue::None);
        assert_eq!(
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID).offset_by(0, 2048),
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID).offset_by(1024, 2048),
            ExtentValue::new_raw(2048, VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID).offset_by(2048, 2048),
            ExtentValue::new_raw(3072, VOLUME_DATA_KEY_ID)
        );

        let make_checksums = |range: std::ops::Range<u64>| Checksums::fletcher(range.collect());

        // In these tests we are making block size 256.
        assert_eq!(
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
                .offset_by(0, 2048),
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
                .offset_by(1024, 2048),
            ExtentValue::with_checksum(2048, make_checksums(4..8), VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
                .offset_by(2048, 2048),
            ExtentValue::with_checksum(3072, Checksums::fletcher(Vec::new()), VOLUME_DATA_KEY_ID)
        );

        // Takes a place to switch from zeros to ones. The goal is to make sure there is exactly
        // one zero and then only ones where we expect offset to slice.
        let make_bitmap = |cut, length| {
            let mut begin_bitmap = BitVec::from_elem(cut, false);
            let mut end_bitmap = BitVec::from_elem(length - cut, true);
            begin_bitmap.append(&mut end_bitmap);
            ExtentMode::OverwritePartial(begin_bitmap)
        };
        let make_extent =
            |device_offset, mode| ExtentValue::Some { device_offset, mode, key_id: 0 };

        assert_eq!(
            make_extent(1024, make_bitmap(1, 8)).offset_by(0, 2048),
            make_extent(1024, make_bitmap(1, 8))
        );
        assert_eq!(
            make_extent(1024, make_bitmap(5, 8)).offset_by(1024, 2048),
            make_extent(2048, make_bitmap(1, 4))
        );
        assert_eq!(
            make_extent(1024, make_bitmap(0, 8)).offset_by(2048, 2048),
            make_extent(3072, ExtentMode::OverwritePartial(BitVec::new()))
        );

        assert_eq!(
            make_extent(1024, ExtentMode::Overwrite).offset_by(0, 2048),
            make_extent(1024, ExtentMode::Overwrite)
        );
        assert_eq!(
            make_extent(1024, ExtentMode::Overwrite).offset_by(1024, 2048),
            make_extent(2048, ExtentMode::Overwrite)
        );
        assert_eq!(
            make_extent(1024, ExtentMode::Overwrite).offset_by(2048, 2048),
            make_extent(3072, ExtentMode::Overwrite)
        );
    }

    #[test]
    fn extent_value_shrunk() {
        assert_eq!(ExtentValue::None.shrunk(2048, 1024), ExtentValue::None);
        assert_eq!(
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID).shrunk(2048, 2048),
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID).shrunk(2048, 1024),
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID).shrunk(2048, 0),
            ExtentValue::new_raw(1024, VOLUME_DATA_KEY_ID)
        );

        let make_checksums = |range: std::ops::Range<u64>| Checksums::fletcher(range.collect());

        // In these tests we are making block size 256.
        assert_eq!(
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
                .shrunk(2048, 2048),
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
                .shrunk(2048, 1024),
            ExtentValue::with_checksum(1024, make_checksums(0..4), VOLUME_DATA_KEY_ID)
        );
        assert_eq!(
            ExtentValue::with_checksum(1024, make_checksums(0..8), VOLUME_DATA_KEY_ID)
                .shrunk(2048, 0),
            ExtentValue::with_checksum(1024, Checksums::fletcher(Vec::new()), VOLUME_DATA_KEY_ID)
        );

        // Takes a place to switch from zeros to ones. The goal is to make sure there is exactly
        // one zero and then only ones where we expect offset to slice.
        let make_bitmap = |cut, length| {
            let mut begin_bitmap = BitVec::from_elem(cut, false);
            let mut end_bitmap = BitVec::from_elem(length - cut, true);
            begin_bitmap.append(&mut end_bitmap);
            ExtentMode::OverwritePartial(begin_bitmap)
        };
        let make_extent =
            |device_offset, mode| ExtentValue::Some { device_offset, mode, key_id: 0 };

        assert_eq!(
            make_extent(1024, make_bitmap(1, 8)).shrunk(2048, 2048),
            make_extent(1024, make_bitmap(1, 8))
        );
        assert_eq!(
            make_extent(1024, make_bitmap(3, 8)).shrunk(2048, 1024),
            make_extent(1024, make_bitmap(3, 4))
        );
        assert_eq!(
            make_extent(1024, make_bitmap(0, 8)).shrunk(2048, 0),
            make_extent(1024, ExtentMode::OverwritePartial(BitVec::new()))
        );

        assert_eq!(
            make_extent(1024, ExtentMode::Overwrite).shrunk(2048, 2048),
            make_extent(1024, ExtentMode::Overwrite)
        );
        assert_eq!(
            make_extent(1024, ExtentMode::Overwrite).shrunk(2048, 1024),
            make_extent(1024, ExtentMode::Overwrite)
        );
        assert_eq!(
            make_extent(1024, ExtentMode::Overwrite).shrunk(2048, 0),
            make_extent(1024, ExtentMode::Overwrite)
        );
    }
}
