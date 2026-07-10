// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.
use super::{Cipher, SECTOR_SIZE, Tweak, UnwrappedKey, XtsProcessor};
use aes::Aes256;
use aes::cipher::{BlockCipherDecrypt, BlockCipherEncrypt, KeyInit};
use anyhow::Error;
use log::warn;
use zerocopy::IntoBytes;

#[derive(Debug)]
pub struct FxfsCipher {
    key: Aes256,
    legacy: bool,
}
impl FxfsCipher {
    pub fn new(key: &UnwrappedKey) -> Self {
        Self { key: Aes256::new(key.as_slice().try_into().unwrap()), legacy: false }
    }

    pub fn new_legacy(key: &UnwrappedKey) -> Self {
        Self { key: Aes256::new(key.as_slice().try_into().unwrap()), legacy: true }
    }
}
impl Cipher for FxfsCipher {
    fn encrypt(
        &self,
        _ino: u64,
        attribute_id: u64,
        _device_offset: u64,
        file_offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        fxfs_trace::duration!("encrypt", "len" => buffer.len());
        assert_eq!(file_offset % SECTOR_SIZE, 0);
        let mut sector_offset = file_offset / SECTOR_SIZE;
        assert_eq!(buffer.len() % (SECTOR_SIZE as usize), 0);
        let upper_tweak = if self.legacy { 0 } else { (attribute_id as u128) << 64 };
        for sector in buffer.chunks_exact_mut(SECTOR_SIZE as usize) {
            let mut tweak = Tweak(upper_tweak | (sector_offset as u128));
            // The same key is used for encrypting the data and computing the tweak.
            self.key.encrypt_block(tweak.as_mut_bytes().try_into().unwrap());
            self.key.encrypt_with_backend(XtsProcessor::new(tweak, sector));
            sector_offset += 1;
        }
        Ok(())
    }

    fn decrypt(
        &self,
        _ino: u64,
        attribute_id: u64,
        _device_offset: u64,
        file_offset: u64,
        buffer: &mut [u8],
    ) -> Result<(), Error> {
        fxfs_trace::duration!("decrypt", "len" => buffer.len());
        assert_eq!(file_offset % SECTOR_SIZE, 0);
        let mut sector_offset = file_offset / SECTOR_SIZE;
        assert_eq!(buffer.len() % (SECTOR_SIZE as usize), 0);
        let upper_tweak = if self.legacy { 0 } else { (attribute_id as u128) << 64 };
        for sector in buffer.chunks_exact_mut(SECTOR_SIZE as usize) {
            let mut tweak = Tweak(upper_tweak | (sector_offset as u128));
            // The same key is used for encrypting the data and computing the tweak.
            self.key.encrypt_block(tweak.as_mut_bytes().try_into().unwrap());
            self.key.decrypt_with_backend(XtsProcessor::new(tweak, sector));
            sector_offset += 1;
        }
        Ok(())
    }

    fn encrypt_filename(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        debug_assert!(false, "encrypt_filename called on fxfs cipher");
        Err(zx_status::Status::NOT_SUPPORTED.into())
    }

    fn decrypt_filename(&self, _object_id: u64, _buffer: &mut Vec<u8>) -> Result<(), Error> {
        // NOTE: This isn't a debug assertion because it would trip on the golden image tests.
        warn!("decrypt_filename called on fxfs cipher");
        Err(zx_status::Status::NOT_SUPPORTED.into())
    }

    fn hash_code(&self, _raw_filename: &[u8], _filename: &str) -> Option<u32> {
        debug_assert!(false, "hash_code called on fxfs cipher");
        None
    }

    fn hash_code_casefold(&self, _filename: &str) -> u32 {
        debug_assert!(false, "hash_code_casefold called on fxfs cipher");
        0
    }

    fn supports_inline_encryption(&self) -> bool {
        false
    }

    fn crypt_ctx(&self, _ino: u64, _attribute_id: u64, _file_offset: u64) -> Option<(u32, u8)> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{Cipher, FxfsCipher, SECTOR_SIZE};
    use crate::UnwrappedKey;

    #[test]
    fn test_legacy_fxfs_cipher_ignores_attribute_id() {
        let key = UnwrappedKey::new(vec![0x42; 32]);
        let cipher = FxfsCipher::new_legacy(&key);
        let mut buf0 = vec![0x12; SECTOR_SIZE as usize];
        let mut buf1 = vec![0x12; SECTOR_SIZE as usize];

        cipher.encrypt(1, 0, 0, 0, &mut buf0).expect("encrypt attr 0");
        cipher.encrypt(1, 4, 0, 0, &mut buf1).expect("encrypt attr 4");
        assert_eq!(
            buf0, buf1,
            "LegacyFxfsCipher should produce identical ciphertext for same file_offset regardless of attribute_id"
        );
    }

    #[test]
    fn test_fxfs_cipher_domain_separates_attribute_id() {
        let key = UnwrappedKey::new(vec![0x42; 32]);
        let cipher = FxfsCipher::new(&key);
        let mut buf0 = vec![0x12; SECTOR_SIZE as usize];
        let mut buf1 = vec![0x12; SECTOR_SIZE as usize];

        cipher.encrypt(1, 0, 0, 0, &mut buf0).expect("encrypt attr 0");
        cipher.encrypt(1, 4, 0, 0, &mut buf1).expect("encrypt attr 4");
        assert_ne!(buf0, buf1, "FxfsCipher should domain-separate tweaks across attribute_id");

        // Verify decryption works correctly for each attribute_id
        cipher.decrypt(1, 0, 0, 0, &mut buf0).expect("decrypt attr 0");
        assert_eq!(buf0, vec![0x12; SECTOR_SIZE as usize]);

        cipher.decrypt(1, 4, 0, 0, &mut buf1).expect("decrypt attr 4");
        assert_eq!(buf1, vec![0x12; SECTOR_SIZE as usize]);
    }
}
