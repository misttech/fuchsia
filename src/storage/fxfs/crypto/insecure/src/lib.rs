// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use aes_gcm_siv::aead::Aead;
use aes_gcm_siv::{Aes256GcmSiv, Key, KeyInit as _, Nonce};
use async_trait::async_trait;
use fuchsia_sync::Mutex;
use fxfs_crypto::{
    Crypt, EncryptionKey, FscryptKeyIdentifierAndNonce, FxfsKey, KeyPurpose, ObjectType,
    UnwrappedKey, WrappedKey, WrappedKeyBytes, WrappingKey, WrappingKeyId,
};
use log::error;
use rand::rngs::StdRng;
use rand::{RngCore, SeedableRng};
use rustc_hash::FxHashMap as HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use zx_status as zx;

pub const DATA_KEY: [u8; 32] = [
    0x0, 0x1, 0x2, 0x3, 0x4, 0x5, 0x6, 0x7, 0x8, 0x9, 0xa, 0xb, 0xc, 0xd, 0xe, 0xf, 0x10, 0x11,
    0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f,
];
pub const METADATA_KEY: [u8; 32] = [
    0xff, 0xfe, 0xfd, 0xfc, 0xfb, 0xfa, 0xf9, 0xf8, 0xf7, 0xf6, 0xf5, 0xf4, 0xf3, 0xf2, 0xf1, 0xf0,
    0xef, 0xee, 0xed, 0xec, 0xeb, 0xea, 0xe9, 0xe8, 0xe7, 0xe6, 0xe5, 0xe4, 0xe3, 0xe2, 0xe1, 0xe0,
];
const DATA_WRAPPING_KEY_ID: WrappingKeyId = u128::to_le_bytes(0);
const METADATA_WRAPPING_KEY_ID: WrappingKeyId = u128::to_le_bytes(1);

/// This struct provides the `Crypt` trait without any strong security.
///
/// It is intended for use only in test code where actual security is inconsequential.
#[derive(Default)]
pub struct InsecureCrypt {
    /// FxfsKey is wrapped using AES256GCM-SIV.
    /// Unwrapping turns a 48-byte signed key into a 32-byte raw key.
    /// This maps from an opaque wrapping_key_id to a specific cipher instance that can unwrap keys.
    ciphers: Mutex<HashMap<WrappingKeyId, Cipher>>,

    /// Legacy fscrypt uses the filesystem UUID to salt encryption keys in some variants.
    /// We don't have direct access to the filesystem so we store the UUID here.
    filesystem_uuid: [u8; 16],

    active_data_key: Option<WrappingKeyId>,
    active_metadata_key: Option<WrappingKeyId>,
    shutdown: AtomicBool,
    use_fxfs_keys_for_fscrypt_dirs: bool,
}

struct Cipher {
    key: [u8; 32],
    aes_gcm_siv: Aes256GcmSiv,
}

impl Cipher {
    fn new(key: [u8; 32]) -> Self {
        let aes_gcm_siv = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));
        Self { key, aes_gcm_siv }
    }
}

impl InsecureCrypt {
    pub fn new() -> Self {
        Self {
            ciphers: Mutex::new(HashMap::from_iter([
                (DATA_WRAPPING_KEY_ID, Cipher::new(DATA_KEY.clone())),
                (METADATA_WRAPPING_KEY_ID, Cipher::new(METADATA_KEY.clone())),
            ])),
            filesystem_uuid: [0; 16],
            active_data_key: Some(DATA_WRAPPING_KEY_ID),
            active_metadata_key: Some(METADATA_WRAPPING_KEY_ID),
            ..Default::default()
        }
    }

    pub fn use_fxfs_keys_for_fscrypt_dirs(&mut self) {
        self.use_fxfs_keys_for_fscrypt_dirs = true;
    }

    /// Simulates a crypt instance prematurely terminating.  All requests will fail.
    pub fn shutdown(&self) {
        self.shutdown.store(true, Ordering::Relaxed);
    }

    pub fn add_wrapping_key(&self, id: WrappingKeyId, key: WrappingKey) {
        match key {
            WrappingKey::Aes256GcmSiv(key) => {
                assert!(self.ciphers.lock().insert(id, Cipher::new(key)).is_none());
            }
            _ => unimplemented!(),
        }
    }

    pub fn remove_wrapping_key(&self, id: &WrappingKeyId) {
        let _key = self.ciphers.lock().remove(id);
    }

    /// Fscrypt in INO_LBLK32 and INO_LBLK64 modes mix the filesystem_uuid into key derivation
    /// functions. Crypt should be told the uuid ahead of time to support decryption of migrated
    /// data. (Note that we make an assumption that there is only one filesystem.)
    pub fn set_filesystem_uuid(&mut self, uuid: &[u8; 16]) {
        self.filesystem_uuid = *uuid;
    }
}

#[async_trait]
impl Crypt for InsecureCrypt {
    async fn create_key(
        &self,
        owner: u64,
        purpose: KeyPurpose,
    ) -> Result<(FxfsKey, UnwrappedKey), zx::Status> {
        if self.shutdown.load(Ordering::Relaxed) {
            error!("Crypt was shut down");
            return Err(zx::Status::INTERNAL);
        }
        let wrapping_key_id = match purpose {
            KeyPurpose::Data => self.active_data_key.as_ref(),
            KeyPurpose::Metadata => self.active_metadata_key.as_ref(),
        }
        .ok_or(zx::Status::INVALID_ARGS)?;
        let ciphers = self.ciphers.lock();
        let cipher = ciphers.get(wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;
        let mut nonce = Nonce::default();
        nonce.as_mut_slice()[..8].copy_from_slice(&owner.to_le_bytes());

        let mut key = [0u8; 32];
        StdRng::from_os_rng().fill_bytes(&mut key);

        let wrapped: Vec<u8> = cipher.aes_gcm_siv.encrypt(&nonce, &key[..]).map_err(|e| {
            error!("Failed to wrap key: {:?}", e);
            zx::Status::INTERNAL
        })?;
        let wrapped = WrappedKeyBytes::try_from(wrapped).map_err(|_| zx::Status::INTERNAL)?;
        Ok((
            FxfsKey { wrapping_key_id: *wrapping_key_id, key: wrapped },
            UnwrappedKey::new(key.to_vec()),
        ))
    }

    async fn create_key_with_id(
        &self,
        owner: u64,
        wrapping_key_id: WrappingKeyId,
        object_type: ObjectType,
    ) -> Result<(EncryptionKey, UnwrappedKey), zx::Status> {
        if self.shutdown.load(Ordering::Relaxed) {
            error!("Crypt was shut down");
            return Err(zx::Status::INTERNAL);
        }

        let ciphers = self.ciphers.lock();
        let cipher = ciphers.get(&wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;

        match object_type {
            ObjectType::Directory if !self.use_fxfs_keys_for_fscrypt_dirs => {
                let mut nonce = [0; 16];
                StdRng::from_os_rng().fill_bytes(&mut nonce);
                let unwrapped_key = unwrap_fscrypt_dir_key(cipher, &nonce);
                Ok((
                    EncryptionKey::FscryptInoLblk32Dir { key_identifier: wrapping_key_id, nonce },
                    unwrapped_key,
                ))
            }
            _ => {
                let mut nonce = Nonce::default();
                nonce.as_mut_slice()[..8].copy_from_slice(&owner.to_le_bytes());

                let mut key = [0u8; 32];
                StdRng::from_os_rng().fill_bytes(&mut key);

                let wrapped: Vec<u8> =
                    cipher.aes_gcm_siv.encrypt(&nonce, &key[..]).map_err(|e| {
                        error!("Failed to wrap key: {:?}", e);
                        zx::Status::INTERNAL
                    })?;
                let wrapped =
                    WrappedKeyBytes::try_from(wrapped).map_err(|_| zx::Status::BAD_STATE)?;
                Ok((
                    EncryptionKey::Fxfs(FxfsKey { wrapping_key_id, key: wrapped }),
                    UnwrappedKey::new(key.to_vec()),
                ))
            }
        }
    }

    async fn unwrap_key(
        &self,
        wrapped_key: &WrappedKey,
        owner: u64,
    ) -> Result<UnwrappedKey, zx::Status> {
        if self.shutdown.load(Ordering::Relaxed) {
            error!("Crypt was shut down");
            return Err(zx::Status::INTERNAL);
        }
        let ciphers = self.ciphers.lock();
        Ok(match wrapped_key {
            WrappedKey::Fxfs(fxfs_key) => {
                let cipher =
                    ciphers.get(&fxfs_key.wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;
                let mut nonce = Nonce::default();
                nonce.as_mut_slice()[..8].copy_from_slice(&owner.to_le_bytes());
                UnwrappedKey::new(
                    cipher
                        .aes_gcm_siv
                        .decrypt(&nonce, &fxfs_key.wrapped_key[..])
                        .map_err(|e| {
                            error!("unwrap keys failed: {:?}", e);
                            zx::Status::INTERNAL
                        })?
                        .try_into()
                        .map_err(|_| {
                            error!("Unexpected wrapped key length");
                            zx::Status::INTERNAL
                        })?,
                )
            }
            WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                key_identifier,
                nonce,
            }) => {
                let cipher = ciphers.get(key_identifier).ok_or(zx::Status::UNAVAILABLE)?;
                unwrap_fscrypt_dir_key(cipher, nonce)
            }
            _ => {
                error!("Unsupported wrapped key {wrapped_key:?}");
                return Err(zx::Status::NOT_SUPPORTED);
            }
        })
    }
}

fn unwrap_fscrypt_dir_key(cipher: &Cipher, nonce: &[u8]) -> UnwrappedKey {
    let mut result = vec![0u8; 96];
    let output: &mut [u8; 96] = (&mut result[..]).try_into().unwrap();
    fscrypt::hkdf::hkdf(&cipher.key, nonce, output);
    UnwrappedKey::new(result)
}
