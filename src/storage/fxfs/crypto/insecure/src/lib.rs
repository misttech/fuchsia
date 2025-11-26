// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use async_trait::async_trait;
use fxfs_crypt_common::CryptBase;
use fxfs_crypto::{
    Crypt, EncryptionKey, FxfsKey, KeyPurpose, ObjectType, UnwrappedKey, WrappedKey, WrappingKey,
    WrappingKeyId,
};
use log::error;
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
pub struct InsecureCrypt {
    inner: CryptBase,
}

impl InsecureCrypt {
    pub fn new() -> Self {
        let inner = CryptBase::new();
        inner.add_wrapping_key(DATA_WRAPPING_KEY_ID, DATA_KEY).unwrap();
        inner.add_wrapping_key(METADATA_WRAPPING_KEY_ID, METADATA_KEY).unwrap();
        inner.set_active_key(KeyPurpose::Data, DATA_WRAPPING_KEY_ID).unwrap();
        inner.set_active_key(KeyPurpose::Metadata, METADATA_WRAPPING_KEY_ID).unwrap();
        Self { inner }
    }

    pub fn use_fxfs_keys_for_fscrypt_dirs(&mut self) {
        self.inner.use_fxfs_keys_for_fscrypt_dirs();
    }

    /// Simulates a crypt instance prematurely terminating.  All requests will fail.
    pub fn shutdown(&self) {
        self.inner.shutdown();
    }

    pub fn add_wrapping_key(&self, id: WrappingKeyId, key: WrappingKey) {
        match key {
            WrappingKey::Aes256GcmSiv(key) => {
                self.inner.add_wrapping_key(id, key).unwrap();
            }
            _ => unimplemented!(),
        }
    }

    pub fn remove_wrapping_key(&self, id: &WrappingKeyId) {
        self.inner.forget_wrapping_key(id).unwrap();
    }

    /// Fscrypt in INO_LBLK32 and INO_LBLK64 modes mix the filesystem_uuid into key derivation
    /// functions. Crypt should be told the uuid ahead of time to support decryption of migrated
    /// data. (Note that we make an assumption that there is only one filesystem.)
    pub fn set_filesystem_uuid(&mut self, uuid: &[u8; 16]) {
        self.inner.set_filesystem_uuid(uuid);
    }
}

#[async_trait]
impl Crypt for InsecureCrypt {
    async fn create_key(
        &self,
        owner: u64,
        purpose: KeyPurpose,
    ) -> Result<(FxfsKey, UnwrappedKey), zx::Status> {
        self.inner.create_key(owner, purpose).await.map_err(|e| {
            error!("Failed to create key: {:?}", e);
            e
        })
    }

    async fn create_key_with_id(
        &self,
        owner: u64,
        wrapping_key_id: WrappingKeyId,
        object_type: ObjectType,
    ) -> Result<(EncryptionKey, UnwrappedKey), zx::Status> {
        self.inner.create_key_with_id(owner, wrapping_key_id, object_type).await.map_err(|e| {
            error!("Failed to create key with id: {:?}", e);
            e
        })
    }

    async fn unwrap_key(
        &self,
        wrapped_key: &WrappedKey,
        owner: u64,
    ) -> Result<UnwrappedKey, zx::Status> {
        self.inner.unwrap_key(wrapped_key, owner).await.map_err(|e| {
            error!("Failed to unwrap key: {:?}", e);
            e
        })
    }
}
