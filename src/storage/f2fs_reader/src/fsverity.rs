// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::superblock::BLOCK_SIZE;
use anyhow::{Error, anyhow, ensure};
use fidl_fuchsia_io as fio;

fn digest_length(algorithm: &fio::HashAlgorithm) -> Result<usize, Error> {
    match algorithm {
        fio::HashAlgorithm::Sha256 => Ok(32),
        fio::HashAlgorithm::Sha512 => Ok(64),
        _ => Err(anyhow!("Unknown fio hash algorithm")),
    }
}

/// Found in the last block of fsverity data.
pub struct FsVerityDescriptor<'a> {
    pub file_size: u64,
    pub algorithm: fio::HashAlgorithm,
    pub root: &'a [u8],
    pub salt: &'a [u8],
}

impl<'a> FsVerityDescriptor<'a> {
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

    pub fn from_bytes(data: &'a [u8]) -> Result<Self, Error> {
        ensure!(data.len() >= 102, "Slice too short for descriptor");

        // Version 0..1
        ensure!(data[0] == 1, "Unsupported version {}", data[0]);

        // Algorithm 1..2
        let algorithm = match data[1] {
            1 => fio::HashAlgorithm::Sha256,
            2 => fio::HashAlgorithm::Sha512,
            _ => return Err(anyhow!("Unsupported hash version {}", data[1])),
        };

        // Block size 2..3
        // Merkle block size here doesn't necessarily need to match fs block size, but it is the
        // most efficient choice, greatly simplifies handling, and is the only supported choice in
        // the destination fxfs. It it stored in the descriptor as the log_2 of the value.
        ensure!((BLOCK_SIZE >> data[2] as usize) == 1, "Unsupported merkle block size {}", data[2]);

        // Salt size 3..4
        let salt_size = data[3] as usize;
        ensure!(salt_size <= 32, "Salt size to long {}", salt_size);

        // Reserved bytes 4..8

        // File size in little endian 8..16
        let file_size = u64::from_le_bytes(data[8..16].try_into().unwrap());

        // Digest root 16..80
        let root = &data[16..(16 + digest_length(&algorithm)?)];

        // Salt 80..102
        let salt = &data[80..(80 + salt_size)];

        Ok(Self { file_size, algorithm, root, salt })
    }

    /// Returns the offset where the start of the data is containing the fsverity descriptor.
    pub fn offset_from_size(file_size: u64) -> u64 {
        file_size.next_multiple_of(64 * 1024)
    }

    pub fn fio_verity_options(&self) -> fio::VerificationOptions {
        fio::VerificationOptions {
            hash_algorithm: Some(self.algorithm),
            salt: Some(self.salt.to_vec()),
            ..Default::default()
        }
    }

    /// The amount of space needed to store the leaf nodes on fxfs. This is notably different in
    /// f2fs where the a single hash produces no leaf node data as only the root is used.
    pub fn leaf_node_size_fxfs(&self) -> u64 {
        self.file_size.div_ceil(BLOCK_SIZE as u64) * digest_length(&self.algorithm).unwrap() as u64
    }
}
