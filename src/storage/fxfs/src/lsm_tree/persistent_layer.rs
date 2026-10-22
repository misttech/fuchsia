// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// PersistentLayer object format
//
// The layer is made up of 1 or more "blocks" whose size are some multiple of the block size used
// by the underlying handle.
//
// The persistent layer has 4 types of blocks:
//  - Header block
//  - Data block
//  - BloomFilter block
//  - Seek block (+LayerInfo)
//
// The structure of the file is as follows:
//
// blk#     contents
// 0        [Header]
// 1        [Data]
// 2        [Data]
// ...      [Data]
// L        [BloomFilter]
// L + 1    [BloomFilter]
// ...      [BloomFilter]
// M        [Seek]
// M + 1    [Seek]
// ...      [Seek]
// N        [Seek/LayerInfo]
//
// Generally, there will be an order of magnitude more Data blocks than Seek/BloomFilter blocks.
//
// Header contains a Version-prefixed LayerHeader struct.  This version is used for everything in
// the layer file.
//
// Data blocks contain a little endian encoded u16 item count at the start, then a series of
// serialized items, and a list of little endian u16 offsets within the block for where
// serialized items start, excluding the first item (since it is at a known offset). The list of
// offsets ends at the end of the block and since the items are of variable length, there may be
// space between the two sections if the next item and its offset cannot fit into the block.
//
// |item_count|item|item|item|item|item|item|dead space|offset|offset|offset|offset|offset|
//
// BloomFilter blocks contain a bitmap which is used to probabilistically determine if a given key
// might exist in the layer file.   See `BloomFilter` for details on this structure.  Note that this
// can be absent from the file if the layer is empty.
//
// Seek/LayerInfo blocks contain both the seek table, and a single LayerInfo struct at the tail of
// the last block, with the LayerInfo's length written as a little-endian u64 at the very end.  The
// padding between the two structs is ignored but nominally is zeroed. They share blocks to avoid
// wasting padding bytes.  Note that the seek table can be absent from the file for small layer
// files (but there will always be one block for the LayerInfo).
//
// The seek table contains a little-endian u64 for each data block, recording the leading u64 of
// that block's first key. Entries are in sorted order (duplicates are possible when an object
// spans multiple blocks).

use crate::drop_event::DropEvent;
use crate::errors::FxfsError;
use crate::filesystem::MAX_BLOCK_SIZE;
use crate::log::*;
use crate::lsm_tree::bloom_filter::{BloomFilterReader, BloomFilterStats, BloomFilterWriter};
use crate::lsm_tree::types::{
    BoxedLayerIterator, Existence, FuzzyHash, Item, ItemRef, Key, Layer, LayerIterator, LayerValue,
    LayerWriter, MaybeContainsKey,
};
use crate::object_handle::{LayerObject, ObjectHandle, ReadObjectHandle, WriteBytes};
use crate::object_store::caching_object_handle::{CHUNK_SIZE, CachedChunk, CachingObjectHandle};
use crate::object_store::extent::MIN_BLOCK_SIZE;
use crate::object_store::{DataObjectHandle, HandleOptions, ObjectStore};
use crate::serialized_types::serialized_key::{KeyDeserializer, compare_keys};
use crate::serialized_types::{
    LATEST_VERSION, OLD_KEY_SERIALIZATION_VERSION, Version, Versioned, VersionedLatest,
};
use anyhow::{Context, Error, anyhow, ensure};
use async_trait::async_trait;
use byteorder::{ByteOrder, LittleEndian, ReadBytesExt, WriteBytesExt};
use fprint::TypeFingerprint;
use fuchsia_sync::Mutex;
use futures::future::BoxFuture;
use futures::stream::{FuturesUnordered, TryStreamExt};
use fxfs_crypto::{Crypt, UnwrappedKey, WrappedKey};
use serde::{Deserialize, Serialize};
use static_assertions::const_assert;
use std::cmp::Ordering;
use std::io::{Read as _, Write as _};
use std::marker::PhantomData;
use std::ops::Bound;
use std::sync::Arc;
use storage_ptr_slice::PtrByteSlice;
use storage_units::BlockSize;

const PERSISTENT_LAYER_MAGIC: &[u8; 8] = b"FxfsLayr";

/// LayerHeader is stored in the first block of the persistent layer.
pub type LayerHeader = LayerHeaderV39;

#[derive(Debug, Serialize, Deserialize, TypeFingerprint, Versioned)]
pub struct LayerHeaderV39 {
    /// 'FxfsLayr'
    magic: [u8; 8],
    /// The block size used within this layer file. This is typically set at compaction time to the
    /// same block size as the underlying object handle.
    ///
    /// (Each block starts with a 2 byte item count so there is a 64k item limit per block,
    /// regardless of block size).
    block_size: u64,
}

/// The last block of each layer contains metadata for the rest of the layer.
pub type LayerInfo = LayerInfoV39;

#[derive(Debug, Serialize, Deserialize, TypeFingerprint, Versioned)]
pub struct LayerInfoV39 {
    /// How many items are in the layer file.  Mainly used for sizing bloom filters during
    /// compaction.
    num_items: usize,
    /// The number of data blocks in the layer file.
    num_data_blocks: u64,
    /// The size of the bloom filter in the layer file.  Not necessarily block-aligned.
    bloom_filter_size_bytes: usize,
    /// The seed for the nonces used in the bloom filter.
    bloom_filter_seed: u64,
    /// How many nonces to use for bloom filter hashing.
    bloom_filter_num_hashes: usize,
}

struct LayerData<K> {
    object_id: u64,
    version: Version,
    block_size: BlockSize,
    data_size: u64,
    seek_table: Vec<u64>,
    num_items: usize,
    bloom_filter: Option<BloomFilterReader<K>>,
    bloom_filter_stats: Option<BloomFilterStats>,
    close_event: Mutex<Option<Arc<DropEvent>>>,
}

impl<K> LayerData<K> {
    fn data_offset(&self) -> u64 {
        NUM_HEADER_BLOCKS * self.block_size
    }
}

/// A handle to an asynchronous, chunk-cached persistent layer.
pub struct PersistentLayer<K, V> {
    object_handle: CachingObjectHandle<Arc<dyn LayerObject>>,
    data: LayerData<K>,
    _value_type: PhantomData<V>,
}

/// A handle to a synchronous, slice-backed persistent layer.
pub struct SyncPersistentLayer<K, V> {
    object_handle: Arc<dyn LayerObject>,
    data: LayerData<K>,
    _value_type: PhantomData<V>,
}

struct BufferCursor<B> {
    buffer: B,
    pos: usize,
}

impl<B: LayerBuffer> BufferCursor<B> {
    fn as_bytes(&self) -> &[u8] {
        self.buffer.as_bytes_from(self.pos)
    }
}

impl<B: LayerBuffer> std::io::Read for BufferCursor<B> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let to_read = self.buffer.read_at(self.pos, buf);
        self.pos += to_read;
        Ok(to_read)
    }
}

trait LayerBuffer {
    fn read_at(&self, pos: usize, buf: &mut [u8]) -> usize;

    fn as_bytes_from(&self, pos: usize) -> &[u8];

    fn has_io_error(&self) -> bool {
        false
    }
}

struct ChunkBuffer<'iter> {
    handle: &'iter CachingObjectHandle<Arc<dyn LayerObject>>,
    // The currently loaded chunk and its index.
    chunk: Option<(usize, CachedChunk)>,
}

impl LayerBuffer for ChunkBuffer<'_> {
    fn read_at(&self, pos: usize, buf: &mut [u8]) -> usize {
        let Some((_, chunk)) = &self.chunk else {
            return 0;
        };
        let to_read = std::cmp::min(buf.len(), chunk.len().saturating_sub(pos));
        if to_read > 0 {
            buf[..to_read].copy_from_slice(&chunk[pos..pos + to_read]);
        }
        to_read
    }

    fn as_bytes_from(&self, pos: usize) -> &[u8] {
        self.chunk.as_ref().and_then(|(_, c)| c.get(pos..)).unwrap_or(&[])
    }
}

#[derive(Clone, Copy)]
struct SliceBuffer<'iter> {
    handle: &'iter dyn LayerObject,
    slice: &'iter [u8],
}

impl LayerBuffer for SliceBuffer<'_> {
    fn read_at(&self, pos: usize, buf: &mut [u8]) -> usize {
        let to_read = std::cmp::min(buf.len(), self.slice.len().saturating_sub(pos));
        if to_read > 0 {
            buf[..to_read].copy_from_slice(&self.slice[pos..pos + to_read]);
        }
        to_read
    }

    fn as_bytes_from(&self, pos: usize) -> &[u8] {
        self.slice.get(pos..).unwrap_or(&[])
    }

    fn has_io_error(&self) -> bool {
        self.handle.has_io_error()
    }
}

// How many blocks we reserve for the header.  Data blocks start at this offset.
const NUM_HEADER_BLOCKS: u64 = 1;

/// The smallest possible (empty) layer file is always 2 blocks, one for the header and one for
/// LayerInfo.
const MINIMUM_LAYER_FILE_BLOCKS: u64 = 2;

// Put safety rails on the size of the bloom filter and seek table to avoid OOMing the system.
// It's more likely that tampering has occurred in these cases.
const MAX_BLOOM_FILTER_SIZE: usize = 64 * 1024 * 1024;
const MAX_SEEK_TABLE_SIZE: usize = 64 * 1024 * 1024;

// The following constants refer to sizes of metadata in the data blocks.
const PER_DATA_BLOCK_HEADER_SIZE: usize = 2;
const PER_DATA_BLOCK_SEEK_ENTRY_SIZE: usize = 2;

enum KeyState<K> {
    None,
    Deserialized(K),
    InPlace,
}

// A key-only iterator, used while seeking through the tree.
struct KeyOnlyIterator<'iter, K: Key, V: LayerValue, B> {
    // Allocated out of |layer|.
    buffer: BufferCursor<B>,

    layer: &'iter LayerData<K>,

    // The position of the _next_ block to be read.
    pos: u64,

    // The item index in the current block.
    item_index: u16,

    // The number of items in the current block.
    item_count: u16,

    // The current key state.
    key: KeyState<K>,

    // The base value used for decoding keys in the current block.
    current_block_base_u64: u64,

    _value_type: PhantomData<V>,
}

impl<K: Key, V: LayerValue, B: LayerBuffer> KeyOnlyIterator<'_, K, V, B> {
    fn corruption_error(&self, err: impl std::fmt::Display + Send + Sync + 'static) -> Error {
        if self.buffer.buffer.has_io_error() {
            anyhow!(zx_status::Status::IO).context(err)
        } else {
            anyhow!(FxfsError::Inconsistent).context(err)
        }
    }

    // Repositions the iterator to point to the `index`'th item in the current block.
    // Returns an error if the index is out of range or the resulting offset contains an obviously
    // invalid value.
    fn seek_to_block_item(&mut self, index: u16) -> Result<(), Error> {
        ensure!(index < self.item_count, FxfsError::OutOfRange);
        if index == self.item_index && matches!(self.key, KeyState::None) {
            // Fast-path when we are seeking in a linear manner, as is the case when advancing a
            // wrapping iterator that also deserializes the values.
            return Ok(());
        }
        let block_start =
            self.layer.block_size.align_down((self.buffer.pos as u64).saturating_sub(1)) as usize;
        let offset_in_block = if index == 0 {
            // First entry isn't actually recorded, it is at the start of the block after the item
            // count.
            PER_DATA_BLOCK_HEADER_SIZE
        } else {
            let old_buffer_pos = self.buffer.pos;
            let seek_entry_pos = (block_start + self.layer.block_size.get() as usize)
                .checked_sub(PER_DATA_BLOCK_SEEK_ENTRY_SIZE * usize::from(self.item_count - index))
                .ok_or_else(|| {
                    self.corruption_error(format!(
                        "Invalid item count {} for index {index}",
                        self.item_count
                    ))
                })?;
            if seek_entry_pos < block_start + PER_DATA_BLOCK_HEADER_SIZE {
                return Err(self.corruption_error(format!(
                    "Invalid item count {} for index {index}",
                    self.item_count
                )));
            }
            self.buffer.pos = seek_entry_pos;
            let res = self.buffer.read_u16::<LittleEndian>();
            self.buffer.pos = old_buffer_pos;
            let offset_in_block = res
                .map_err(|e| self.corruption_error(e))
                .context("Failed to read offset")? as usize;
            if offset_in_block >= self.layer.block_size.get() as usize
                || offset_in_block <= PER_DATA_BLOCK_HEADER_SIZE
            {
                return Err(self
                    .corruption_error(format!("Offset {offset_in_block} is out of valid range.")));
            }
            offset_in_block
        };
        self.item_index = index;
        self.key = KeyState::None;
        self.buffer.pos = block_start + offset_in_block;
        Ok(())
    }

    fn take_item(&mut self) -> Result<Option<Item<K, V>>, Error> {
        let key = match std::mem::replace(&mut self.key, KeyState::None) {
            KeyState::None => return Ok(None),
            KeyState::InPlace => {
                let (mut deserializer, key_len) =
                    KeyDeserializer::new(self.buffer.as_bytes(), Some(self.current_block_base_u64))
                        .map_err(|e| self.corruption_error(e))
                        .context("Corrupt layer (key format)")?;
                let key = K::deserialize_key_from(&mut deserializer)
                    .map_err(|e| self.corruption_error(e))
                    .context("Corrupt layer (key)")?;
                if !deserializer.is_empty() {
                    return Err(self.corruption_error("Trailing bytes in serialized key"));
                }
                self.buffer.pos += key_len;
                key
            }
            KeyState::Deserialized(key) => key,
        };
        let value = V::deserialize_from_version(self.buffer.by_ref(), self.layer.version)
            .map_err(|e| self.corruption_error(e))
            .context("Corrupt layer (value)")?;
        Ok(Some(Item { key, value }))
    }

    fn read_block_header(&mut self) -> Result<(), Error> {
        self.item_count =
            self.buffer.read_u16::<LittleEndian>().map_err(|e| self.corruption_error(e))?;
        if self.item_count == 0 {
            return Err(self.corruption_error(format!(
                "Read block with zero item count (object: {}, offset: {})",
                self.layer.object_id, self.pos
            )));
        }
        if PER_DATA_BLOCK_HEADER_SIZE
            + (usize::from(self.item_count) - 1) * PER_DATA_BLOCK_SEEK_ENTRY_SIZE
            >= self.layer.block_size.get() as usize
        {
            return Err(self.corruption_error("Block seek table overlaps header"));
        }
        debug!(
            pos = self.pos,
            object_size = self.layer.data_offset() + self.layer.data_size,
            oid = self.layer.object_id;
            ""
        );
        if self.layer.version > OLD_KEY_SERIALIZATION_VERSION {
            let block_index = (self.pos - self.layer.data_offset()) / self.layer.block_size;
            self.current_block_base_u64 = *self
                .layer
                .seek_table
                .get(block_index as usize)
                .ok_or_else(|| self.corruption_error("Block index out of bounds"))?;
        }
        self.pos += self.layer.block_size;
        self.item_index = 0;
        self.key = KeyState::None;
        Ok(())
    }

    fn deserialize_current_key(&mut self) -> Result<(), Error> {
        self.seek_to_block_item(self.item_index)?;
        self.key = if self.layer.version <= OLD_KEY_SERIALIZATION_VERSION {
            KeyState::Deserialized(
                K::deserialize_from_version(self.buffer.by_ref(), self.layer.version)
                    .map_err(|e| self.corruption_error(e))
                    .context("Corrupt layer (key)")?,
            )
        } else {
            KeyState::InPlace
        };
        self.item_index += 1;
        Ok(())
    }

    // Binary searches the current block for `search_key`.  The iterator must be positioned at the
    // first item in the block, and that item must be less than `search_key`.  If the block
    // contains a key that is greater than or equal to `search_key`, the iterator is positioned at
    // the first such key and `Some(true)` is returned for an exact match, or `Some(false)`
    // otherwise.  If every key in the block is less than `search_key`, `None` is returned and the
    // caller should advance the iterator to the next block.
    fn binary_search_block(
        &mut self,
        search_key: &mut SearchKey<'_, K>,
    ) -> Result<Option<bool>, Error> {
        if self.layer.version > OLD_KEY_SERIALIZATION_VERSION {
            let target = search_key
                .serialized_with_base(self.current_block_base_u64)
                .ok_or_else(|| self.corruption_error("Search key precedes block"))?;
            return self.binary_search_block_in_place(target);
        }

        let mut left_index = 0;
        let mut right_index = self.item_count;
        while left_index < (right_index - 1) {
            let mid_index = left_index + ((right_index - left_index) / 2);
            self.seek_to_block_item(mid_index).context("Read index offset for binary search")?;
            self.deserialize_current_key()?;
            match search_key.compare(self)?.context("Unexpected EOF")? {
                Ordering::Greater => right_index = mid_index,
                Ordering::Equal => return Ok(Some(true)),
                Ordering::Less => left_index = mid_index,
            }
        }
        if right_index < self.item_count {
            self.seek_to_block_item(right_index)
                .context("Read index for offset of right pointer")?;
            self.deserialize_current_key()?;
            return Ok(Some(false));
        }
        self.key = KeyState::None;
        Ok(None)
    }

    // Like `binary_search_block`, but compares the serialized `target` key directly against the
    // keys in the block without repositioning the iterator for each probe.
    fn binary_search_block_in_place(&mut self, target: &[u8]) -> Result<Option<bool>, Error> {
        let mut left_index = 0;
        let mut right_index = self.item_count;
        if right_index > 1 {
            let block_size = self.layer.block_size.get() as usize;
            let block_start =
                self.layer.block_size.align_down((self.buffer.pos as u64).saturating_sub(1))
                    as usize;
            let block_bytes = self
                .buffer
                .buffer
                .as_bytes_from(block_start)
                .get(..block_size)
                .ok_or_else(|| self.corruption_error("Short block"))?;
            // `read_block_header` has checked that the seek entries don't overlap the header.
            let seek_table_offset =
                block_size - usize::from(right_index - 1) * PER_DATA_BLOCK_SEEK_ENTRY_SIZE;
            let (data_bytes, seek_entries) = block_bytes.split_at(seek_table_offset);
            let mut found = None;
            while left_index < (right_index - 1) {
                let mid_index = left_index + ((right_index - left_index) / 2);
                let entry_pos = usize::from(mid_index - 1) * PER_DATA_BLOCK_SEEK_ENTRY_SIZE;
                let offset = LittleEndian::read_u16(
                    &seek_entries[entry_pos..entry_pos + PER_DATA_BLOCK_SEEK_ENTRY_SIZE],
                ) as usize;
                if offset >= seek_table_offset || offset <= PER_DATA_BLOCK_HEADER_SIZE {
                    return Err(
                        self.corruption_error(format!("Offset {offset} is out of valid range."))
                    );
                }
                match compare_keys(&data_bytes[offset..], target)
                    .map_err(|e| self.corruption_error(e))?
                {
                    Ordering::Greater => {
                        right_index = mid_index;
                        found = Some((mid_index, offset, false));
                    }
                    Ordering::Equal => {
                        found = Some((mid_index, offset, true));
                        break;
                    }
                    Ordering::Less => left_index = mid_index,
                }
            }
            if let Some((index, offset, exact)) = found {
                self.item_index = index + 1;
                self.key = KeyState::InPlace;
                self.buffer.pos = block_start + offset;
                return Ok(Some(exact));
            }
        }
        self.item_index = self.item_count;
        self.key = KeyState::None;
        Ok(None)
    }
}

impl<'iter, K: Key, V: LayerValue, B> KeyOnlyIterator<'iter, K, V, B> {
    // Returns an iterator which reads the block at `pos` (the file offset of a key block) out of
    // `buffer`, starting at `buffer_pos`.
    fn new(layer: &'iter LayerData<K>, buffer: B, buffer_pos: usize, pos: u64) -> Self {
        assert!(layer.block_size.is_aligned(pos));
        Self {
            layer,
            buffer: BufferCursor { buffer, pos: buffer_pos },
            pos,
            item_index: 0,
            item_count: 0,
            key: KeyState::None,
            current_block_base_u64: 0,
            _value_type: PhantomData,
        }
    }
}

impl<'iter, K: Key, V: LayerValue> KeyOnlyIterator<'iter, K, V, ChunkBuffer<'iter>> {
    fn new_async(layer: &'iter PersistentLayer<K, V>, pos: u64) -> Self {
        Self::new(
            &layer.data,
            ChunkBuffer { handle: &layer.object_handle, chunk: None },
            (pos % CHUNK_SIZE) as usize,
            pos,
        )
    }

    // Returns a new iterator for the block at `pos`, reusing the chunk held by `self` or `other`
    // if either contains `pos`, which saves a cache lookup.
    fn reposition(&self, pos: u64, other: Option<&Self>) -> Self {
        let chunk_num = (pos / CHUNK_SIZE) as usize;
        let chunk =
            std::iter::once(self).chain(other).find_map(|iter| match &iter.buffer.buffer.chunk {
                Some((n, chunk)) if *n == chunk_num => Some((chunk_num, chunk.clone())),
                _ => None,
            });
        Self::new(
            self.layer,
            ChunkBuffer { handle: self.buffer.buffer.handle, chunk },
            (pos % CHUNK_SIZE) as usize,
            pos,
        )
    }

    async fn advance(&mut self) -> Result<(), Error> {
        if !self.try_advance()? {
            let chunk_num = (self.pos / CHUNK_SIZE) as usize;
            let chunk = self
                .buffer
                .buffer
                .handle
                .read(self.pos as usize)
                .await
                .context("Reading during advance")?;
            self.buffer.buffer.chunk = Some((chunk_num, chunk));
            self.buffer.pos = (self.pos % CHUNK_SIZE) as usize;
            self.read_block_header()?;
            self.deserialize_current_key()?;
        }
        Ok(())
    }

    // Advances without blocking.  Returns false, leaving the iterator unchanged, if the next block
    // needs to be read from the underlying handle.
    fn try_advance(&mut self) -> Result<bool, Error> {
        if self.item_index >= self.item_count {
            if self.pos >= self.layer.data_offset() + self.layer.data_size {
                self.key = KeyState::None;
                return Ok(true);
            }
            let chunk_num = (self.pos / CHUNK_SIZE) as usize;
            if !matches!(&self.buffer.buffer.chunk, Some((n, _)) if *n == chunk_num) {
                let Some(chunk) = self.buffer.buffer.handle.try_read(self.pos as usize) else {
                    return Ok(false);
                };
                self.buffer.buffer.chunk = Some((chunk_num, chunk));
            }
            self.buffer.pos = (self.pos % CHUNK_SIZE) as usize;
            self.read_block_header()?;
        }
        self.deserialize_current_key()?;
        Ok(true)
    }
}

impl<'iter, K: Key, V: LayerValue> KeyOnlyIterator<'iter, K, V, SliceBuffer<'iter>> {
    fn new_sync(layer: &'iter SyncPersistentLayer<K, V>, pos: u64) -> Self {
        let slice = layer.object_handle.as_slice().expect("slice must be present");
        Self::new(
            &layer.data,
            SliceBuffer { handle: layer.object_handle.as_ref(), slice },
            pos as usize,
            pos,
        )
    }

    // Returns a new iterator for the block at `pos`.  `_other` exists for parity with the
    // chunk-backed version; the whole layer is always available here.
    fn reposition(&self, pos: u64, _other: Option<&Self>) -> Self {
        Self::new(self.layer, self.buffer.buffer, pos as usize, pos)
    }

    fn advance(&mut self) -> Result<(), Error> {
        if self.item_index >= self.item_count {
            if self.pos >= self.layer.data_offset() + self.layer.data_size {
                self.key = KeyState::None;
                return Ok(());
            }
            self.buffer.pos = self.pos as usize;
            self.read_block_header()?;
        }
        self.deserialize_current_key()
    }
}

struct Iterator<'iter, K: Key, V: LayerValue, B> {
    inner: KeyOnlyIterator<'iter, K, V, B>,
    // The current item.
    item: Option<Item<K, V>>,
}

impl<'iter, K: Key, V: LayerValue, B: LayerBuffer> Iterator<'iter, K, V, B> {
    fn new(mut seek_iterator: KeyOnlyIterator<'iter, K, V, B>) -> Result<Self, Error> {
        let item = seek_iterator.take_item()?;
        Ok(Self { inner: seek_iterator, item })
    }
}

impl<'iter, K: Key, V: LayerValue> LayerIterator<K, V>
    for Iterator<'iter, K, V, ChunkBuffer<'iter>>
{
    async fn advance(&mut self) -> Result<(), Error> {
        self.inner.advance().await?;
        self.item = self.inner.take_item()?;
        Ok(())
    }

    fn advance_dyn<'a>(&'a mut self) -> Result<Option<BoxFuture<'a, Result<(), Error>>>, Error> {
        if self.inner.try_advance()? {
            self.item = self.inner.take_item()?;
            Ok(None)
        } else {
            Ok(Some(Box::pin(self.advance())))
        }
    }

    fn get(&self) -> Option<ItemRef<'_, K, V>> {
        self.item.as_ref().map(<&Item<K, V>>::into)
    }
}

impl<'iter, K: Key, V: LayerValue> LayerIterator<K, V>
    for Iterator<'iter, K, V, SliceBuffer<'iter>>
{
    async fn advance(&mut self) -> Result<(), Error> {
        self.inner.advance()?;
        self.item = self.inner.take_item()?;
        Ok(())
    }

    fn advance_dyn<'a>(&'a mut self) -> Result<Option<BoxFuture<'a, Result<(), Error>>>, Error> {
        self.inner.advance()?;
        self.item = self.inner.take_item()?;
        Ok(None)
    }

    fn get(&self) -> Option<ItemRef<'_, K, V>> {
        self.item.as_ref().map(<&Item<K, V>>::into)
    }
}

fn seek_table_size(version: Version, num_data_blocks: u64) -> Result<usize, Error> {
    if num_data_blocks == 0 || version <= OLD_KEY_SERIALIZATION_VERSION {
        // Ignore the seek table on older versions because AllocatorKey::get_leading_u64()
        // changed from bytes to blocks.
        return Ok(0);
    }
    let seek_table_size = (num_data_blocks as usize) * std::mem::size_of::<u64>();
    if seek_table_size > MAX_SEEK_TABLE_SIZE {
        return Err(anyhow!(FxfsError::NotSupported)).context("Seek table too large");
    }
    Ok(seek_table_size)
}

fn parse_seek_table(bytes: PtrByteSlice<'_>, num_data_blocks: u64) -> Result<Vec<u64>, Error> {
    let mut seek_table = Vec::with_capacity(num_data_blocks as usize);
    let mut prev = 0;
    for chunk in bytes.iter_as::<[u8; 8]>() {
        let next = u64::from_le_bytes(chunk);
        // Should be in strict ascending order, otherwise something's broken, or we've gone off
        // the end and we're reading zeroes.
        if prev > next {
            return Err(anyhow!(FxfsError::Inconsistent))
                .context(format!("Seek table entry out of order, {prev:?} > {next:?}"));
        }
        prev = next;
        seek_table.push(next);
    }
    Ok(seek_table)
}

async fn load_seek_table(
    object_handle: &(impl ReadObjectHandle + 'static),
    seek_table_offset: u64,
    num_data_blocks: u64,
    version: Version,
) -> Result<Vec<u64>, Error> {
    let seek_table_size = seek_table_size(version, num_data_blocks)?;
    if seek_table_size == 0 {
        return Ok(Vec::new());
    }
    let aligned_size =
        object_handle.block_size().align_up(seek_table_size as u64).ok_or(FxfsError::TooBig)?
            as usize;
    let mut buffer = object_handle.allocate_buffer(aligned_size).await;
    let bytes_read = object_handle
        .read_aligned(seek_table_offset, buffer.as_mut())
        .await
        .context("Reading seek table blocks")?;
    ensure!(bytes_read >= seek_table_size, "Short read");

    parse_seek_table(buffer.subslice(0..seek_table_size).as_ptr_slice(), num_data_blocks)
}

const BLOOM_FILTER_READ_CHUNK_SIZE: usize = 1024 * 1024;

fn bloom_filter_size(version: Version, layer_info: &LayerInfo) -> Result<usize, Error> {
    if version != LATEST_VERSION || layer_info.bloom_filter_size_bytes == 0 {
        // Ignore the bloom filter for layer files in outdated versions.  We don't know whether
        // keys have changed formats or not (and therefore have different hash values), so we
        // must ignore the bloom filter and always query the layer.
        return Ok(0);
    }
    if layer_info.bloom_filter_size_bytes > MAX_BLOOM_FILTER_SIZE {
        return Err(anyhow!(FxfsError::NotSupported)).context("Bloom filter too large");
    }
    Ok(layer_info.bloom_filter_size_bytes)
}

async fn load_bloom_filter<K: FuzzyHash>(
    handle: &(impl ReadObjectHandle + 'static),
    bloom_filter_offset: u64,
    version: Version,
    layer_info: &LayerInfo,
) -> Result<Option<BloomFilterReader<K>>, Error> {
    let size = bloom_filter_size(version, layer_info)?;
    if size == 0 {
        return Ok(None);
    }
    let aligned_size = handle.block_size().align_up(size as u64).ok_or(FxfsError::TooBig)? as usize;
    let mut buffer = handle.allocate_buffer(aligned_size).await;
    let reads = FuturesUnordered::new();
    let mut offset = bloom_filter_offset;
    for chunk in buffer.as_mut().chunks_mut(BLOOM_FILTER_READ_CHUNK_SIZE) {
        let chunk_len = chunk.len() as u64;
        reads.push(async move {
            handle.read_aligned(offset, chunk).await.context("Failed to read")?;
            Ok::<(), Error>(())
        });
        offset += chunk_len;
    }
    reads.try_collect::<()>().await?;
    Ok(Some(BloomFilterReader::read(
        buffer.subslice(0..size).as_ptr_slice(),
        layer_info.bloom_filter_seed,
        layer_info.bloom_filter_num_hashes,
    )?))
}

struct SearchKey<'a, K: Key> {
    key: &'a K,
    buf: Vec<u8>,
    cached_base: Option<u64>,
}

impl<'a, K: Key> SearchKey<'a, K> {
    fn new(key: &'a K) -> Self {
        Self { key, buf: Vec::new(), cached_base: None }
    }

    // Returns the key serialized relative to `base`, or None if the key sorts before any key that
    // can be encoded relative to `base`.
    fn serialized_with_base(&mut self, base: u64) -> Option<&[u8]> {
        if self.key.get_leading_u64() < base {
            return None;
        }
        if self.cached_base != Some(base) {
            self.buf.clear();
            self.key.serialize_key_with_base_into(&mut self.buf, base);
            self.cached_base = Some(base);
        }
        Some(&self.buf)
    }

    fn compare<V: LayerValue, B: LayerBuffer>(
        &mut self,
        iter: &KeyOnlyIterator<'_, K, V, B>,
    ) -> Result<Option<Ordering>, Error> {
        match &iter.key {
            KeyState::None => Ok(None),
            KeyState::Deserialized(k) => Ok(Some(k.cmp_upper_bound(self.key))),
            KeyState::InPlace => {
                let Some(target) = self.serialized_with_base(iter.current_block_base_u64) else {
                    return Ok(Some(Ordering::Greater));
                };
                Ok(Some(
                    compare_keys(iter.buffer.as_bytes(), target)
                        .map_err(|e| iter.corruption_error(e))?,
                ))
            }
        }
    }
}

impl<K: FuzzyHash> LayerData<K> {
    fn new(
        object_id: u64,
        version: Version,
        block_size: BlockSize,
        layer_info: &LayerInfo,
        seek_table: Vec<u64>,
        bloom_filter: Option<BloomFilterReader<K>>,
    ) -> Self {
        let bloom_filter_stats = bloom_filter.as_ref().map(|b| b.stats());
        Self {
            object_id,
            version,
            block_size,
            data_size: block_size * layer_info.num_data_blocks,
            seek_table,
            num_items: layer_info.num_items,
            bloom_filter,
            bloom_filter_stats,
            close_event: Mutex::new(Some(Arc::new(DropEvent::new()))),
        }
    }

    fn parse_header(
        mut reader: PtrByteSlice<'_>,
        handle_block_size: BlockSize,
        total_size: u64,
    ) -> Result<(Version, BlockSize, u64), Error> {
        let version = Version::deserialize_from(&mut reader)?;
        ensure!(version <= LATEST_VERSION, FxfsError::InvalidVersion);
        let header = LayerHeader::deserialize_from_version(&mut reader, version)
            .context("Failed to deserialize header")?;
        if &header.magic != PERSISTENT_LAYER_MAGIC {
            return Err(anyhow!(FxfsError::Inconsistent).context("Invalid layer file magic"));
        }
        let block_size = BlockSize::from_u64(header.block_size).ok_or_else(|| {
            anyhow!(FxfsError::Inconsistent)
                .context(format!("Invalid block size {}", header.block_size))
        })?;
        ensure!(block_size <= MAX_BLOCK_SIZE, FxfsError::NotSupported);
        ensure!(block_size >= MIN_BLOCK_SIZE, FxfsError::NotSupported);
        if !handle_block_size.is_aligned(block_size.get()) {
            return Err(anyhow!(FxfsError::Inconsistent)).context(format!(
                "{block_size} not a multiple of handle block size {handle_block_size}"
            ));
        }
        if total_size < MINIMUM_LAYER_FILE_BLOCKS * block_size {
            return Err(anyhow!(FxfsError::Inconsistent).context("Layer file too short"));
        }
        let last_block_offset = total_size
            .checked_sub(block_size.get())
            .ok_or(FxfsError::Inconsistent)
            .context("Layer file unexpectedly short")?;
        Ok((version, block_size, last_block_offset))
    }

    fn parse_layer_info(
        last_block: PtrByteSlice<'_>,
        version: Version,
        block_size: BlockSize,
        total_size: u64,
    ) -> Result<(LayerInfo, u64, u64), Error> {
        let bs = block_size.get() as usize;
        let layer_info_len = u64::from_le_bytes(last_block.subslice(bs - 8..bs).read().unwrap());
        let layer_info_offset = std::mem::size_of::<u64>()
            .checked_add(layer_info_len as usize)
            .and_then(|n| bs.checked_sub(n))
            .ok_or(FxfsError::Inconsistent)
            .context("Invalid layer info length")?;
        let mut reader = last_block.subslice(layer_info_offset..bs);
        let layer_info = LayerInfo::deserialize_from_version(&mut reader, version)
            .context("Failed to deserialize LayerInfo")?;
        if layer_info.num_items == 0 && layer_info.num_data_blocks > 0 {
            return Err(anyhow!(FxfsError::Inconsistent))
                .context("Invalid num_items/num_data_blocks");
        }
        let total_blocks = total_size / block_size;
        let bloom_filter_blocks =
            block_size.align_up_to_blocks(layer_info.bloom_filter_size_bytes as u64);
        if layer_info.num_data_blocks.saturating_add(bloom_filter_blocks)
            > total_blocks - MINIMUM_LAYER_FILE_BLOCKS
        {
            return Err(anyhow!(FxfsError::Inconsistent)).context("Invalid number of blocks");
        }
        let bloom_filter_offset = block_size * (NUM_HEADER_BLOCKS + layer_info.num_data_blocks);
        let seek_offset =
            block_size * (NUM_HEADER_BLOCKS + layer_info.num_data_blocks + bloom_filter_blocks);
        Ok((layer_info, bloom_filter_offset, seek_offset))
    }

    async fn open(handle: &Arc<dyn LayerObject>) -> Result<Self, Error> {
        let handle_block_size = handle.block_size();
        let mut buffer = handle.allocate_buffer(handle_block_size.get() as usize).await;
        handle.read_aligned(0, buffer.as_mut()).await.context("Failed to read first block")?;
        let (version, block_size, last_block_offset) =
            Self::parse_header(buffer.as_ptr_slice(), handle_block_size, handle.get_size())?;

        let bs = block_size.get() as usize;
        handle
            .read_aligned(last_block_offset, buffer.subslice_mut(0..bs))
            .await
            .context("Failed to read layer info")?;
        let (layer_info, bloom_filter_offset, seek_offset) = Self::parse_layer_info(
            buffer.subslice(0..bs).as_ptr_slice(),
            version,
            block_size,
            handle.get_size(),
        )?;
        std::mem::drop(buffer);

        let bloom_filter = load_bloom_filter(handle, bloom_filter_offset, version, &layer_info)
            .await
            .context("Failed to load bloom filter")?;
        let seek_table = load_seek_table(handle, seek_offset, layer_info.num_data_blocks, version)
            .await
            .context("Failed to load seek table")?;

        Ok(Self::new(
            handle.object_id(),
            version,
            block_size,
            &layer_info,
            seek_table,
            bloom_filter,
        ))
    }

    fn open_from_slice(handle: &Arc<dyn LayerObject>) -> Result<Self, Error> {
        let slice = handle.as_slice().ok_or(FxfsError::InvalidArgs)?;
        let io_or_corruption = |err: Error| -> Error {
            if handle.has_io_error() { anyhow!(zx_status::Status::IO).context(err) } else { err }
        };
        let handle_block_size = handle.block_size();
        let total_size = slice.len() as u64;
        if total_size < handle_block_size.get() {
            return Err(io_or_corruption(
                anyhow!(FxfsError::Inconsistent).context("Layer file too short"),
            ));
        }
        let (version, block_size, last_block_offset) = Self::parse_header(
            slice[..handle_block_size.get() as usize].into(),
            handle_block_size,
            total_size,
        )
        .map_err(&io_or_corruption)?;

        let bs = block_size.get() as usize;
        let last_block = &slice[last_block_offset as usize..last_block_offset as usize + bs];
        let (layer_info, bloom_filter_offset, seek_offset) =
            Self::parse_layer_info(last_block.into(), version, block_size, total_size)
                .map_err(&io_or_corruption)?;

        let bf_size = bloom_filter_size(version, &layer_info)
            .map_err(&io_or_corruption)
            .context("Failed to load bloom filter")?;
        let st_size = seek_table_size(version, layer_info.num_data_blocks)
            .map_err(&io_or_corruption)
            .context("Failed to load seek table")?;
        if bf_size > 0 {
            handle.prefetch(bloom_filter_offset..total_size);
        } else if st_size > 0 {
            handle.prefetch(seek_offset..total_size);
        }

        let bloom_filter = if bf_size > 0 {
            let start = bloom_filter_offset as usize;
            let end = start
                .checked_add(bf_size)
                .filter(|&end| end <= slice.len())
                .ok_or_else(|| io_or_corruption(anyhow!(FxfsError::Inconsistent)))
                .context("Bloom filter out of bounds")?;
            Some(BloomFilterReader::from_bytes(
                &slice[start..end],
                layer_info.bloom_filter_seed,
                layer_info.bloom_filter_num_hashes,
            ))
        } else {
            None
        };

        let seek_table = if st_size > 0 {
            let start = seek_offset as usize;
            let end = start
                .checked_add(st_size)
                .filter(|&end| end <= slice.len())
                .ok_or_else(|| io_or_corruption(anyhow!(FxfsError::Inconsistent)))
                .context("Seek table out of bounds")?;
            parse_seek_table(slice[start..end].into(), layer_info.num_data_blocks)
                .map_err(&io_or_corruption)
                .context("Failed to load seek table")?
        } else {
            Vec::new()
        };

        if handle.has_io_error() {
            return Err(anyhow!(zx_status::Status::IO).context("I/O error while opening layer"));
        }

        Ok(Self::new(
            handle.object_id(),
            version,
            block_size,
            &layer_info,
            seek_table,
            bloom_filter,
        ))
    }
}

macro_rules! seek_impl {
    ($self:ident, $bound:ident $(, $await:tt)?) => {{
        let (key, excluded) = match $bound {
            Bound::Unbounded => {
                let mut iterator = $self.key_only_iterator($self.data_offset());
                iterator.advance()$(.$await)? .context("Unbounded seek advance")?;
                return Ok(Iterator::new(iterator)?);
            }
            Bound::Included(k) => (k, false),
            Bound::Excluded(k) => (k, true),
        };
        let first_data_block_index = $self.data_offset() / $self.data.block_size;

        let (mut left_offset, mut right_offset) = if $self.data.seek_table.is_empty() {
            ($self.data_offset(), $self.data_offset() + $self.data.data_size)
        } else {
            // We are searching for a range here, as multiple items can have the same value in
            // this approximate search. Since the values in the seek table represent the first
            // key in each block (ordered by cmp_upper_bound), if the value equals the target
            // we must also search the block before it. The goal is for
            // table[left] < target < table[right].
            let target = key.get_leading_u64();
            let right_index =
                $self.data.seek_table.as_slice().partition_point(|&x| x <= target) as u64;
            if right_index == 0 {
                let mut iterator = $self.key_only_iterator($self.data_offset());
                iterator.advance()$(.$await)? .context("Initial seek advance")?;
                return Ok(Iterator::new(iterator)?);
            }
            // Since partition_point will find the index of the first place where the predicate
            // is false, we subtract 1 to get the index where it was last true.
            let left_index = $self.data.seek_table.as_slice()[..right_index as usize]
                .partition_point(|&x| x < target)
                .saturating_sub(1) as u64;

            (
                (left_index + first_data_block_index) * $self.data.block_size,
                (right_index + first_data_block_index) * $self.data.block_size,
            )
        };
        let mut left = $self.key_only_iterator(left_offset);
        left.advance()$(.$await)? .context("Initial seek advance")?;
        let mut search_key = SearchKey::new(key);
        match search_key.compare(&left)? {
            Some(Ordering::Less) => {}
            Some(Ordering::Equal) if excluded => {
                left.advance()$(.$await)??;
                return Ok(Iterator::new(left)?);
            }
            _ => return Ok(Iterator::new(left)?),
        }
        let mut right = None;
        while right_offset - left_offset > $self.data.block_size {
            // Pick a block midway.
            let mid_offset =
                $self.data.block_size.align_down(left_offset + (right_offset - left_offset) / 2);
            let mut iterator = left.reposition(mid_offset, right.as_ref());
            iterator.advance()$(.$await)??;
            match search_key.compare(&iterator)?.context("Unexpected EOF")? {
                Ordering::Greater => {
                    right_offset = mid_offset;
                    right = Some(iterator);
                }
                Ordering::Equal => {
                    if excluded {
                        iterator.advance()$(.$await)??;
                    }
                    return Ok(Iterator::new(iterator)?);
                }
                Ordering::Less => {
                    left_offset = mid_offset;
                    left = iterator;
                }
            }
        }

        // Finish the binary search on the block pointed to by `left`.
        match left.binary_search_block(&mut search_key)? {
            Some(true) if excluded => left.advance()$(.$await)??,
            Some(_) => {}
            // When we don't find an entry that is greater than or equal to the target in `left`,
            // we need to return with the first entry of the next block, which might already be
            // pointed to by `right`.  Otherwise, advance to the next block (or the end of the
            // layer).
            None => match right {
                Some(right) => return Ok(Iterator::new(right)?),
                None => left.advance()$(.$await)??,
            },
        }
        Ok(Iterator::new(left)?)
    }};
}

fn page_size() -> BlockSize {
    #[cfg(target_os = "fuchsia")]
    {
        storage_units::page_size().into()
    }
    #[cfg(not(target_os = "fuchsia"))]
    {
        BlockSize::SIZE_4KIB
    }
}

impl<K: Key, V: LayerValue> PersistentLayer<K, V> {
    pub async fn open(handle: impl LayerObject + 'static) -> Result<Arc<dyn Layer<K, V>>, Error> {
        Self::open_layer(Arc::new(handle)).await
    }

    pub async fn open_layer(handle: Arc<dyn LayerObject>) -> Result<Arc<dyn Layer<K, V>>, Error> {
        let data = LayerData::open(&handle).await?;
        if handle.as_slice().is_some() && data.block_size <= page_size() {
            Ok(Arc::new(SyncPersistentLayer {
                object_handle: handle,
                data,
                _value_type: PhantomData,
            }) as Arc<dyn Layer<K, V>>)
        } else {
            Ok(Arc::new(PersistentLayer {
                object_handle: CachingObjectHandle::new(handle),
                data,
                _value_type: PhantomData,
            }) as Arc<dyn Layer<K, V>>)
        }
    }

    pub async fn open_async(handle: Arc<dyn LayerObject>) -> Result<Arc<Self>, Error> {
        let data = LayerData::open(&handle).await?;
        Ok(Arc::new(PersistentLayer {
            object_handle: CachingObjectHandle::new(handle),
            data,
            _value_type: PhantomData,
        }))
    }

    /// Opens a persistent layer backed by `handle`. If the layer is encrypted and the platform
    /// uses a pager, `unwrapped_key` is registered with the pager and dropped immediately.
    pub async fn open_handle(
        handle: DataObjectHandle<ObjectStore>,
        unwrapped_key: Option<UnwrappedKey>,
    ) -> Result<Arc<dyn Layer<K, V>>, Error> {
        let layer_object: Arc<dyn LayerObject> =
            if let Some(layer_pager) = handle.store().filesystem().layer_pager() {
                layer_pager.open_layer(handle, unwrapped_key).await?
            } else {
                Arc::new(handle)
            };
        Self::open_layer(layer_object).await
    }

    fn data_offset(&self) -> u64 {
        self.data.data_offset()
    }

    fn key_only_iterator(&self, pos: u64) -> KeyOnlyIterator<'_, K, V, ChunkBuffer<'_>> {
        KeyOnlyIterator::new_async(self, pos)
    }

    async fn seek<'a>(
        &'a self,
        bound: Bound<&K>,
    ) -> Result<Iterator<'a, K, V, ChunkBuffer<'a>>, Error> {
        seek_impl!(self, bound, await)
    }
}

impl<K: Key, V: LayerValue> SyncPersistentLayer<K, V> {
    pub fn open(handle: Arc<dyn LayerObject>) -> Result<Arc<Self>, Error> {
        let data = LayerData::open_from_slice(&handle)?;
        ensure!(data.block_size <= page_size(), FxfsError::NotSupported);
        Ok(Arc::new(Self { object_handle: handle, data, _value_type: PhantomData }))
    }

    fn data_offset(&self) -> u64 {
        self.data.data_offset()
    }

    fn key_only_iterator(&self, pos: u64) -> KeyOnlyIterator<'_, K, V, SliceBuffer<'_>> {
        KeyOnlyIterator::new_sync(self, pos)
    }

    fn seek<'a>(&'a self, bound: Bound<&K>) -> Result<Iterator<'a, K, V, SliceBuffer<'a>>, Error> {
        seek_impl!(self, bound)
    }
}

/// Unwraps the encryption key for `object_id` if a `LayerPager` is active on `store`.
async fn unwrap_layer_key(
    store: &ObjectStore,
    object_id: u64,
    crypt: &dyn Crypt,
) -> Result<Option<UnwrappedKey>, Error> {
    if store.filesystem().layer_pager().is_none() {
        return Ok(None);
    }
    let keys = store
        .get_keys(object_id)
        .await
        .with_context(|| format!("Failed to get keys for layer file {object_id}"))?;
    let (_, key) = keys
        .first()
        .ok_or_else(|| anyhow!(FxfsError::Inconsistent))
        .with_context(|| format!("Missing key for encrypted layer file {object_id}"))?;
    let wrapped_key = WrappedKey::from(key.clone());
    let unwrapped = crypt
        .unwrap_key(&wrapped_key, object_id)
        .await
        .with_context(|| format!("Failed to unwrap key for layer file {object_id}"))?;
    Ok(Some(unwrapped))
}

/// Opens a persistent layer from a newly created or existing `DataObjectHandle`.
///
/// If the layer is encrypted and the platform uses a pager, `unwrapped_key` is registered with the
/// pager and dropped immediately.
pub async fn layer_from_handle<K: Key, V: LayerValue>(
    handle: DataObjectHandle<ObjectStore>,
    unwrapped_key: Option<UnwrappedKey>,
) -> Result<Arc<dyn Layer<K, V>>, Error> {
    PersistentLayer::open_handle(handle, unwrapped_key).await
}

/// Opens persistent layers for `object_ids` from `store`.
///
/// Returns `(layers, total_size)` where `layers` is the vector of opened `Layer` trait objects and
/// `total_size` is the sum of the sizes in bytes of all opened layer objects.
pub async fn open_layers<K: Key, V: LayerValue>(
    store: &Arc<ObjectStore>,
    object_ids: impl IntoIterator<Item = u64>,
    crypt: Option<Arc<dyn Crypt>>,
) -> Result<(Vec<Arc<dyn Layer<K, V>>>, u64), Error> {
    let mut layers = Vec::new();
    let mut total_size = 0;
    for object_id in object_ids {
        let handle =
            ObjectStore::open_object(store, object_id, HandleOptions::default(), crypt.clone())
                .await
                .with_context(|| format!("Failed to open layer file {object_id}"))?;
        total_size += handle.get_size();
        let unwrapped_key = if let Some(crypt) = &crypt {
            unwrap_layer_key(store, object_id, crypt.as_ref()).await?
        } else {
            None
        };
        layers.push(PersistentLayer::open_handle(handle, unwrapped_key).await?);
    }
    Ok((layers, total_size))
}

#[async_trait]
impl<K: Key, V: LayerValue> Layer<K, V> for PersistentLayer<K, V> {
    fn handle(&self) -> Option<&dyn LayerObject> {
        Some(self.object_handle.source())
    }

    fn purge_cached_data(&self) {
        self.object_handle.purge();
    }

    fn clear_cached_data(&self) {
        self.object_handle.clear();
    }

    async fn seek<'a>(&'a self, bound: Bound<&K>) -> Result<BoxedLayerIterator<'a, K, V>, Error> {
        Ok(Box::new(PersistentLayer::seek(self, bound).await?))
    }

    fn len(&self) -> usize {
        self.data.num_items
    }

    fn maybe_contains_key(&self, key: &K) -> MaybeContainsKey {
        self.data.bloom_filter.as_ref().map_or(MaybeContainsKey::Maybe, |f| f.maybe_contains(key))
    }

    fn has_bloom_filter(&self) -> bool {
        self.data.bloom_filter.is_some()
    }

    async fn key_exists(&self, key: &K) -> Result<Existence, Error> {
        match &self.data.bloom_filter {
            Some(filter) => Ok(match filter.maybe_contains(key) {
                MaybeContainsKey::False => Existence::Missing,
                MaybeContainsKey::Maybe | MaybeContainsKey::RangeKeyTooLarge => {
                    Existence::MaybeExists
                }
            }),
            None => {
                let iter = self.seek(Bound::Included(key)).await?;
                Ok(iter.get().map_or(Existence::Missing, |i| {
                    if i.key.cmp_upper_bound(key).is_eq() {
                        Existence::Exists
                    } else {
                        Existence::Missing
                    }
                }))
            }
        }
    }

    fn lock(&self) -> Option<Arc<DropEvent>> {
        self.data.close_event.lock().clone()
    }

    async fn close(&self) {
        let listener = self.data.close_event.lock().take().expect("close already called").listen();
        listener.await;
        self.object_handle.source().close().await;
    }

    fn get_version(&self) -> Version {
        return self.data.version;
    }

    fn record_inspect_data(self: Arc<Self>, node: &fuchsia_inspect::Node) {
        node.record_uint("num_items", self.data.num_items as u64);
        node.record_bool("persistent", true);
        node.record_uint("size", self.object_handle.source().get_size());
        if let Some(stats) = self.data.bloom_filter_stats.as_ref() {
            node.record_child("bloom_filter", move |node| {
                node.record_uint("size", stats.size as u64);
                node.record_uint("num_hashes", stats.num_hashes as u64);
                node.record_uint("fill_percentage", stats.fill_percentage as u64);
            });
        }
    }
}

#[async_trait]
impl<K: Key, V: LayerValue> Layer<K, V> for SyncPersistentLayer<K, V> {
    fn handle(&self) -> Option<&dyn LayerObject> {
        Some(&self.object_handle)
    }

    fn purge_cached_data(&self) {
        self.object_handle.purge_cached_data();
    }

    fn clear_cached_data(&self) {
        self.object_handle.purge_cached_data();
    }

    async fn seek<'a>(&'a self, bound: Bound<&K>) -> Result<BoxedLayerIterator<'a, K, V>, Error> {
        Ok(Box::new(SyncPersistentLayer::seek(self, bound)?))
    }

    fn len(&self) -> usize {
        self.data.num_items
    }

    fn maybe_contains_key(&self, key: &K) -> MaybeContainsKey {
        self.data.bloom_filter.as_ref().map_or(MaybeContainsKey::Maybe, |f| f.maybe_contains(key))
    }

    fn has_bloom_filter(&self) -> bool {
        self.data.bloom_filter.is_some()
    }

    async fn key_exists(&self, key: &K) -> Result<Existence, Error> {
        match &self.data.bloom_filter {
            Some(filter) => Ok(match filter.maybe_contains(key) {
                MaybeContainsKey::False => Existence::Missing,
                MaybeContainsKey::Maybe | MaybeContainsKey::RangeKeyTooLarge => {
                    Existence::MaybeExists
                }
            }),
            None => {
                let iter = self.seek(Bound::Included(key))?;
                Ok(iter.get().map_or(Existence::Missing, |i| {
                    if i.key.cmp_upper_bound(key).is_eq() {
                        Existence::Exists
                    } else {
                        Existence::Missing
                    }
                }))
            }
        }
    }

    fn lock(&self) -> Option<Arc<DropEvent>> {
        self.data.close_event.lock().clone()
    }

    async fn close(&self) {
        let listener = self.data.close_event.lock().take().expect("close already called").listen();
        listener.await;
        self.object_handle.close().await;
    }

    fn get_version(&self) -> Version {
        return self.data.version;
    }

    fn record_inspect_data(self: Arc<Self>, node: &fuchsia_inspect::Node) {
        node.record_uint("num_items", self.data.num_items as u64);
        node.record_bool("persistent", true);
        node.record_uint("size", self.object_handle.get_size());
        if let Some(stats) = self.data.bloom_filter_stats.as_ref() {
            node.record_child("bloom_filter", move |node| {
                node.record_uint("size", stats.size as u64);
                node.record_uint("num_hashes", stats.num_hashes as u64);
                node.record_uint("fill_percentage", stats.fill_percentage as u64);
            });
        }
    }
}

// This ensures that item_count can't be overflowed below.
const_assert!(MAX_BLOCK_SIZE.size() <= u16::MAX as u64 + 1);

// -- Writer support --

pub struct PersistentLayerWriter<W: WriteBytes, K: Key, V: LayerValue> {
    writer: W,
    version: Version,
    block_size: BlockSize,
    buf: Vec<u8>,
    buf_item_count: LayerWriterBufItemCount,
    item_count: usize,
    block_offsets: Vec<u16>,
    block_keys: Vec<u64>,
    bloom_filter: BloomFilterWriter<K>,
    _value: PhantomData<V>,
}

impl<W: WriteBytes, K: Key, V: LayerValue> PersistentLayerWriter<W, K, V> {
    /// Creates a new writer that will serialize items to the object accessible via |object_handle|
    pub async fn new(writer: W, num_items: usize, block_size: BlockSize) -> Result<Self, Error> {
        Self::new_with_version(writer, num_items, block_size, LATEST_VERSION).await
    }

    pub(crate) async fn new_with_version(
        mut writer: W,
        num_items: usize,
        block_size: BlockSize,
        version: Version,
    ) -> Result<Self, Error> {
        ensure!(block_size <= MAX_BLOCK_SIZE, FxfsError::NotSupported);
        ensure!(block_size >= MIN_BLOCK_SIZE, FxfsError::NotSupported);

        // Write the header block.
        let header =
            LayerHeader { magic: PERSISTENT_LAYER_MAGIC.clone(), block_size: block_size.get() };
        let mut buf = vec![0u8; block_size.get() as usize];
        {
            let mut cursor = std::io::Cursor::new(&mut buf[..]);
            version.serialize_into(&mut cursor)?;
            header.serialize_into(&mut cursor)?;
        }
        writer.write_bytes(&buf[..]).await?;

        let seed: u64 = rand::random();
        Ok(Self {
            writer,
            version,
            block_size,
            buf: Vec::new(),
            buf_item_count: LayerWriterBufItemCount(0),
            item_count: 0,
            block_offsets: Vec::new(),
            block_keys: Vec::new(),
            bloom_filter: BloomFilterWriter::new(seed, num_items),
            _value: PhantomData,
        })
    }

    /// Writes `self.buf` out as a block.
    ///
    /// Blocks are fixed size, consisting of a 16-bit item count, data, zero padding
    /// and seek table at the end.
    async fn write_block(&mut self) -> Result<(), Error> {
        if *self.buf_item_count == 0 {
            return Ok(());
        }
        let seek_table_size = self.block_offsets.len() * PER_DATA_BLOCK_SEEK_ENTRY_SIZE;
        assert!(
            PER_DATA_BLOCK_HEADER_SIZE + seek_table_size + self.buf.len()
                <= self.block_size.get() as usize
        );
        let mut cursor = std::io::Cursor::new(vec![0u8; self.block_size.get() as usize]);
        cursor.write_u16::<LittleEndian>(*self.buf_item_count)?;
        cursor.write_all(&self.buf)?;
        cursor.set_position(self.block_size - seek_table_size as u64);
        // Write the seek table. Entries are 2 bytes each and items are always at least 10.
        for &offset in &self.block_offsets {
            cursor.write_u16::<LittleEndian>(offset)?;
        }
        self.writer.write_bytes(cursor.get_ref()).await?;
        debug!(item_count = *self.buf_item_count, byte_count = self.buf.len(); "wrote items");
        self.buf.clear();
        *self.buf_item_count = 0;
        self.block_offsets.clear();
        Ok(())
    }

    // Assumes the writer is positioned to a new block.
    // Returns the size, in bytes, of the seek table.
    // Note that the writer will be positioned to exactly the end of the seek table, not to the end
    // of a block.
    async fn write_seek_table(&mut self) -> Result<usize, Error> {
        let keys = if self.version <= OLD_KEY_SERIALIZATION_VERSION {
            self.block_keys.get(1..).unwrap_or(&[])
        } else {
            &self.block_keys
        };
        if keys.len() == 0 {
            return Ok(0);
        }
        let size = keys.len() * std::mem::size_of::<u64>();
        self.buf.resize(size, 0);
        let mut len = 0;
        for key in keys {
            LittleEndian::write_u64(&mut self.buf[len..len + std::mem::size_of::<u64>()], *key);
            len += std::mem::size_of::<u64>();
        }
        self.writer.write_bytes(&self.buf).await?;
        Ok(size)
    }

    // Assumes the writer is positioned to exactly the end of the seek table, which was
    // `seek_table_len` bytes.
    async fn write_info(
        &mut self,
        num_data_blocks: u64,
        bloom_filter_size_bytes: usize,
        seek_table_len: usize,
    ) -> Result<(), Error> {
        let block_size = self.writer.block_size().get() as usize;
        let layer_info = LayerInfo {
            num_items: self.item_count,
            num_data_blocks,
            bloom_filter_size_bytes,
            bloom_filter_seed: self.bloom_filter.seed(),
            bloom_filter_num_hashes: self.bloom_filter.num_hashes(),
        };
        self.buf.clear();
        layer_info.serialize_into(&mut self.buf)?;
        let layer_info_len = self.buf.len() as u64;
        self.buf.write_u64::<LittleEndian>(layer_info_len)?;
        let actual_len = self.buf.len();

        // We want the LayerInfo to be at the end of the last block.  That might require creating a
        // new block if we don't have enough room.
        let avail_in_block =
            block_size - (seek_table_len as u64 % self.writer.block_size()) as usize;
        let to_skip = if avail_in_block < actual_len {
            block_size + avail_in_block - actual_len
        } else {
            avail_in_block - actual_len
        };
        self.buf.resize(to_skip + actual_len, 0);
        self.buf.copy_within(0..actual_len, to_skip);
        self.buf[..to_skip].fill(0);
        self.writer.write_bytes(&self.buf).await?;
        Ok(())
    }

    // Assumes the writer is positioned to a new block.
    // Returns the size of the bloom filter, in bytes.
    async fn write_bloom_filter(&mut self) -> Result<usize, Error> {
        if self.data_blocks() == 0 || self.bloom_filter.serialized_size() == 0 {
            return Ok(0);
        }
        // TODO(https://fxbug.dev/323571978): Avoid bounce-buffering.
        let size =
            self.block_size.align_up(self.bloom_filter.serialized_size() as u64).unwrap() as usize;
        self.buf.resize(size, 0);
        let mut cursor = std::io::Cursor::new(&mut self.buf);
        self.bloom_filter.write(&mut cursor)?;
        self.writer.write_bytes(&self.buf).await?;
        Ok(self.bloom_filter.serialized_size())
    }

    // Returns the bloom filter writer. Intended to be used for testing purposes, e.g., gain access
    // to the bloom filter to then corrupt it.
    #[cfg(test)]
    pub(crate) fn bloom_filter(&mut self) -> &mut BloomFilterWriter<K> {
        &mut self.bloom_filter
    }

    fn serialize_item(&mut self, item: ItemRef<'_, K, V>) -> Result<(), Error> {
        if self.version <= OLD_KEY_SERIALIZATION_VERSION {
            item.key.serialize_into(&mut self.buf)?;
        } else {
            item.key.serialize_key_with_base_into(&mut self.buf, *self.block_keys.last().unwrap());
        }
        item.value.serialize_into(&mut self.buf)?;
        Ok(())
    }

    fn data_blocks(&self) -> usize {
        self.block_keys.len()
    }
}

impl<W: WriteBytes + Send, K: Key, V: LayerValue> LayerWriter<K, V>
    for PersistentLayerWriter<W, K, V>
{
    async fn write(&mut self, item: ItemRef<'_, K, V>) -> Result<(), Error> {
        // Note the length before we write this item.
        let len = self.buf.len();
        // Each data block's keys are delta-encoded relative to the leading u64 of that block's
        // first key (stored in `block_keys`). Record the base key for the first block here before
        // serializing; for subsequent blocks, `block_keys` is updated below when a block overflows.
        if self.block_keys.is_empty() {
            self.block_keys.push(item.key.get_leading_u64());
        }
        self.serialize_item(item)?;

        let mut added_offset = false;
        // Never record the first item. The offset is always the same.
        if *self.buf_item_count > 0 {
            self.block_offsets.push(u16::try_from(len + PER_DATA_BLOCK_HEADER_SIZE).unwrap());
            added_offset = true;
        }

        // If writing the item took us over a block, flush the bytes in the buffer prior to this
        // item.
        if PER_DATA_BLOCK_HEADER_SIZE
            + self.buf.len()
            + (self.block_offsets.len() * PER_DATA_BLOCK_SEEK_ENTRY_SIZE)
            > self.block_size.get() as usize - 1
        {
            if added_offset {
                // Drop the recently added offset from the list. The latest item will be the first
                // on the next block and have a known offset there.
                self.block_offsets.pop();
            }
            self.buf.truncate(len);
            self.write_block().await?;

            // Start a new block with `item` as its first entry and re-serialize using the new base.
            self.block_keys.push(item.key.get_leading_u64());
            self.serialize_item(item)?;
        }

        self.bloom_filter.insert(&item.key);
        *self.buf_item_count += 1;
        self.item_count += 1;
        Ok(())
    }

    async fn complete(mut self) -> Result<u64, Error> {
        self.write_block().await?;
        let data_blocks = self.data_blocks() as u64;
        let bloom_filter_len = self.write_bloom_filter().await?;
        let seek_table_len = self.write_seek_table().await?;
        self.write_info(data_blocks, bloom_filter_len, seek_table_len).await?;
        self.writer.complete().await
    }
}

/// Logs a warning if this object is dropped and the contained value isn't 0.
#[repr(transparent)]
struct LayerWriterBufItemCount(u16);

impl Drop for LayerWriterBufItemCount {
    fn drop(&mut self) {
        debug_assert!(self.0 == 0, "Dropping unwritten items; did you forget to call complete?");
        if self.0 > 0 {
            warn!("Dropping unwritten items; did you forget to call complete?");
        }
    }
}

impl std::ops::Deref for LayerWriterBufItemCount {
    type Target = u16;
    fn deref(&self) -> &u16 {
        &self.0
    }
}

impl std::ops::DerefMut for LayerWriterBufItemCount {
    fn deref_mut(&mut self) -> &mut u16 {
        &mut self.0
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BlockSize, FxfsError, LayerData, NUM_HEADER_BLOCKS, PersistentLayer, PersistentLayerWriter,
        SyncPersistentLayer,
    };
    use crate::filesystem::MAX_BLOCK_SIZE;
    use crate::lsm_tree::LayerIterator;
    use crate::lsm_tree::testing::TestKey;
    use crate::lsm_tree::types::{
        Existence, Item, ItemRef, Layer, LayerWriter, MaybeContainsKey, OrdUpperBound,
    };
    use crate::object_handle::{
        LayerObject, ObjectHandle, ReadObjectHandle, WriteBytes, WriteObjectHandle,
    };
    use crate::object_store::AttributeId;
    use crate::object_store::allocator::AllocatorKey;
    use crate::object_store::extent::{Extent, MIN_BLOCK_SIZE};
    use crate::object_store::object_record::ObjectKey;
    use crate::round::round_up;
    use crate::serialized_types::OLD_KEY_SERIALIZATION_VERSION;
    use crate::testing::fake_object::{FakeObject, FakeObjectHandle};
    use crate::testing::writer::Writer;
    use anyhow::Error;
    use async_trait::async_trait;
    use std::fmt::Debug;
    use std::ops::{Bound, Range};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use storage_device::buffer::{BufferFuture, MutableBufferRef};

    impl<W: WriteBytes> Debug for PersistentLayerWriter<W, i32, i32> {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
            f.debug_struct("rPersistentLayerWriter")
                .field("block_size", &self.block_size)
                .field("item_count", &*self.buf_item_count)
                .finish()
        }
    }

    #[fuchsia::test]
    async fn test_iterate_after_write() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 10000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize * 4,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i, i).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        let mut iterator = layer.seek(Bound::Unbounded).await.expect("seek failed");
        for i in 0..ITEM_COUNT {
            let ItemRef { key, value, .. } = iterator.get().expect("missing item");
            assert_eq!((key, value), (&i, &i));
            iterator.advance().await.expect("failed to advance");
        }
        assert!(iterator.get().is_none());
    }

    #[fuchsia::test]
    async fn test_seek_after_write() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 5000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize * 18,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                // Populate every other value as an item.
                writer.write(Item::new(i * 2, i * 2).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        // Search for all values to check the in-between values.
        for i in 0..ITEM_COUNT * 2 {
            // We've written every other value, we expect to get either the exact value searched
            // for, or the next one after it. So round up to the nearest multiple of 2.
            let expected = round_up(i, 2).unwrap();
            let mut iterator = layer.seek(Bound::Included(&i)).await.expect("failed to seek");
            // We've written values up to (N-1)*2=2*N-2, so when looking for 2*N-1 we'll go off the
            // end of the layer and get back no item.
            if i >= (ITEM_COUNT * 2) - 1 {
                assert!(iterator.get().is_none());
            } else {
                let ItemRef { key, value, .. } = iterator.get().expect("missing item");
                assert_eq!((key, value), (&expected, &expected));
            }

            // Check that we can advance to the next item.
            iterator.advance().await.expect("failed to advance");
            // The highest value is 2*N-2, searching for 2*N-3 will find the last value, and
            // advancing will go off the end of the layer and return no item. If there was
            // previously no item, then it will latch and always return no item.
            if i >= (ITEM_COUNT * 2) - 3 {
                assert!(iterator.get().is_none());
            } else {
                let ItemRef { key, value, .. } = iterator.get().expect("missing item");
                let next = expected + 2;
                assert_eq!((key, value), (&next, &next));
            }
        }
    }

    #[fuchsia::test]
    async fn test_seek_unbounded() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 1000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize * 18,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i, i).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        let mut iterator = layer.seek(Bound::Unbounded).await.expect("failed to seek");
        let ItemRef { key, value, .. } = iterator.get().expect("missing item");
        assert_eq!((key, value), (&0, &0));

        // Check that we can advance to the next item.
        iterator.advance().await.expect("failed to advance");
        let ItemRef { key, value, .. } = iterator.get().expect("missing item");
        assert_eq!((key, value), (&1, &1));
    }

    #[fuchsia::test]
    async fn test_zero_items() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                0,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            writer.complete().await.expect("flush failed");
        }

        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        let iterator = (layer.as_ref() as &dyn Layer<i32, i32>)
            .seek(Bound::Unbounded)
            .await
            .expect("seek failed");
        assert!(iterator.get().is_none())
    }

    #[fuchsia::test]
    async fn test_one_item() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                1,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            writer.write(Item::new(42, 42).as_item_ref()).await.expect("write failed");
            writer.complete().await.expect("flush failed");
        }

        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        {
            let mut iterator = (layer.as_ref() as &dyn Layer<i32, i32>)
                .seek(Bound::Unbounded)
                .await
                .expect("seek failed");
            let ItemRef { key, value, .. } = iterator.get().expect("missing item");
            assert_eq!((key, value), (&42, &42));
            iterator.advance().await.expect("failed to advance");
            assert!(iterator.get().is_none())
        }
        {
            let mut iterator = (layer.as_ref() as &dyn Layer<i32, i32>)
                .seek(Bound::Included(&30))
                .await
                .expect("seek failed");
            let ItemRef { key, value, .. } = iterator.get().expect("missing item");
            assert_eq!((key, value), (&42, &42));
            iterator.advance().await.expect("failed to advance");
            assert!(iterator.get().is_none())
        }
        {
            let mut iterator = (layer.as_ref() as &dyn Layer<i32, i32>)
                .seek(Bound::Included(&42))
                .await
                .expect("seek failed");
            let ItemRef { key, value, .. } = iterator.get().expect("missing item");
            assert_eq!((key, value), (&42, &42));
            iterator.advance().await.expect("failed to advance");
            assert!(iterator.get().is_none())
        }
        {
            let iterator = (layer.as_ref() as &dyn Layer<i32, i32>)
                .seek(Bound::Included(&43))
                .await
                .expect("seek failed");
            assert!(iterator.get().is_none())
        }
    }

    #[fuchsia::test]
    async fn test_large_block_size() {
        // At the upper end of the supported size.
        const BLOCK_SIZE: BlockSize = MAX_BLOCK_SIZE;
        // Items will be 18 bytes, so fill up a few pages.
        let item_count: i32 = ((BLOCK_SIZE.get() as i32) / 18) * 3;

        let handle = FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                item_count as usize * 18,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            // Use large values to force varint encoding to use consistent space.
            for i in 2000000000..(2000000000 + item_count) {
                writer.write(Item::new(i, i).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }

        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        let mut iterator = layer.seek(Bound::Unbounded).await.expect("seek failed");
        for i in 2000000000..(2000000000 + item_count) {
            let ItemRef { key, value, .. } = iterator.get().expect("missing item");
            assert_eq!((key, value), (&i, &i));
            iterator.advance().await.expect("failed to advance");
        }
        assert!(iterator.get().is_none());
    }

    #[fuchsia::test]
    async fn test_overlarge_block_size() {
        // At the upper end of the supported size.
        const BLOCK_SIZE: BlockSize = BlockSize::from_u64(MAX_BLOCK_SIZE.size() * 2).unwrap();

        let handle = FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        PersistentLayerWriter::<_, i32, i32>::new(Writer::new(&handle).await, 0, BLOCK_SIZE)
            .await
            .expect_err("Creating writer with overlarge block size.");
    }

    #[fuchsia::test]
    async fn test_seek_bound_excluded() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 10000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize * 18,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i, i).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");

        for i in 9982..ITEM_COUNT {
            let mut iterator = layer.seek(Bound::Excluded(&i)).await.expect("failed to seek");
            let i_plus_one = i + 1;
            if i_plus_one < ITEM_COUNT {
                let ItemRef { key, value, .. } = iterator.get().expect("missing item");

                assert_eq!((key, value), (&i_plus_one, &i_plus_one));

                // Check that we can advance to the next item.
                iterator.advance().await.expect("failed to advance");
                let i_plus_two = i + 2;
                if i_plus_two < ITEM_COUNT {
                    let ItemRef { key, value, .. } = iterator.get().expect("missing item");
                    assert_eq!((key, value), (&i_plus_two, &i_plus_two));
                } else {
                    assert!(iterator.get().is_none());
                }
            } else {
                assert!(iterator.get().is_none());
            }
        }
    }

    /// Generates extent records for a given object_id (of size 1).
    /// This produces a series of records with the same leading_u64.
    /// Returns the generated items and the next available object_id.
    fn generate_extents(
        object_id: u64,
        base_offset: u64,
        count: u64,
    ) -> (Vec<Item<ObjectKey, u64>>, u64) {
        let mut items = Vec::new();
        for i in 0..count {
            items.push(Item::new(
                ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_offset + i) * MIN_BLOCK_SIZE..(base_offset + i + 1) * MIN_BLOCK_SIZE,
                ),
                object_id,
            ));
        }
        (items, object_id + 1)
    }

    /// Generates objects object_ids over a range.
    /// This produced a series of records with unique leading_u64.
    /// Returns the generated items and the next available value for sequencing.
    fn generate_objects(object_id_range: Range<u64>) -> (Vec<Item<ObjectKey, u64>>, u64) {
        let mut items = Vec::new();
        let end = object_id_range.end;
        for object_id in object_id_range {
            items.push(Item::new(ObjectKey::object(object_id), object_id));
        }
        (items, end)
    }

    // Create a large spread of data across several blocks to ensure that no part of the range is
    // lost by the partial search using the layer seek table.
    #[fuchsia::test]
    async fn test_block_seek_duplicate_leading_u64() {
        // At the upper end of the supported size.
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEMS_PER_PHASE: u64 = 50;

        let mut to_find = Vec::new();

        let handle = FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        {
            let mut items = Vec::new();
            // Make all values take up maximum space for varint encoding.
            let mut object_id = u32::MAX as u64 + 1;

            // First fill the front with duplicate object IDs, then look at the start,
            // middle and end of the range.
            {
                let base_extent_offset = 0;
                let (mut generated, next_object_id) =
                    generate_extents(object_id, base_extent_offset, ITEMS_PER_PHASE * 3);
                items.append(&mut generated);
                let count = ITEMS_PER_PHASE * 3;
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    base_extent_offset * MIN_BLOCK_SIZE..(base_extent_offset + 1) * MIN_BLOCK_SIZE,
                ));
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_extent_offset + count / 2) * MIN_BLOCK_SIZE
                        ..(base_extent_offset + count / 2 + 1) * MIN_BLOCK_SIZE,
                ));
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_extent_offset + count - 1) * MIN_BLOCK_SIZE
                        ..(base_extent_offset + count) * MIN_BLOCK_SIZE,
                ));
                object_id = next_object_id;
            }

            // Add some filler of all different leading u64.
            {
                let (mut generated, next_object_id) =
                    generate_objects(object_id..object_id + ITEMS_PER_PHASE * 3);
                items.append(&mut generated);
                object_id = next_object_id;
            }

            // Fill the middle with duplicate object IDs, then look at the start,
            // middle and end of the range.
            {
                let base_extent_offset = 1000;
                let (mut generated, next_object_id) =
                    generate_extents(object_id, base_extent_offset, ITEMS_PER_PHASE * 3);
                items.append(&mut generated);
                let count = ITEMS_PER_PHASE * 3;
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    base_extent_offset * MIN_BLOCK_SIZE..(base_extent_offset + 1) * MIN_BLOCK_SIZE,
                ));
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_extent_offset + count / 2) * MIN_BLOCK_SIZE
                        ..(base_extent_offset + count / 2 + 1) * MIN_BLOCK_SIZE,
                ));
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_extent_offset + count - 1) * MIN_BLOCK_SIZE
                        ..(base_extent_offset + count) * MIN_BLOCK_SIZE,
                ));
                object_id = next_object_id;
            }

            // Add some filler of all different leading u64.
            {
                let (mut generated, next_object_id) =
                    generate_objects(object_id..object_id + ITEMS_PER_PHASE * 3);
                items.append(&mut generated);
                object_id = next_object_id;
            }

            // Fill the end with duplicate object IDs, then look at the start,
            // middle and end of the range.
            {
                let base_extent_offset = 2000;
                let (mut generated, _) =
                    generate_extents(object_id, base_extent_offset, ITEMS_PER_PHASE * 3);
                items.append(&mut generated);
                let count = ITEMS_PER_PHASE * 3;
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    base_extent_offset * MIN_BLOCK_SIZE..(base_extent_offset + 1) * MIN_BLOCK_SIZE,
                ));
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_extent_offset + count / 2) * MIN_BLOCK_SIZE
                        ..(base_extent_offset + count / 2 + 1) * MIN_BLOCK_SIZE,
                ));
                to_find.push(ObjectKey::extent(
                    object_id,
                    AttributeId::TEST_ID,
                    (base_extent_offset + count - 1) * MIN_BLOCK_SIZE
                        ..(base_extent_offset + count) * MIN_BLOCK_SIZE,
                ));
            }

            // Sort items by cmp_upper_bound!
            items.sort_by(|a, b| a.key.cmp_upper_bound(&b.key));

            let mut writer = PersistentLayerWriter::<_, ObjectKey, u64>::new(
                Writer::new(&handle).await,
                (3 * BLOCK_SIZE) as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");

            for item in items {
                writer.write(item.as_item_ref()).await.expect("write failed");
            }

            writer.complete().await.expect("flush failed");
        }

        let layer = PersistentLayer::<ObjectKey, u64>::open(handle).await.expect("new failed");
        for target in to_find {
            let iterator = layer.seek(Bound::Included(&target)).await.expect("failed to seek");
            let ItemRef { key, .. } = iterator.get().expect("missing item");
            assert_eq!(&target, key);
        }
    }

    #[fuchsia::test]
    async fn test_two_seek_blocks() {
        // At the upper end of the supported size.
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEMS_PER_PHASE: u64 = 50;
        const ITEM_COUNT: u64 = ITEMS_PER_PHASE * ((BLOCK_SIZE.size() / 8) + 2);

        let mut to_find = Vec::new();

        let handle = FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        {
            let mut writer = PersistentLayerWriter::<_, TestKey, u64>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize * 18,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");

            // Make all values take up maximum space for varint encoding.
            let initial_value = u32::MAX as u64 + 1;
            for i in 0..ITEM_COUNT {
                writer
                    .write(
                        Item::new(TestKey(initial_value + i..initial_value + i), initial_value)
                            .as_item_ref(),
                    )
                    .await
                    .expect("write failed");
            }
            // Look at the start middle and end.
            to_find.push(TestKey(initial_value..initial_value));
            let middle = initial_value + ITEM_COUNT / 2;
            to_find.push(TestKey(middle..middle));
            let end = initial_value + ITEM_COUNT - 1;
            to_find.push(TestKey(end..end));

            writer.complete().await.expect("flush failed");
        }

        let layer = PersistentLayer::<TestKey, u64>::open(handle).await.expect("new failed");
        for target in to_find {
            let iterator = layer.seek(Bound::Included(&target)).await.expect("failed to seek");
            let ItemRef { key, .. } = iterator.get().expect("missing item");
            assert_eq!(&target, key);
        }
    }

    // Verifies behaviour around creating full seek blocks, to ensure that it is able to be opened
    // and parsed afterward.
    #[fuchsia::test]
    async fn test_full_seek_block() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEMS_PER_PHASE: u64 = 50;

        // How many entries there are in a seek table block.
        const SEEK_TABLE_ENTRIES: u64 = BLOCK_SIZE.size() / 8;

        // Number of entries to fill a seek block would need one more block of entries, but we're
        // starting low here on purpose to do a range and make sure we hit the size we are
        // interested in.
        const START_ENTRIES_COUNT: u64 = ITEMS_PER_PHASE * SEEK_TABLE_ENTRIES;

        for entries in START_ENTRIES_COUNT..START_ENTRIES_COUNT + (ITEMS_PER_PHASE * 2) {
            let handle =
                FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
            {
                let mut writer = PersistentLayerWriter::<_, TestKey, u64>::new(
                    Writer::new(&handle).await,
                    entries as usize,
                    BLOCK_SIZE,
                )
                .await
                .expect("writer new");

                // Make all values take up maximum space for varint encoding.
                let initial_value = u32::MAX as u64 + 1;
                for i in 0..entries {
                    writer
                        .write(
                            Item::new(TestKey(initial_value + i..initial_value + i), initial_value)
                                .as_item_ref(),
                        )
                        .await
                        .expect("write failed");
                }

                writer.complete().await.expect("flush failed");
            }
            PersistentLayer::<TestKey, u64>::open(handle).await.expect("new failed");
        }
    }

    #[fuchsia::test]
    async fn test_ignore_bloom_filter_on_older_versions() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: u64 = 250;

        let old_version_handle =
            FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        let current_version_handle =
            FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        // Make all values take up maximum space for varint encoding.
        let initial_value = u32::MAX as u64 + 1;
        {
            let mut old_version_writer =
                PersistentLayerWriter::<_, TestKey, u64>::new_with_version(
                    Writer::new(&old_version_handle).await,
                    ITEM_COUNT as usize,
                    BLOCK_SIZE,
                    OLD_KEY_SERIALIZATION_VERSION,
                )
                .await
                .expect("writer new");
            let mut current_version_writer = PersistentLayerWriter::<_, TestKey, u64>::new(
                Writer::new(&current_version_handle).await,
                ITEM_COUNT as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");

            for i in 0..ITEM_COUNT {
                old_version_writer
                    .write(
                        Item::new(TestKey(initial_value + i..initial_value + i), initial_value)
                            .as_item_ref(),
                    )
                    .await
                    .expect("write failed");
                current_version_writer
                    .write(
                        Item::new(TestKey(initial_value + i..initial_value + i), initial_value)
                            .as_item_ref(),
                    )
                    .await
                    .expect("write failed");
            }

            old_version_writer.complete().await.expect("flush failed");
            current_version_writer.complete().await.expect("flush failed");
        }

        let old_layer =
            PersistentLayer::<TestKey, u64>::open(old_version_handle).await.expect("open failed");
        let current_layer = PersistentLayer::<TestKey, u64>::open(current_version_handle)
            .await
            .expect("open failed");
        assert!(!old_layer.has_bloom_filter());
        assert!(current_layer.has_bloom_filter());

        // Verify seeking works in both layers (including before the first block's key).
        let iter = old_layer.seek(Bound::Included(&TestKey(0..0))).await.expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key.0.start, initial_value);

        let iter = current_layer.seek(Bound::Included(&TestKey(0..0))).await.expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key.0.start, initial_value);

        let iter = old_layer.seek(Bound::Unbounded).await.expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key.0.start, initial_value);

        let iter = current_layer.seek(Bound::Unbounded).await.expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key.0.start, initial_value);

        let target_val = initial_value + ITEM_COUNT / 2;
        let iter = old_layer
            .seek(Bound::Included(&TestKey(target_val..target_val)))
            .await
            .expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key.0.start, target_val);

        let iter = current_layer
            .seek(Bound::Included(&TestKey(target_val..target_val)))
            .await
            .expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key.0.start, target_val);
    }

    #[fuchsia::test]
    async fn test_allocator_key_older_version_seek() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: u64 = 100;

        let old_version_handle =
            FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        let current_version_handle =
            FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        let step = MIN_BLOCK_SIZE.get();
        {
            let mut old_version_writer =
                PersistentLayerWriter::<_, AllocatorKey, i64>::new_with_version(
                    Writer::new(&old_version_handle).await,
                    ITEM_COUNT as usize,
                    BLOCK_SIZE,
                    OLD_KEY_SERIALIZATION_VERSION,
                )
                .await
                .expect("writer new");
            let mut current_version_writer = PersistentLayerWriter::<_, AllocatorKey, i64>::new(
                Writer::new(&current_version_handle).await,
                ITEM_COUNT as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");

            for i in 0..ITEM_COUNT {
                let key = AllocatorKey { device_range: Extent(i * step..(i + 1) * step) };
                old_version_writer
                    .write(Item::new(key.clone(), i as i64).as_item_ref())
                    .await
                    .expect("write failed");
                current_version_writer
                    .write(Item::new(key, i as i64).as_item_ref())
                    .await
                    .expect("write failed");
            }

            old_version_writer.complete().await.expect("flush failed");
            current_version_writer.complete().await.expect("flush failed");
        }

        let old_layer = PersistentLayer::<AllocatorKey, i64>::open(old_version_handle)
            .await
            .expect("open failed");
        let current_layer = PersistentLayer::<AllocatorKey, i64>::open(current_version_handle)
            .await
            .expect("open failed");

        let target_idx = ITEM_COUNT / 2;
        let target_key =
            AllocatorKey { device_range: Extent(target_idx * step..(target_idx + 1) * step) };

        let iter = old_layer.seek(Bound::Included(&target_key)).await.expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key, &target_key);

        let iter = current_layer.seek(Bound::Included(&target_key)).await.expect("seek failed");
        let item = iter.get().expect("missing item");
        assert_eq!(item.key, &target_key);
    }

    #[fuchsia::test]
    async fn test_small_layer_has_bloom_filter() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_8KIB;
        const ITEM_COUNT: i32 = 10;

        let handle = FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i * 2, i * 2).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer =
            PersistentLayer::<i32, i32>::open_async(Arc::new(handle)).await.expect("new failed");
        assert_eq!(layer.data.data_size, BLOCK_SIZE.get());
        assert!(layer.has_bloom_filter());
        let mut missing_count = 0;
        for i in 0..ITEM_COUNT {
            assert_eq!(
                layer.key_exists(&(i * 2)).await.expect("key_exists failed"),
                Existence::MaybeExists
            );
            if layer.key_exists(&(i * 2 + 1)).await.expect("key_exists failed")
                == Existence::Missing
            {
                missing_count += 1;
            }
        }
        assert!(missing_count > ITEM_COUNT / 2);
    }

    #[fuchsia::test]
    async fn test_key_exists_with_bloom_filter() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 10000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i * 2, i * 2).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("new failed");
        assert!(layer.has_bloom_filter());

        for i in 0..ITEM_COUNT {
            // With a bloom filter, we expect MaybeExists for present keys.
            assert_eq!(
                layer.key_exists(&(i * 2)).await.expect("key_exists failed"),
                Existence::MaybeExists
            );
        }

        // For missing keys, we expect Missing, but might get MaybeExists due to false positives.
        // We can at least assert it's NOT Exists.
        let mut missing_count = 0;
        for i in 0..ITEM_COUNT {
            let result = layer.key_exists(&(i * 2 + 1)).await.expect("key_exists failed");
            assert_ne!(result, Existence::Exists);
            if result == Existence::Missing {
                missing_count += 1;
            }
        }
        // We expect mostly Missing.
        assert!(missing_count > ITEM_COUNT / 2);
    }

    #[fuchsia::test]
    async fn test_load_large_bloom_filter_multi_chunk() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        // Sizing for 600_000 items creates a 2 MiB bloom filter (exceeding 1 MiB chunk size).
        const ESTIMATED_ITEMS: usize = 600_000;
        const WRITTEN_ITEMS: i32 = 2000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ESTIMATED_ITEMS,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..WRITTEN_ITEMS {
                writer.write(Item::new(i * 2, i * 2).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let layer = PersistentLayer::<i32, i32>::open(handle).await.expect("open failed");
        assert!(layer.has_bloom_filter());

        for i in 0..WRITTEN_ITEMS {
            assert_eq!(layer.maybe_contains_key(&(i * 2)), MaybeContainsKey::Maybe);
        }
        let mut false_count = 0;
        for i in 0..WRITTEN_ITEMS {
            if layer.maybe_contains_key(&(i * 2 + 1)) == MaybeContainsKey::False {
                false_count += 1;
            }
        }
        assert!(false_count > WRITTEN_ITEMS / 2);
    }

    #[fuchsia::test]
    async fn test_clear_cached_data() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                100,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            writer.write(Item::new(1, 1).as_item_ref()).await.expect("write failed");
            writer.complete().await.expect("flush failed");
        }
        let layer =
            PersistentLayer::<i32, i32>::open_async(Arc::new(handle)).await.expect("open failed");
        let iter = layer.seek(Bound::Unbounded).await.expect("seek failed");
        assert_eq!(iter.get().map(|i| (*i.key, *i.value)), Some((1, 1)));
        drop(iter);

        assert!(layer.object_handle.try_read(BLOCK_SIZE.get() as usize).is_some());

        layer.clear_cached_data();

        assert!(layer.object_handle.try_read(BLOCK_SIZE.get() as usize).is_none());
    }

    struct SliceLayerObject {
        handle: FakeObjectHandle,
        slice: Vec<u8>,
        io_error: AtomicBool,
        purged: AtomicBool,
        closed: AtomicBool,
    }

    impl ObjectHandle for SliceLayerObject {
        fn object_id(&self) -> u64 {
            self.handle.object_id()
        }
        fn block_size(&self) -> BlockSize {
            self.handle.block_size()
        }
        fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
            self.handle.allocate_buffer(size)
        }
    }

    #[async_trait]
    impl ReadObjectHandle for SliceLayerObject {
        async fn read_aligned(
            &self,
            offset: u64,
            buf: MutableBufferRef<'_>,
        ) -> Result<usize, Error> {
            self.handle.read_aligned(offset, buf).await
        }
        fn get_size(&self) -> u64 {
            self.handle.get_size()
        }
    }

    #[async_trait]
    impl LayerObject for SliceLayerObject {
        fn as_slice(&self) -> Option<&[u8]> {
            Some(&self.slice)
        }
        fn has_io_error(&self) -> bool {
            self.io_error.load(Ordering::SeqCst)
        }
        fn purge_cached_data(&self) {
            self.purged.store(true, Ordering::SeqCst);
        }
        async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
    }

    #[fuchsia::test]
    async fn test_sync_persistent_layer() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 1000;

        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i * 2, i * 2).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }

        let size = handle.get_size() as usize;
        let mut buf = handle.allocate_buffer(size).await;
        handle.read_aligned(0, buf.as_mut()).await.expect("read failed");
        let slice = buf.subslice(..).to_vec();
        drop(buf);
        let slice_obj = Arc::new(SliceLayerObject {
            handle,
            slice: slice.clone(),
            io_error: AtomicBool::new(false),
            purged: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });

        let layer: Arc<dyn Layer<i32, i32>> =
            SyncPersistentLayer::<i32, i32>::open(slice_obj.clone()).expect("open failed");
        assert!(layer.has_bloom_filter());
        assert_eq!(layer.len(), ITEM_COUNT as usize);

        // Unbounded iteration should complete synchronously (advance_dyn returns Ok(None)).
        let mut iter = layer.seek(Bound::Unbounded).await.expect("seek failed");
        for i in 0..ITEM_COUNT {
            let item = iter.get().expect("expected item");
            assert_eq!(*item.key, i * 2);
            assert_eq!(*item.value, i * 2);
            let fut = iter.advance_dyn().expect("advance_dyn failed");
            assert!(fut.is_none(), "SyncPersistentLayer iterator must advance synchronously");
        }
        assert!(iter.get().is_none());

        // Seek Included and Excluded.
        let iter = layer.seek(Bound::Included(&500)).await.expect("seek included failed");
        assert_eq!(*iter.get().expect("expected item").key, 500);

        let iter = layer.seek(Bound::Excluded(&500)).await.expect("seek excluded failed");
        assert_eq!(*iter.get().expect("expected item").key, 502);

        let iter = layer.seek(Bound::Included(&501)).await.expect("seek non-existent failed");
        assert_eq!(*iter.get().expect("expected item").key, 502);

        // Verify purge_cached_data delegates to LayerObject::purge_cached_data.
        assert!(!slice_obj.purged.load(Ordering::SeqCst));
        layer.purge_cached_data();
        assert!(slice_obj.purged.load(Ordering::SeqCst));

        // Verify close delegates to LayerObject::close.
        assert!(!slice_obj.closed.load(Ordering::SeqCst));
        layer.close().await;
        assert!(slice_obj.closed.load(Ordering::SeqCst));

        // Simulate zero-filled data block on disk corruption vs pager I/O error.
        let mut zeroed_slice = slice;
        let bs = BLOCK_SIZE.get() as usize;
        zeroed_slice[bs..bs * 2].fill(0);
        let zeroed_obj = Arc::new(SliceLayerObject {
            handle: FakeObjectHandle::new(Arc::new(FakeObject::new())),
            slice: zeroed_slice,
            io_error: AtomicBool::new(false),
            purged: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });
        // Write valid header/footer into the underlying FakeObjectHandle so LayerData::open
        // succeeds.
        let mut wbuf = zeroed_obj.handle.allocate_buffer(zeroed_obj.slice.len()).await;
        wbuf.as_mut_ptr_slice().copy_from_slice(&zeroed_obj.slice);
        zeroed_obj.handle.write_or_append(Some(0), wbuf.as_ref()).await.unwrap();
        drop(wbuf);
        let zeroed_layer =
            PersistentLayer::<i32, i32>::open_layer(zeroed_obj.clone()).await.expect("open failed");

        // When `has_io_error()` is false, corruption returns `FxfsError::Inconsistent`
        // (`ZX_ERR_IO_DATA_INTEGRITY`).
        let err = zeroed_layer.seek(Bound::Unbounded).await.err().expect("seek should fail");
        assert!(FxfsError::Inconsistent.matches(&err), "expected Inconsistent, got {err:?}");

        // When `has_io_error()` is true (signaled by `SUPPLY_ZEROES_ON_ERROR`), it returns
        // `zx_status::Status::IO` (`ZX_ERR_IO`).
        zeroed_obj.io_error.store(true, Ordering::SeqCst);
        let err = zeroed_layer.seek(Bound::Unbounded).await.err().expect("seek should fail");
        assert_eq!(
            err.root_cause().downcast_ref::<zx_status::Status>(),
            Some(&zx_status::Status::IO)
        );
    }

    #[fuchsia::test]
    async fn test_open_from_slice() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_512B;
        const ITEM_COUNT: i32 = 500;

        // 1. Handle without `as_slice()` returns `InvalidArgs`.
        let non_slice_handle: Arc<dyn LayerObject> =
            Arc::new(FakeObjectHandle::new(Arc::new(FakeObject::new())));
        let err = LayerData::<i32>::open_from_slice(&non_slice_handle)
            .err()
            .expect("open_from_slice should fail without slice");
        assert!(FxfsError::InvalidArgs.matches(&err));

        // 2. Populated layer (with bloom filter and multiple data/seek table blocks) matches
        //    `LayerData::open`.
        let handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                ITEM_COUNT as usize,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..ITEM_COUNT {
                writer.write(Item::new(i * 2, i * 2).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }
        let size = handle.get_size() as usize;
        let mut buf = handle.allocate_buffer(size).await;
        handle.read_aligned(0, buf.as_mut()).await.expect("read failed");
        let valid_slice = buf.subslice(..).to_vec();
        drop(buf);

        let make_slice_obj = |slice: Vec<u8>, io_error: bool| -> Arc<dyn LayerObject> {
            Arc::new(SliceLayerObject {
                handle: FakeObjectHandle::new(Arc::new(FakeObject::new())),
                slice,
                io_error: AtomicBool::new(io_error),
                purged: AtomicBool::new(false),
                closed: AtomicBool::new(false),
            })
        };

        let async_handle: Arc<dyn LayerObject> = Arc::new(handle);
        let expected_data = LayerData::<i32>::open(&async_handle).await.expect("open failed");
        let slice_data =
            LayerData::<i32>::open_from_slice(&make_slice_obj(valid_slice.clone(), false))
                .expect("open_from_slice failed");
        assert_eq!(slice_data.version, expected_data.version);
        assert_eq!(slice_data.block_size, expected_data.block_size);
        assert_eq!(slice_data.data_size, expected_data.data_size);
        assert_eq!(slice_data.seek_table, expected_data.seek_table);
        assert_eq!(slice_data.num_items, expected_data.num_items);
        assert!(slice_data.bloom_filter.is_some());
        for k in [0, 1, 250, 251, 998, 999] {
            assert_eq!(
                slice_data.bloom_filter.as_ref().unwrap().maybe_contains(&k),
                expected_data.bloom_filter.as_ref().unwrap().maybe_contains(&k)
            );
        }

        // 3. Valid slice when `has_io_error()` is true fails with `zx_status::Status::IO`.
        let err = LayerData::<i32>::open_from_slice(&make_slice_obj(valid_slice.clone(), true))
            .err()
            .expect("open_from_slice should fail on io_error");
        assert_eq!(
            err.root_cause().downcast_ref::<zx_status::Status>(),
            Some(&zx_status::Status::IO)
        );

        // 4. Empty (0-item) layer without bloom filter or seek table succeeds.
        let empty_handle = FakeObjectHandle::new(Arc::new(FakeObject::new()));
        {
            let writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&empty_handle).await,
                0,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            writer.complete().await.expect("flush failed");
        }
        let empty_size = empty_handle.get_size() as usize;
        let mut buf = empty_handle.allocate_buffer(empty_size).await;
        empty_handle.read_aligned(0, buf.as_mut()).await.expect("read failed");
        let empty_slice = buf.subslice(..).to_vec();
        drop(buf);
        let empty_data = LayerData::<i32>::open_from_slice(&make_slice_obj(empty_slice, false))
            .expect("open_from_slice on empty layer failed");
        assert_eq!(empty_data.num_items, 0);
        assert!(empty_data.seek_table.is_empty());
        assert!(empty_data.bloom_filter.is_none());

        // 5. Corruption cases return `FxfsError::Inconsistent` when `has_io_error()` is false, and
        //    `zx_status::Status::IO` when `has_io_error()` is true.
        let bs = BLOCK_SIZE.get() as usize;

        // 5a. Truncated slice (< block_size).
        let truncated = vec![0u8; bs / 2];

        // 5b. Invalid magic in header (after 4-byte Version prefix).
        let mut bad_magic = valid_slice.clone();
        bad_magic[4..12].fill(0);

        // 5c. Invalid LayerInfo length in last block.
        let mut bad_layer_info = valid_slice.clone();
        let len = bad_layer_info.len();
        bad_layer_info[len - 8..len].copy_from_slice(&u64::MAX.to_le_bytes());

        // 5d. Out-of-order seek table entries.
        let mut bad_seek_table = valid_slice.clone();
        let num_data_blocks = (slice_data.data_size / BLOCK_SIZE) as usize;
        assert!(num_data_blocks >= 2);
        let bf_blocks = BLOCK_SIZE
            .align_up_to_blocks(slice_data.bloom_filter_stats.as_ref().unwrap().size as u64)
            as usize;
        let seek_offset = (NUM_HEADER_BLOCKS as usize + num_data_blocks + bf_blocks) * bs;
        bad_seek_table[seek_offset..seek_offset + 8].copy_from_slice(&100u64.to_le_bytes());
        bad_seek_table[seek_offset + 8..seek_offset + 16].copy_from_slice(&50u64.to_le_bytes());

        for corrupted in [truncated, bad_magic, bad_layer_info, bad_seek_table] {
            let err = LayerData::<i32>::open_from_slice(&make_slice_obj(corrupted.clone(), false))
                .err()
                .expect("expected corruption error");
            assert!(FxfsError::Inconsistent.matches(&err), "expected Inconsistent, got {err:?}");

            let err = LayerData::<i32>::open_from_slice(&make_slice_obj(corrupted, true))
                .err()
                .expect("expected I/O error");
            assert_eq!(
                err.root_cause().downcast_ref::<zx_status::Status>(),
                Some(&zx_status::Status::IO)
            );
        }
    }

    #[fuchsia::test]
    async fn test_layer_block_size_gt_4k_falls_back_to_async_persistent_layer() {
        const BLOCK_SIZE: BlockSize = BlockSize::SIZE_8KIB;
        let handle = FakeObjectHandle::new_with_block_size(Arc::new(FakeObject::new()), BLOCK_SIZE);
        {
            let mut writer = PersistentLayerWriter::<_, i32, i32>::new(
                Writer::new(&handle).await,
                10,
                BLOCK_SIZE,
            )
            .await
            .expect("writer new");
            for i in 0..10 {
                writer.write(Item::new(i, i).as_item_ref()).await.expect("write failed");
            }
            writer.complete().await.expect("flush failed");
        }

        let size = handle.get_size() as usize;
        let mut buf = handle.allocate_buffer(size).await;
        handle.read_aligned(0, buf.as_mut()).await.expect("read failed");
        let slice = buf.subslice(..).to_vec();
        drop(buf);
        let slice_obj = Arc::new(SliceLayerObject {
            handle,
            slice,
            io_error: AtomicBool::new(false),
            purged: AtomicBool::new(false),
            closed: AtomicBool::new(false),
        });

        // SyncPersistentLayer::open must reject layers with block_size > 4KiB.
        assert!(SyncPersistentLayer::<i32, i32>::open(slice_obj.clone()).is_err());

        // PersistentLayer::open_layer must fall back to async PersistentLayer and succeed.
        let layer = PersistentLayer::<i32, i32>::open_layer(slice_obj).await.expect("open_layer");
        let mut iter = layer.seek(Bound::Unbounded).await.expect("seek");
        for i in 0..10 {
            assert_eq!(*iter.get().expect("item").key, i);
            iter.advance().await.expect("advance");
        }
        assert!(iter.get().is_none());
    }
}
