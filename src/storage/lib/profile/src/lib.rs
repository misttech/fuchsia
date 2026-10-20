// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Shared profile message types and block-aligned VMO encoding/decoding used across Fxfs,
//! `blob_pager_and_verifier`, and `mapping` / `block_server`.

use arrayref::{array_refs, mut_array_refs};
use fuchsia_hash::Hash;
use std::borrow::Borrow;
use std::marker::PhantomData;
use std::mem::size_of;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};

/// Profile name used for startup profile recording and replay.
pub const BOOT_PROFILE_NAME: &str = ".boot";

/// Sentinel offset used by Fxfs to record that a file or blob was opened during a recording window.
pub const FILE_OPEN_MARKER: u64 = u64::MAX;

/// Upper bound on profile VMO size to prevent unbounded memory growth.
pub const MAX_PROFILE_VMO_SIZE: u64 = 64 * 1024 * 1024;

// Streams [`Message`] entries into a resizable `zx::Vmo` in `block_size`-aligned chunks without
// crossing block boundaries.
struct VmoWriter {
    vmo: zx::Vmo,
    vmo_offset: u64,
    block_size: usize,
    buffer: Vec<u8>,
}

impl VmoWriter {
    fn new(block_size: usize) -> Result<Self, zx::Status> {
        let vmo = zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, 0)?;
        Ok(Self { vmo, vmo_offset: 0, block_size, buffer: Vec::with_capacity(block_size) })
    }

    fn write<M: Message>(&mut self, msg: &M) -> Result<(), zx::Status> {
        let msg_size = size_of::<M>();
        if msg_size == 0 || self.block_size < msg_size {
            return Ok(());
        }
        // Profiles are best-effort prefetch hints, so once the VMO reaches `MAX_PROFILE_VMO_SIZE`,
        // drop additional entries to cap memory usage rather than failing and discarding the entire
        // profile.
        if self.vmo_offset + self.block_size as u64 > MAX_PROFILE_VMO_SIZE {
            return Ok(());
        }
        let start = self.buffer.len();
        self.buffer.resize(start + msg_size, 0);
        msg.encode_to(MutPtrByteSlice::from(&mut self.buffer[start..]));
        // Flush if another message cannot fit in this block.
        if self.buffer.len() + msg_size > self.block_size {
            self.flush()?;
        }
        Ok(())
    }

    fn flush(&mut self) -> Result<(), zx::Status> {
        let new_size = self.vmo_offset + self.block_size as u64;
        let res = self.vmo.set_size(new_size).and_then(|()| {
            self.vmo.write(&self.buffer, self.vmo_offset)?;
            self.vmo_offset = new_size;
            Ok(())
        });
        self.buffer.clear();
        res
    }

    fn finish(mut self) -> Result<zx::Vmo, zx::Status> {
        if !self.buffer.is_empty() {
            self.flush()?;
        }
        Ok(self.vmo)
    }
}

/// Reads and decodes [`Message`] entries from a `zx::Vmo` one block at a time.
#[derive(Debug)]
pub struct DecodedMessageIter<M> {
    vmo: zx::Vmo,
    vmo_size: u64,
    vmo_offset: u64,
    block_size: usize,
    block_buf: Vec<u8>,
    block_offset: usize,
    _marker: PhantomData<M>,
}

impl<M: Message> DecodedMessageIter<M> {
    fn new(vmo: zx::Vmo, block_size: usize) -> Result<Self, zx::Status> {
        let vmo_size = vmo.get_size()?;
        if vmo_size > MAX_PROFILE_VMO_SIZE {
            return Err(zx::Status::OUT_OF_RANGE);
        }
        Ok(Self {
            vmo,
            vmo_size,
            vmo_offset: 0,
            block_size,
            block_buf: Vec::with_capacity(block_size),
            block_offset: 0,
            _marker: PhantomData,
        })
    }
}

impl<M: Message> Iterator for DecodedMessageIter<M> {
    type Item = M;

    fn next(&mut self) -> Option<Self::Item> {
        let msg_size = size_of::<M>();
        if msg_size == 0 || self.block_size < msg_size {
            return None;
        }
        if self.block_offset + msg_size > self.block_buf.len() {
            if self.vmo_offset + self.block_size as u64 > self.vmo_size {
                return None;
            }
            self.block_buf.resize(self.block_size, 0);
            if self.vmo.read(&mut self.block_buf, self.vmo_offset).is_err() {
                return None;
            }
            self.vmo_offset += self.block_size as u64;
            self.block_offset = 0;
        }
        let entry_bytes = &self.block_buf[self.block_offset..self.block_offset + msg_size];
        self.block_offset += msg_size;
        let msg = M::decode_from(PtrByteSlice::from(entry_bytes));
        // Trailing zero-padding in the final block marks the end of the recorded entries.
        if msg.is_zeroes() {
            self.vmo_offset = self.vmo_size;
            return None;
        }
        Some(msg)
    }
}

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

    /// Encodes `messages` into a new `zx::Vmo` packed into `block_size`-aligned blocks (typically
    /// 4 KiB) without crossing `block_size` boundaries. Any trailing bytes in a block that cannot
    /// fit a full message are zero-padded.
    fn encode_to_vmo(
        messages: impl IntoIterator<Item = impl Borrow<Self>>,
        block_size: usize,
    ) -> Result<zx::Vmo, zx::Status> {
        let mut writer = VmoWriter::new(block_size)?;
        for msg in messages {
            writer.write(msg.borrow())?;
        }
        writer.finish()
    }

    /// Returns an iterator that lazily decodes messages from `vmo` one `block_size`-aligned block
    /// at a time. An all-zero message (`Self::is_zeroes()`) marks the end of the recorded stream.
    fn decode_from_vmo(
        vmo: zx::Vmo,
        block_size: usize,
    ) -> Result<DecodedMessageIter<Self>, zx::Status> {
        DecodedMessageIter::new(vmo, block_size)
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
        let empty_vmo = FileMessage::encode_to_vmo(&[] as &[FileMessage], BLOCK_SIZE)
            .expect("encode_to_vmo failed");
        assert_eq!(
            FileMessage::decode_from_vmo(empty_vmo, BLOCK_SIZE)
                .expect("decode_from_vmo failed")
                .collect::<Vec<_>>(),
            Vec::<FileMessage>::new()
        );

        // Exact block multiple (256 * 16 = 4096 bytes) with no trailing zeroes.
        let exact_block: Vec<FileMessage> =
            (0..256).map(|i| FileMessage { id: (i % 7) + 1, offset: i * 4096 }).collect();
        let exact_vmo =
            FileMessage::encode_to_vmo(&exact_block, BLOCK_SIZE).expect("encode_to_vmo failed");
        assert_eq!(
            FileMessage::decode_from_vmo(exact_vmo, BLOCK_SIZE)
                .expect("decode_from_vmo failed")
                .collect::<Vec<_>>(),
            exact_block
        );

        // Multi-block with partial second block (260 > 256 entries per 4 KiB block).
        let messages: Vec<FileMessage> =
            (0..260).map(|i| FileMessage { id: (i % 7) + 1, offset: i * 4096 }).collect();
        let vmo = FileMessage::encode_to_vmo(&messages, BLOCK_SIZE).expect("encode_to_vmo failed");
        let decoded: Vec<_> = FileMessage::decode_from_vmo(vmo, BLOCK_SIZE)
            .expect("decode_from_vmo failed")
            .collect();
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

        let decoded: Vec<_> = BlobMessage::decode_from_vmo(vmo, BLOCK_SIZE)
            .expect("decode_from_vmo failed")
            .collect();
        assert_eq!(decoded, messages);
    }
}
