// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use bssl_crypto::digest::Sha256;
use std::mem::{size_of, size_of_val};

use crate::{BLOCK_SIZE, HASH_SIZE, Hash};

pub(crate) const HASHES_PER_BLOCK: usize = BLOCK_SIZE / HASH_SIZE;

type BlockIdentity = [u8; size_of::<u64>() + size_of::<u32>()];

/// Generate the bytes representing a block's identity.
fn make_identity(length: usize, level: usize, offset: usize) -> BlockIdentity {
    let offset_or_level = (offset as u64 | level as u64).to_le_bytes();
    let length = (length as u32).to_le_bytes();
    let mut ret: BlockIdentity = [0; size_of::<BlockIdentity>()];
    let (ret_offset_or_level, ret_length) = ret.split_at_mut(size_of_val(&offset_or_level));
    ret_offset_or_level.copy_from_slice(&offset_or_level);
    ret_length.copy_from_slice(&length);
    ret
}

/// Compute the merkle hash of a block of data.
///
/// A merkle hash is the SHA-256 hash of a block of data with a small header built from the length
/// of the data, the level of the tree (0 for data blocks), and the offset into the level. The
/// block will be zero filled if its len is less than [`BLOCK_SIZE`], except for when the first
/// data block is completely empty.
///
/// # Panics
///
/// Panics if `block.len()` exceeds [`BLOCK_SIZE`] or if `offset` is not aligned to [`BLOCK_SIZE`]
pub(crate) fn hash_block(block: &[u8], offset: usize) -> Hash {
    assert!(block.len() <= BLOCK_SIZE);
    assert!(offset.is_multiple_of(BLOCK_SIZE));

    let mut hasher = Sha256::new();
    hasher.update(&make_identity(block.len(), 0, offset));
    hasher.update(block);
    // Zero fill block up to BLOCK_SIZE. As a special case, if the first data block is completely
    // empty, it is not zero filled.
    if block.len() != BLOCK_SIZE && !(block.is_empty() && offset == 0) {
        update_with_zeros(&mut hasher, BLOCK_SIZE - block.len());
    }

    Hash::from(hasher.digest())
}

/// Updates `hasher` with `count` zeros.
pub(crate) fn update_with_zeros(hasher: &mut Sha256, mut count: usize) {
    const BUF_SIZE: usize = 512;
    const ZEROS: [u8; BUF_SIZE] = [0; BUF_SIZE];
    while count >= BUF_SIZE {
        count -= BUF_SIZE;
        hasher.update(&ZEROS);
    }
    if count > 0 {
        hasher.update(&ZEROS[0..count]);
    }
}

/// Creates a new [`Sha256`] for hashing blocks of hashes. The hasher is initialized with the
/// block's identity.
pub(crate) fn make_hash_hasher(hashes_per_hash: usize, level: usize, offset: usize) -> Sha256 {
    debug_assert!(level > 0);
    let bytes_per_hash = hashes_per_hash * HASH_SIZE;
    debug_assert!(offset.is_multiple_of(bytes_per_hash));
    let mut hasher = Sha256::new();
    hasher.update(&make_identity(bytes_per_hash, level, offset));
    hasher
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_block_empty() {
        let block = [];
        let hash = hash_block(&block[..], 0);
        let expected =
            "15ec7bf0b50732b49f8228e07d24365338f9e3ab994b00af08e5a3bffe55fd8b".parse().unwrap();
        assert_eq!(hash, expected);
    }

    #[test]
    fn test_hash_block_single() {
        let block = vec![0xFF; 8192];
        let hash = hash_block(&block[..], 0);
        let expected =
            "68d131bc271f9c192d4f6dcd8fe61bef90004856da19d0f2f514a7f4098b0737".parse().unwrap();
        assert_eq!(hash, expected);
    }
}
