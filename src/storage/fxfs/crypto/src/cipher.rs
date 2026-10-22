// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use crate::{EncryptionKey, UnwrappedKey, WrappedKey};
use aes::cipher::inout::InOutBuf;
use aes::cipher::{
    Block, BlockCipherDecrypt, BlockModeDecrypt, BlockModeEncrypt, KeyInit, KeyIvInit,
};
use anyhow::{Error, ensure};
use std::collections::BTreeMap;
use std::sync::Arc;
pub use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
pub use storage_xts::{Tweak, XtsInPlaceProcessor, XtsProcessor};
use zerocopy::IntoBytes;
use zx_status as zx;

pub mod fscrypt_ino_lblk32;
pub mod fscrypt_ino_lblk64;
#[cfg(test)]
mod fscrypt_test_data;
pub(crate) mod fxfs;

// TODO(https://fxbug.dev/375700939): Support different padding sizes based on
// SET_ENCRYPTION_POLICY flags.
// Note: This constant is used in platform code. It would be nice to move all fscrypt
// internals into fxfs_lib and keep platform as simple as possible.
pub const FSCRYPT_PADDING: usize = 16;
// Fxfs will always use a block size >= 512 bytes, so we just assume a sector size of 512 bytes,
// which will work fine even if a different block size is used by Fxfs or the underlying device.
const SECTOR_SIZE: u64 = 512;
const BLOCK_SIZE: usize = 4096;
const MAX_FILENAME_LEN: usize = 255;
pub const MAX_SYMLINK_LEN: usize = 4093;

fn encrypt_filename_cts(
    cts_key: &[u8; 32],
    iv: &[u32; 4],
    buffer: &mut Vec<u8>,
    max_len: usize,
) -> Result<(), Error> {
    ensure!(buffer.len() <= max_len, "Filename too long");
    buffer.resize(buffer.len().next_multiple_of(FSCRYPT_PADDING), 0);

    let mut cbc = cbc::Encryptor::<aes::Aes256>::new(
        cts_key.try_into().unwrap(),
        iv.as_bytes().try_into().unwrap(),
    );
    let inout = InOutBuf::<'_, '_, u8>::from(&mut buffer[..]);
    let (mut blocks, _): (InOutBuf<'_, '_, Block<aes::Aes256>>, _) = inout.into_chunks();
    let mut chunks = blocks.get_out();
    cbc.encrypt_blocks(&mut chunks);
    if chunks.len() >= 2 {
        // We are encrypting with CTS.  In most cases, the padding will mean it's a multiple of
        // FSCRYPT_PADDING bytes, so all we need to do is swap the last two chunks.  There is one
        // exception: when the filename ends up being longer than max_len after padding.  In
        // that case, all we have to do is trim the end after swapping the last two chunks.
        chunks.swap(chunks.len() - 1, chunks.len() - 2);
        buffer.truncate(max_len);
    }
    Ok(())
}

fn decrypt_filename_cts(
    cts_key: &[u8; 32],
    iv: &[u32; 4],
    buffer: &mut Vec<u8>,
    max_len: usize,
) -> Result<(), Error> {
    let alignment = buffer.len() % FSCRYPT_PADDING;
    if alignment != 0 {
        // For CTS, the only case we need to care about is when the encrypted filename is
        // max_len bytes. In all other cases, the filename should be a multiple of FSCRYPT_PADDING
        // bytes.
        ensure!(buffer.len() == max_len, "Unexpected filename length");

        // Decrypt the second to last block.
        let cipher = aes::Aes256::new(cts_key.try_into().unwrap());
        let mut out: Block<aes::Aes256> =
            buffer[max_len - alignment - FSCRYPT_PADDING..max_len - alignment].try_into().unwrap();
        cipher.decrypt_block(&mut out);

        // Copy the extra bytes we need.
        buffer.extend_from_slice(&out[alignment..]);
    }

    let mut cbc = cbc::Decryptor::<aes::Aes256>::new(
        cts_key.try_into().unwrap(),
        iv.as_bytes().try_into().unwrap(),
    );
    let inout = InOutBuf::<'_, '_, u8>::from(&mut buffer[..]);
    let (mut blocks, _): (InOutBuf<'_, '_, Block<aes::Aes256>>, _) = inout.into_chunks();
    let mut chunks = blocks.get_out();
    if chunks.len() >= 2 {
        chunks.swap(chunks.len() - 1, chunks.len() - 2);
    }
    cbc.decrypt_blocks(&mut chunks);

    // Strip padding
    while let Some(0) = buffer.last() {
        buffer.pop();
    }
    Ok(())
}

/// Trait defining common methods shared across all ciphers.
pub trait Cipher: std::fmt::Debug + Send + Sync {
    /// Encrypts data in the `buffer`.
    ///
    /// * `offset` is the byte offset within the file.
    /// * `buffer` is mutated in place.
    ///
    /// `buffer` *must* be 16 byte aligned.
    fn encrypt(
        &self,
        ino: u64,
        attribute_id: u64,
        device_offset: u64,
        file_offset: u64,
        buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error>;

    /// Decrypt the data in `buffer`.
    ///
    /// * `offset` is the byte offset within the file.
    /// * `buffer` is mutated in place.
    ///
    /// `buffer` *must* be 16 byte aligned.
    fn decrypt(
        &self,
        ino: u64,
        attribute_id: u64,
        device_offset: u64,
        file_offset: u64,
        buffer: MutPtrByteSlice<'_>,
    ) -> Result<(), Error>;

    /// Decrypts data from `src` into `dst`.
    ///
    /// * `file_offset` is the byte offset within the file.
    /// * `src` and `dst` must have the same length.
    /// * `src` and `dst` must both be aligned to 64 bytes.
    fn decrypt_to(
        &self,
        ino: u64,
        attribute_id: u64,
        device_offset: u64,
        file_offset: u64,
        src: PtrByteSlice<'_>,
        mut dst: MutPtrByteSlice<'_>,
    ) -> Result<(), Error> {
        dst.copy_from_ptr_slice(src);
        self.decrypt(ino, attribute_id, device_offset, file_offset, dst)
    }

    /// Encrypts the filename contained in `buffer`.
    fn encrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error>;

    /// Decrypts the filename contained in `buffer`.
    fn decrypt_filename(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error>;

    /// Encrypts the symlink target contained in `buffer`.
    fn encrypt_symlink(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.encrypt_filename(object_id, buffer)
    }

    /// Decrypts the symlink target contained in `buffer`.
    fn decrypt_symlink(&self, object_id: u64, buffer: &mut Vec<u8>) -> Result<(), Error> {
        self.decrypt_filename(object_id, buffer)
    }

    /// Returns a hash_code to use.
    /// Note in the case of encrypted filenames, takes the raw encrypted bytes.
    fn hash_code(&self, _raw_filename: &[u8], filename: &str) -> Option<u32>;

    /// Returns a case-folded hash_code to use for 'filename'.
    fn hash_code_casefold(&self, _filename: &str) -> u32;

    /// True if supports inline encryption
    fn supports_inline_encryption(&self) -> bool;

    /// If this cipher type supports inline encryption, returns the (dun, slot) value.
    /// Else returns None.
    fn crypt_ctx(&self, ino: u64, attribute_id: u64, file_offset: u64) -> Option<(u64, u8)>;
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyType {
    Fxfs,
    FscryptInoLblk32Dir,
    FscryptInoLblk32File,
    FscryptInoLblk64Dir,
    FscryptInoLblk64File,
}

pub trait ToKeyType {
    fn to_key_type(&self) -> Option<KeyType>;
}

impl ToKeyType for WrappedKey {
    fn to_key_type(&self) -> Option<KeyType> {
        match self {
            WrappedKey::Fxfs(_) => Some(KeyType::Fxfs),
            WrappedKey::FscryptInoLblk32Dir { .. } => Some(KeyType::FscryptInoLblk32Dir),
            WrappedKey::FscryptInoLblk32File { .. } => Some(KeyType::FscryptInoLblk32File),
            WrappedKey::FscryptInoLblk64Dir { .. } => Some(KeyType::FscryptInoLblk64Dir),
            WrappedKey::FscryptInoLblk64File { .. } => Some(KeyType::FscryptInoLblk64File),
            _ => None,
        }
    }
}

impl ToKeyType for EncryptionKey {
    fn to_key_type(&self) -> Option<KeyType> {
        match self {
            EncryptionKey::LegacyFxfs(_) => unreachable!(),
            EncryptionKey::Fxfs(_) => Some(KeyType::Fxfs),
            EncryptionKey::FscryptInoLblk32Dir { .. } => Some(KeyType::FscryptInoLblk32Dir),
            EncryptionKey::FscryptInoLblk32File { .. } => Some(KeyType::FscryptInoLblk32File),
            EncryptionKey::FscryptInoLblk64Dir { .. } => Some(KeyType::FscryptInoLblk64Dir),
            EncryptionKey::FscryptInoLblk64File { .. } => Some(KeyType::FscryptInoLblk64File),
        }
    }
}

impl ToKeyType for KeyType {
    fn to_key_type(&self) -> Option<KeyType> {
        Some(*self)
    }
}

/// Helper function to obtain a Cipher for a key.
/// Uses key to interpret the meaning of the UnwrappedKey blob and then creates a
/// cipher instance from the blob, returning it.
#[inline]
pub fn key_to_cipher(
    key_type: &impl ToKeyType,
    unwrapped_key: &UnwrappedKey,
) -> Result<Arc<dyn Cipher>, zx::Status> {
    key_type
        .to_key_type()
        .map(|key_type| match key_type {
            KeyType::Fxfs => Arc::new(fxfs::FxfsCipher::new(unwrapped_key)) as Arc<dyn Cipher>,
            KeyType::FscryptInoLblk32Dir => {
                Arc::new(fscrypt_ino_lblk32::FscryptInoLblk32DirCipher::new(unwrapped_key))
            }
            KeyType::FscryptInoLblk32File => {
                Arc::new(fscrypt_ino_lblk32::FscryptInoLblk32FileCipher::new(unwrapped_key))
            }
            KeyType::FscryptInoLblk64Dir => {
                Arc::new(fscrypt_ino_lblk64::FscryptInoLblk64DirCipher::new(unwrapped_key))
            }
            KeyType::FscryptInoLblk64File => {
                Arc::new(fscrypt_ino_lblk64::FscryptInoLblk64FileCipher::new(unwrapped_key))
            }
        })
        .ok_or(zx::Status::NOT_SUPPORTED)
}

#[derive(Clone, Debug)]
pub enum CipherHolder {
    Cipher(Arc<dyn Cipher>),
    Unavailable,
}

impl CipherHolder {
    pub fn into_cipher(self) -> Option<Arc<dyn Cipher>> {
        match self {
            CipherHolder::Cipher(c) => Some(c),
            _ => None,
        }
    }
}

/// A container that holds ciphers related to a specific object.
#[derive(Clone, Debug, Default)]
pub struct CipherSet(BTreeMap<u64, CipherHolder>);
impl CipherSet {
    pub fn find_key(self: &Arc<Self>, id: u64) -> FindKeyResult {
        match self.0.get(&id) {
            Some(CipherHolder::Cipher(cipher)) => FindKeyResult::Key(Arc::clone(cipher)),
            Some(CipherHolder::Unavailable) => FindKeyResult::Unavailable,
            None => FindKeyResult::NotFound,
        }
    }

    pub fn add_key(&mut self, id: u64, cipher: CipherHolder) {
        self.0.insert(id, cipher);
    }
}
impl From<Vec<(u64, CipherHolder)>> for CipherSet {
    fn from(keys: Vec<(u64, CipherHolder)>) -> Self {
        Self(keys.into_iter().collect())
    }
}
impl From<BTreeMap<u64, CipherHolder>> for CipherSet {
    fn from(keys: BTreeMap<u64, CipherHolder>) -> Self {
        Self(keys)
    }
}

pub enum FindKeyResult {
    /// No key registered with that key_id.
    NotFound,
    /// The key is known, but not available for use (cannot be unwrapped).
    Unavailable,
    Key(Arc<dyn Cipher>),
}
