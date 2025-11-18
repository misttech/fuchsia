// Copyright 2018 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::Hash;
use std::io;

/// A `MerkleTree` contains levels of hashes that can be used to verify the integrity of data.
///
/// This struct is being deprecated and can be replaced with `fuchsia_merkle::root_from_slice` and
/// `fuchsia_merkle::root_from_reader`.
#[derive(Clone, Eq, PartialEq, Ord, PartialOrd, Hash, Debug)]
pub struct MerkleTree {
    /// The root of the merkle tree.
    ///
    /// All users of this struct are being migrated to `root_from_slice`, `root_from_reader`,
    /// `MerkleRootBuilder`, or `BufferedMerkleRootBuilder`. All current users of this struct only
    /// access to the root so only the root is stored.
    root: Hash,
}

impl MerkleTree {
    /// Creates a `MerkleTree`.
    pub(crate) fn from_root(root: Hash) -> Self {
        Self { root }
    }

    /// The root hash of the merkle tree.
    pub fn root(&self) -> Hash {
        self.root
    }

    /// Creates a `MerkleTree` from all of the bytes of a `Read`er.
    ///
    /// # Examples
    /// ```
    /// # use fuchsia_merkle::MerkleTree;
    /// let data_to_hash = [0xffu8; 8192];
    /// let tree = MerkleTree::from_reader(&data_to_hash[..]).unwrap();
    /// assert_eq!(
    ///     tree.root(),
    ///     "68d131bc271f9c192d4f6dcd8fe61bef90004856da19d0f2f514a7f4098b0737".parse().unwrap()
    /// );
    /// ```
    pub fn from_reader(mut reader: impl std::io::Read) -> Result<MerkleTree, io::Error> {
        crate::from_read(&mut reader)
    }
}
