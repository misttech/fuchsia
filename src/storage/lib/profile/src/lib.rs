// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared profile message types and block-aligned VMO encoding/decoding used across Fxfs,
//! `blob_pager_and_verifier`, and `mapping` / `block_server`.

use arrayref::{array_refs, mut_array_refs};
use fuchsia_hash::Hash;
use std::mem::size_of;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};

/// Sentinel offset used by Fxfs to record that a file or blob was opened during a recording window.
pub const FILE_OPEN_MARKER: u64 = u64::MAX;

/// A fixed-size profile entry recording an access to `(id, offset)`.
pub trait Message:
    Eq + PartialEq + Sized + Send + Sync + std::hash::Hash + Copy + Clone + 'static
{
    type IdType: std::fmt::Display + Ord + Send + Sized + Copy + Clone;

    fn id(&self) -> Self::IdType;
    fn offset(&self) -> u64;
    fn encode_to(&self, dest: MutPtrByteSlice<'_>);
    fn decode_from(src: PtrByteSlice<'_>) -> Self;
    fn is_zeroes(&self) -> bool;

    fn is_open_marker(&self) -> bool {
        self.offset() == FILE_OPEN_MARKER
    }

    // Encodes `messages` into a new `zx::Vmo` packed into `block_size`-aligned blocks (typically
    // 4 KiB) without straddling `block_size` boundaries. Any trailing bytes in a block that cannot
    // fit a full message are zero-padded.
    fn encode_to_vmo(messages: &[Self], block_size: usize) -> Result<zx::Vmo, zx::Status> {
        let msg_size = size_of::<Self>();
        if messages.is_empty() || msg_size == 0 || block_size < msg_size {
            return zx::Vmo::create(0);
        }

        let entries_per_block = block_size / msg_size;
        let num_blocks = messages.len().div_ceil(entries_per_block);
        let total_size = num_blocks * block_size;
        let mut buf = vec![0u8; total_size];

        for (block, chunk) in
            buf.chunks_exact_mut(block_size).zip(messages.chunks(entries_per_block))
        {
            for (dest, msg) in block.chunks_exact_mut(msg_size).zip(chunk) {
                msg.encode_to(MutPtrByteSlice::from(dest));
            }
        }

        let vmo = zx::Vmo::create(total_size as u64)?;
        vmo.write(&buf, 0)?;
        Ok(vmo)
    }

    // Decodes messages from `vmo` formatted with `block_size`-aligned blocks. An all-zero message
    // (`Self::is_zeroes()`) marks the end of the recorded stream.
    fn decode_from_vmo(vmo: &zx::Vmo, block_size: usize) -> Result<Vec<Self>, zx::Status> {
        let size = vmo.get_size()? as usize;
        let msg_size = size_of::<Self>();
        if size == 0 || msg_size == 0 || block_size < msg_size {
            return Ok(Vec::new());
        }

        let mut buf = vec![0u8; size];
        vmo.read(&mut buf, 0)?;

        let mut messages = Vec::new();
        for block in buf.chunks(block_size) {
            for entry_bytes in block.chunks_exact(msg_size) {
                let msg = Self::decode_from(PtrByteSlice::from(entry_bytes));
                if msg.is_zeroes() {
                    return Ok(messages);
                }
                messages.push(msg);
            }
        }
        Ok(messages)
    }
}

/// A 40-byte profile entry identifying a blob by its 32-byte Merkle root [`Hash`] and a logical
/// byte `offset`.
#[derive(Copy, Clone, Debug, Eq, std::hash::Hash, PartialEq)]
pub struct BlobMessage {
    pub id: Hash,
    /// Readahead-aligned logical byte offset within the blob. Only the start offset (without
    /// length) is stored because replay prefetches a single page at `offset` and relies on the
    /// pager's readahead to fault in the full window.
    pub offset: u64,
}

impl Message for BlobMessage {
    type IdType = Hash;

    fn id(&self) -> Self::IdType {
        self.id
    }

    fn offset(&self) -> u64 {
        self.offset
    }

    fn encode_to(&self, mut dest: MutPtrByteSlice<'_>) {
        let mut buf = [0u8; size_of::<Self>()];
        let (first, second) = mut_array_refs![&mut buf, size_of::<Hash>(), size_of::<u64>()];
        *first = self.id.into();
        *second = self.offset.to_le_bytes();
        dest.copy_from_slice(&buf);
    }

    fn decode_from(src: PtrByteSlice<'_>) -> Self {
        let mut buf = [0u8; size_of::<Self>()];
        src.copy_to_slice(&mut buf);
        let (first, second) = array_refs!(&buf, size_of::<Hash>(), size_of::<u64>());
        Self { id: Hash::from_array(*first), offset: u64::from_le_bytes(*second) }
    }

    fn is_zeroes(&self) -> bool {
        self.id == Hash::from_array([0u8; size_of::<Hash>()]) && self.offset == 0
    }
}

/// A 16-byte profile entry identifying a file by a numeric `id` (Fxfs object ID or mapper session
/// `key`) and a logical byte `offset`.
#[derive(Copy, Clone, Debug, Eq, std::hash::Hash, PartialEq)]
pub struct FileMessage {
    pub id: u64,
    /// Readahead-aligned logical byte offset within the file. Only the start offset (without
    /// length) is stored because replay prefetches a single page at `offset` and relies on the
    /// pager's readahead to fault in the full window.
    pub offset: u64,
}

impl Message for FileMessage {
    type IdType = u64;

    fn id(&self) -> Self::IdType {
        self.id
    }

    fn offset(&self) -> u64 {
        self.offset
    }

    fn encode_to(&self, mut dest: MutPtrByteSlice<'_>) {
        let mut buf = [0u8; size_of::<Self>()];
        let (first, second) = mut_array_refs![&mut buf, size_of::<u64>(), size_of::<u64>()];
        *first = self.id.to_le_bytes();
        *second = self.offset.to_le_bytes();
        dest.copy_from_slice(&buf);
    }

    fn decode_from(src: PtrByteSlice<'_>) -> Self {
        let mut buf = [0u8; size_of::<Self>()];
        src.copy_to_slice(&mut buf);
        let (first, second) = array_refs!(&buf, size_of::<u64>(), size_of::<u64>());
        Self { id: u64::from_le_bytes(*first), offset: u64::from_le_bytes(*second) }
    }

    fn is_zeroes(&self) -> bool {
        self.id == 0 && self.offset == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK_SIZE: usize = 4096;

    #[fuchsia::test]
    fn test_encode_decode_blob() {
        let mut buf = [0u8; size_of::<BlobMessage>()];
        let m = BlobMessage { id: [88u8; 32].into(), offset: 77 };
        m.encode_to(MutPtrByteSlice::from(&mut buf[..]));
        let m2 = BlobMessage::decode_from(PtrByteSlice::from(&buf[..]));
        assert!(!m2.is_zeroes());
        assert_eq!(m, m2);
    }

    #[fuchsia::test]
    fn test_encode_decode_file() {
        let mut buf = [0u8; size_of::<FileMessage>()];
        let m = FileMessage { id: 88, offset: 77 };
        m.encode_to(MutPtrByteSlice::from(&mut buf[..]));
        let m2 = FileMessage::decode_from(PtrByteSlice::from(&buf[..]));
        assert!(!m2.is_zeroes());
        assert_eq!(m, m2);
    }

    #[fuchsia::test]
    fn test_file_message_vmo_round_trip() {
        let empty_vmo = FileMessage::encode_to_vmo(&[], BLOCK_SIZE).expect("encode_to_vmo failed");
        assert_eq!(
            FileMessage::decode_from_vmo(&empty_vmo, BLOCK_SIZE).expect("decode_from_vmo failed"),
            Vec::<FileMessage>::new()
        );

        // Exact block multiple (256 * 16 = 4096 bytes) with no trailing zeroes.
        let exact_block: Vec<FileMessage> =
            (0..256).map(|i| FileMessage { id: (i % 7) + 1, offset: i * 4096 }).collect();
        let exact_vmo =
            FileMessage::encode_to_vmo(&exact_block, BLOCK_SIZE).expect("encode_to_vmo failed");
        assert_eq!(
            FileMessage::decode_from_vmo(&exact_vmo, BLOCK_SIZE).expect("decode_from_vmo failed"),
            exact_block
        );

        // Multi-block with partial second block (260 > 256 entries per 4 KiB block).
        let messages: Vec<FileMessage> =
            (0..260).map(|i| FileMessage { id: (i % 7) + 1, offset: i * 4096 }).collect();
        let vmo = FileMessage::encode_to_vmo(&messages, BLOCK_SIZE).expect("encode_to_vmo failed");
        let decoded =
            FileMessage::decode_from_vmo(&vmo, BLOCK_SIZE).expect("decode_from_vmo failed");
        assert_eq!(decoded, messages);
    }

    #[fuchsia::test]
    fn test_blob_message_vmo_round_trip_across_blocks() {
        // 102 * 40 = 4080 bytes per 4 KiB block (with 16 bytes zero padding at the end of block 0).
        // Test 105 messages so we cross the 4 KiB block boundary and verify the 16-byte padding at
        // the end of the first block is skipped rather than misinterpreted as an end-of-profile
        // zero entry.
        let messages: Vec<BlobMessage> = (0..105)
            .map(|i| BlobMessage { id: [(i as u8) + 1; 32].into(), offset: (i as u64) * 131072 })
            .collect();
        let vmo = BlobMessage::encode_to_vmo(&messages, BLOCK_SIZE).expect("encode_to_vmo failed");
        assert_eq!(vmo.get_size().expect("get_size failed"), (2 * BLOCK_SIZE) as u64);

        let decoded =
            BlobMessage::decode_from_vmo(&vmo, BLOCK_SIZE).expect("decode_from_vmo failed");
        assert_eq!(decoded, messages);
    }
}
