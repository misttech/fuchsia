// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::superblock::BLOCK_SIZE;
use anyhow::{Error, anyhow};
use fidl_fuchsia_io as fio;
use fsverity_merkle::FsVerityDescriptor as DecodedDescriptor;

/// Found in the last block of fsverity data.
pub struct FsVerityDescriptor<'a> {
    pub file_size: u64,
    pub algorithm: fio::HashAlgorithm,
    pub root: &'a [u8],
    pub salt: &'a [u8],
}

impl<'a> FsVerityDescriptor<'a> {
    /// Creates a descriptor from fuchsia.io VerificationOptions.
    pub fn from_verification_options(
        options: &'a fio::VerificationOptions,
        root: &'a [u8],
        file_size: u64,
    ) -> Result<Self, Error> {
        Ok(Self {
            file_size,
            algorithm: *options
                .hash_algorithm
                .as_ref()
                .ok_or_else(|| anyhow!("Missing hash algorithm"))?,
            root,
            salt: options.salt.as_ref().ok_or_else(|| anyhow!("Missing salt value"))?.as_slice(),
        })
    }

    /// Parses out the descriptor from bytes.
    pub fn from_bytes(data: &'a [u8]) -> Result<Self, Error> {
        let descriptor = DecodedDescriptor::from_bytes(data, BLOCK_SIZE)?;

        Ok(Self {
            file_size: descriptor.file_size() as u64,
            algorithm: descriptor.digest_algorithm(),
            root: descriptor.root_digest(),
            salt: descriptor.salt(),
        })
    }

    /// Create a fuchsia.io VerificationOptions to match this descriptor.
    pub fn fio_verification_options(&self) -> fio::VerificationOptions {
        fio::VerificationOptions {
            hash_algorithm: Some(self.algorithm),
            salt: Some(self.salt.to_vec()),
            ..Default::default()
        }
    }

    /// The amount of space needed to store the leaf nodes on fxfs. This is notably different in
    /// f2fs where the a single hash produces no leaf node data as only the root is used.
    pub fn leaf_node_size_fxfs(&self) -> u64 {
        // TODO(https://fxbug.dev/450398331): This can be cleaned up once fxfs stops generating
        // these digests.
        let digest_length = match self.algorithm {
            fio::HashAlgorithm::Sha256 => 32,
            fio::HashAlgorithm::Sha512 => 64,
            _ => unimplemented!("Only supporting SHA256 and SHA512"),
        };

        self.file_size.div_ceil(BLOCK_SIZE as u64) * digest_length
    }
}
