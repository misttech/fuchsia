// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use aes_gcm_siv::aead::Aead;
use aes_gcm_siv::{Aes256GcmSiv, Key, KeyInit as _, Nonce};
use anyhow::{Error, anyhow};
use fidl_fuchsia_fxfs::{
    CryptCreateKeyResult, CryptCreateKeyWithIdResult, CryptRequest, CryptRequestStream,
    CryptUnwrapKeyResult, FscryptKeyIdentifier, FscryptKeyIdentifierAndNonce, FxfsKey, KeyPurpose,
    ObjectType, WrappedKey,
};
use fidl_fuchsia_hardware_inlineencryption::DeviceSynchronousProxy;
use fscrypt::hkdf::{
    HKDF_CONTEXT_DIRHASH_KEY, HKDF_CONTEXT_INODE_HASH_KEY, HKDF_CONTEXT_IV_INO_LBLK_32_KEY,
    HKDF_CONTEXT_KEY_IDENTIFIER,
};
use fscrypt::{ENCRYPTION_MODE_AES_256_CTS, TEST_F2FS_IMAGE_FILESYSTEM_UUID};
use fuchsia_sync::Mutex;
use futures::stream::StreamExt;
use hkdf::Hkdf;
use linux_uapi::FSCRYPT_KEY_IDENTIFIER_SIZE;
use log::error;
use starnix_uapi::error as starnix_error;
use starnix_uapi::errors::Errno;
use std::collections::hash_map::{Entry, HashMap};

// In this implementation of fscrypt, we use a HKDF (Hmac Key Derivation Function) to derive a
// a wrapping key and wrapping key id from the raw key bytes passed in by a user on
// FS_IOC_ADD_ENCRYPTION_KEY. HKDFs requires an input "info" string. We define constants for the
// respective "info" strings here.
const FXFS_FSCRYPT_KEY_IDENTIFIER_INFO: &'static str = "fscrypt0";
const FXFS_FSCRYPT_WRAPPING_KEY_INFO: &'static str = "fscrypt1";
const FSCRYPT_HKDF_NONCE_PREFIX: &[u8] = b"fscrypt\0";

const DATA_UNIT_SIZE: u32 = 4096;
pub const AES256_KEY_SIZE: usize = 32;

/// Contains an fscrypt wrapping key id.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Copy)]
pub struct EncryptionKeyId([u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize]);

impl From<[u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize]> for EncryptionKeyId {
    fn from(buf: [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize]) -> Self {
        Self(buf)
    }
}

impl EncryptionKeyId {
    pub fn as_raw(&self) -> [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize] {
        self.0.clone()
    }
}

#[derive(Clone, Debug)]
pub struct Lblk32DerivedKeys {
    pub ino_hash_key: [u8; 16],
    pub cts_key: Vec<u8>,
}

#[derive(Clone)]
pub struct Lblk32KeyInfo {
    pub hardware_wrapped: bool,
    pub slot: Option<u8>,
    pub derived_keys: Lblk32DerivedKeys,
    pub main_key: Vec<u8>,
}

#[allow(clippy::large_enum_variant)]
#[derive(Clone)]
pub enum UserKey {
    FxfsKey { cipher: Aes256GcmSiv },
    InlineCryptoLblk32Key { key_info: Lblk32KeyInfo },
}

#[derive(Clone)]
pub struct KeyInfo {
    users: Vec<u32>,
    key: UserKey,
}

#[derive(Default)]
pub struct CryptServiceInner {
    ciphers: HashMap<EncryptionKeyId, KeyInfo>,
    metadata_key: Option<EncryptionKeyId>,
    data_key: Option<EncryptionKeyId>,
}

impl CryptServiceInner {
    pub fn ciphers(&self) -> HashMap<EncryptionKeyId, KeyInfo> {
        self.ciphers.clone()
    }
}

#[derive(Default)]
pub struct CryptService {
    inner: Mutex<CryptServiceInner>,
}

fn zero_extended_nonce(val: u64) -> Nonce {
    let mut nonce = Nonce::default();
    nonce.as_mut_slice()[..8].copy_from_slice(&val.to_le_bytes());
    nonce
}

impl CryptService {
    pub fn new() -> Self {
        Self { inner: Mutex::new(CryptServiceInner::default()) }
    }

    pub fn contains_key(&self, key: EncryptionKeyId) -> bool {
        let inner = self.inner.lock();
        inner.ciphers.contains_key(&key)
    }

    pub fn get_users_for_key(&self, key: EncryptionKeyId) -> Option<Vec<u32>> {
        let inner = self.inner.lock();
        inner.ciphers.get(&key).map(|x| x.users.clone())
    }

    fn get_or_program_lblk32_key(
        &self,
        key_info: &mut Lblk32KeyInfo,
        inline_encryption_controller: &DeviceSynchronousProxy,
    ) -> Result<u8, i32> {
        if let Some(slot) = key_info.slot {
            Ok(slot)
        } else {
            let program_key_res = inline_encryption_controller
                .program_key(&key_info.main_key, DATA_UNIT_SIZE, zx::MonotonicInstant::INFINITE)
                .expect("FIDL transport error on program key");
            match program_key_res {
                Ok(slot) => {
                    key_info.slot = Some(slot);
                    Ok(slot)
                }
                Err(e) => {
                    error!("Program key failed with {:?}", e);
                    Err(zx::Status::INTERNAL.into_raw())
                }
            }
        }
    }

    pub fn derive_wrapping_key_id_and_lblk32_derived_keys(
        &self,
        key: &[u8],
        hardware_wrapped: bool,
        inline_encryption_proxy: &DeviceSynchronousProxy,
    ) -> ([u8; 16], Lblk32DerivedKeys) {
        // For hardware-wrapped keys, the key identifier and ino_hash are derived from the software
        // secret.
        let main_key = if hardware_wrapped {
            inline_encryption_proxy
                .derive_raw_secret(key, zx::MonotonicInstant::INFINITE)
                .expect("Fidl transport error on derive_raw_secret")
                .expect("derive_raw_secret failed")
        } else {
            key.to_vec()
        };
        let hk = Hkdf::<sha2::Sha512>::new(None, &main_key);
        let mut ino_hash_key: [u8; 16] = [0u8; 16 as usize];
        let mut hkdf_info = FSCRYPT_HKDF_NONCE_PREFIX.to_vec();
        hkdf_info.push(HKDF_CONTEXT_INODE_HASH_KEY);

        hk.expand(&hkdf_info, &mut ino_hash_key)
            .expect("FSCRYPT_KEY_IDENTIFIER_SIZE is a valid length for Sha256 to output");

        let mut cts_key: [u8; 32] = [0u8; 32 as usize];
        let mut hkdf_info = FSCRYPT_HKDF_NONCE_PREFIX.to_vec();
        hkdf_info.push(HKDF_CONTEXT_IV_INO_LBLK_32_KEY);
        hkdf_info.extend_from_slice(&[ENCRYPTION_MODE_AES_256_CTS]);
        hkdf_info.extend_from_slice(&TEST_F2FS_IMAGE_FILESYSTEM_UUID);

        hk.expand(&hkdf_info, &mut cts_key)
            .expect("FSCRYPT_KEY_IDENTIFIER_SIZE is a valid length for Sha256 to output");

        let mut key_identifier: [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize] =
            [0u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize];
        let mut hkdf_info = FSCRYPT_HKDF_NONCE_PREFIX.to_vec();
        hkdf_info.push(HKDF_CONTEXT_KEY_IDENTIFIER);
        hk.expand(&hkdf_info, &mut key_identifier)
            .expect("FSCRYPT_KEY_IDENTIFIER_SIZE is a valid length for Sha256 to output");
        (key_identifier, Lblk32DerivedKeys { ino_hash_key, cts_key: cts_key.to_vec() })
    }

    pub fn derive_fxfs_wrapping_key_id_and_cipher(
        &self,
        raw_key: &[u8],
    ) -> ([u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize], Aes256GcmSiv) {
        let hk = Hkdf::<sha2::Sha256>::new(None, raw_key);
        let mut key_identifier: [u8; 16] = [0u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize];
        hk.expand(FXFS_FSCRYPT_KEY_IDENTIFIER_INFO.as_bytes(), &mut key_identifier)
            .expect("FSCRYPT_KEY_IDENTIFIER_SIZE is a valid length for Sha256 to output");
        let mut wrapping_key = [0u8; AES256_KEY_SIZE];
        hk.expand(FXFS_FSCRYPT_WRAPPING_KEY_INFO.as_bytes(), &mut wrapping_key)
            .expect("AES256_KEY_SIZE is a valid length for Sha256 to output");
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&wrapping_key));
        (key_identifier, cipher)
    }

    fn create_key(&self, owner: u64, purpose: KeyPurpose) -> CryptCreateKeyResult {
        let inner = self.inner.lock();
        let wrapping_key_id = match purpose {
            KeyPurpose::Data => inner.data_key.as_ref().ok_or_else(|| {
                log::error!(
                    "tried to create key with KeyPurpose::Data but no active data wrapping key"
                );
                zx::Status::BAD_STATE.into_raw()
            })?,
            KeyPurpose::Metadata => inner.metadata_key.as_ref().ok_or_else(|| {
                log::error!(
                    "tried to create key with KeyPurpose::Metadata but no active data wrapping key"
                );
                zx::Status::BAD_STATE.into_raw()
            })?,
            _ => return Err(zx::Status::INVALID_ARGS.into_raw()),
        };
        let cipher = match inner
            .ciphers
            .get(wrapping_key_id)
            .ok_or_else(|| zx::Status::BAD_STATE.into_raw())?
            .key
            .clone()
        {
            UserKey::FxfsKey { cipher } => cipher,
            _ => {
                error!("create_key called for Lblk32 key");
                return Err(zx::Status::INTERNAL.into_raw());
            }
        };
        let nonce = zero_extended_nonce(owner);

        let mut key = [0u8; 32];
        rand::fill(&mut key[..]);

        let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|e| {
            error!("Failed to wrap key error: {:?}", e);
            zx::Status::INTERNAL.into_raw()
        })?;

        Ok((wrapping_key_id.as_raw(), wrapped.into(), key.into()))
    }

    fn create_key_with_id(
        &self,
        owner: u64,
        wrapping_key_id: EncryptionKeyId,
        object_type: ObjectType,
        inline_encryption_controller: Option<&DeviceSynchronousProxy>,
    ) -> CryptCreateKeyWithIdResult {
        let mut inner = self.inner.lock();
        match inner
            .ciphers
            .get_mut(&wrapping_key_id)
            .ok_or_else(|| zx::Status::NOT_FOUND.into_raw())?
            .key
            .clone()
        {
            UserKey::FxfsKey { cipher } => {
                let nonce = zero_extended_nonce(owner);

                let mut key = [0u8; 32];
                rand::fill(&mut key[..]);

                let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|error| {
                    error!("Failed to wrap key error: {:?}", error);
                    zx::Status::INTERNAL.into_raw()
                })?;

                Ok((
                    WrappedKey::Fxfs(FxfsKey {
                        wrapping_key_id: wrapping_key_id.as_raw(),
                        wrapped_key: wrapped.try_into().expect("wrapped key wrong size"),
                    }),
                    key.into(),
                ))
            }
            UserKey::InlineCryptoLblk32Key { mut key_info } => {
                let inline_encryption_controller =
                    inline_encryption_controller.ok_or_else(|| {
                        error!(
                            "create_key_with_id called on a lblk32 key but no inline encryption
                                controller provided"
                        );
                        zx::Status::BAD_STATE.into_raw()
                    })?;
                match object_type {
                    ObjectType::Directory | ObjectType::Symlink => {
                        let main_key = if key_info.hardware_wrapped {
                            inline_encryption_controller
                                .derive_raw_secret(
                                    &key_info.main_key,
                                    zx::MonotonicInstant::INFINITE,
                                )
                                .expect("Fidl transport error on derive_raw_secret")
                                .expect("derive_raw_secret failed")
                        } else {
                            key_info.main_key.clone()
                        };
                        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&main_key));
                        let nonce = zero_extended_nonce(owner);
                        let mut key = [0u8; 32];
                        rand::fill(&mut key[..]);

                        let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|error| {
                            error!("Failed to wrap key error: {:?}", error);
                            zx::Status::INTERNAL.into_raw()
                        })?;

                        Ok((
                            WrappedKey::Fxfs(FxfsKey {
                                wrapping_key_id: wrapping_key_id.as_raw(),
                                wrapped_key: wrapped.try_into().expect("wrapped key wrong size"),
                            }),
                            key.into(),
                        ))
                    }
                    ObjectType::File => {
                        let slot = self.get_or_program_lblk32_key(
                            &mut key_info,
                            &inline_encryption_controller,
                        )?;
                        let mut unwrapped_key = vec![slot];
                        unwrapped_key.extend(key_info.derived_keys.ino_hash_key.clone());
                        Ok((
                            WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier {
                                key_identifier: wrapping_key_id.as_raw(),
                            }),
                            unwrapped_key,
                        ))
                    }
                    _ => Err(zx::Status::BAD_STATE.into_raw()),
                }
            }
        }
    }

    fn unwrap_key(
        &self,
        owner: u64,
        key: WrappedKey,
        inline_encryption_controller: Option<&DeviceSynchronousProxy>,
    ) -> CryptUnwrapKeyResult {
        let mut inner = self.inner.lock();
        match key {
            WrappedKey::Fxfs(FxfsKey { wrapping_key_id, wrapped_key }) => {
                let wrapping_key_id = EncryptionKeyId::from(wrapping_key_id);
                let cipher = match inner
                    .ciphers
                    .get(&wrapping_key_id)
                    .ok_or_else(|| zx::Status::NOT_FOUND.into_raw())?
                    .clone()
                    .key
                {
                    UserKey::FxfsKey { cipher } => cipher,
                    UserKey::InlineCryptoLblk32Key { key_info } => {
                        Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key_info.main_key))
                    }
                };
                let nonce = zero_extended_nonce(owner);

                Ok(cipher
                    .decrypt(&nonce, &wrapped_key[..])
                    .map_err(|_| zx::Status::IO_DATA_INTEGRITY.into_raw())?)
            }
            WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier { key_identifier }) => {
                let inline_encryption_controller =
                    inline_encryption_controller.ok_or_else(|| {
                        error!(
                            "create_key_with_id called on a lblk32 key but no inline encryption
                        controller provided"
                        );
                        zx::Status::BAD_STATE.into_raw()
                    })?;
                let wrapping_key_id = EncryptionKeyId::from(key_identifier);
                let mut key_info = match inner
                    .ciphers
                    .get_mut(&wrapping_key_id)
                    .ok_or_else(|| zx::Status::NOT_FOUND.into_raw())?
                    .key
                    .clone()
                {
                    UserKey::InlineCryptoLblk32Key { key_info } => key_info,
                    _ => {
                        error!("unwrap_key on WrappedKey::FscryptInoLblk32File called on FxfsKey");
                        return Err(zx::Status::INTERNAL.into_raw());
                    }
                };
                let slot =
                    self.get_or_program_lblk32_key(&mut key_info, &inline_encryption_controller)?;
                let mut unwrapped_key = vec![slot];
                unwrapped_key.extend(key_info.derived_keys.ino_hash_key.clone());
                Ok(unwrapped_key)
            }

            // This is only used for f2fs integration tests
            WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                key_identifier,
                nonce,
            }) => {
                let inline_encryption_controller =
                    inline_encryption_controller.ok_or_else(|| {
                        error!(
                            "create_key_with_id called on a lblk32 key but no inline encryption
                        controller provided"
                        );
                        zx::Status::BAD_STATE.into_raw()
                    })?;
                let wrapping_key_id = EncryptionKeyId::from(key_identifier);
                let key_info = match inner
                    .ciphers
                    .get(&wrapping_key_id)
                    .ok_or_else(|| zx::Status::NOT_FOUND.into_raw())?
                    .key
                    .clone()
                {
                    UserKey::InlineCryptoLblk32Key { key_info } => key_info,
                    _ => {
                        error!("unwrap_key on WrappedKey::FscryptInoLblk32File called on FxfsKey");
                        return Err(zx::Status::INTERNAL.into_raw());
                    }
                };
                let main_key = if key_info.hardware_wrapped {
                    inline_encryption_controller
                        .derive_raw_secret(&key_info.main_key, zx::MonotonicInstant::INFINITE)
                        .expect("Fidl transport error on derive_raw_secret")
                        .expect("derive_raw_secret failed")
                } else {
                    key_info.main_key
                };
                let mut dirhash_key: [u8; 16] = [0u8; 16 as usize];
                let mut hkdf_info = FSCRYPT_HKDF_NONCE_PREFIX.to_vec();
                hkdf_info.push(HKDF_CONTEXT_DIRHASH_KEY);
                hkdf_info.extend_from_slice(&nonce);
                let hk = Hkdf::<sha2::Sha256>::new(None, &main_key);
                hk.expand(&hkdf_info, &mut dirhash_key)
                    .expect("FSCRYPT_KEY_IDENTIFIER_SIZE is a valid length for Sha256 to output");

                let mut unwrapped_key = key_info.derived_keys.cts_key;
                unwrapped_key.extend_from_slice(&key_info.derived_keys.ino_hash_key);
                unwrapped_key.extend_from_slice(&dirhash_key);

                Ok(unwrapped_key)
            }
            _ => Err(zx::Status::NOT_SUPPORTED.into_raw()),
        }
    }

    pub fn add_wrapping_key(
        &self,
        wrapping_key_id: [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize],
        key: UserKey,
        uid: u32,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        match inner.ciphers.entry(EncryptionKeyId::from(wrapping_key_id)) {
            Entry::Occupied(mut e) => {
                let users = &mut e.get_mut().users;
                if !users.contains(&uid) {
                    users.push(uid);
                }
                Ok(())
            }
            Entry::Vacant(vacant) => {
                vacant.insert(KeyInfo { users: vec![uid], key });
                Ok(())
            }
        }
    }

    pub fn forget_wrapping_key(
        &self,
        wrapping_key_id: [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize],
        uid: u32,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        match inner.ciphers.entry(EncryptionKeyId::from(wrapping_key_id)) {
            Entry::Occupied(mut e) => {
                let user_ids = &mut e.get_mut().users;
                if !user_ids.contains(&uid) {
                    return starnix_error!(ENOKEY);
                } else {
                    let index = user_ids.iter().position(|x: &u32| *x == uid).unwrap();
                    user_ids.remove(index);
                    if user_ids.is_empty() {
                        e.remove();
                    }
                }
            }
            Entry::Vacant(_) => {
                return starnix_error!(ENOKEY);
            }
        }
        Ok(())
    }

    pub fn set_active_key(
        &self,
        wrapping_key_id: [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize],
        purpose: KeyPurpose,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        let key_id = EncryptionKeyId::from(wrapping_key_id);
        if !inner.ciphers.contains_key(&key_id) {
            return starnix_error!(ENOENT);
        }
        match purpose {
            KeyPurpose::Data => inner.data_key = Some(key_id),
            KeyPurpose::Metadata => inner.metadata_key = Some(key_id),
            _ => return starnix_error!(EINVAL),
        }
        Ok(())
    }

    pub async fn handle_connection(
        &self,
        mut stream: CryptRequestStream,
        inline_crypto_controller: Option<DeviceSynchronousProxy>,
    ) -> Result<(), Error> {
        while let Some(request) = stream.next().await {
            match request {
                Ok(CryptRequest::CreateKey { owner, purpose, responder }) => {
                    responder
                        .send(match &self.create_key(owner, purpose) {
                            Ok((id, wrapped, key)) => Ok((id, wrapped, key)),
                            Err(e) => Err(*e),
                        })
                        .unwrap_or_else(|e| error!("Failed to send CreateKey response {:?}", e));
                }
                Ok(CryptRequest::CreateKeyWithId {
                    owner,
                    wrapping_key_id,
                    object_type,
                    responder,
                    ..
                }) => {
                    responder
                        .send(
                            match self.create_key_with_id(
                                owner,
                                EncryptionKeyId::from(wrapping_key_id),
                                object_type,
                                inline_crypto_controller.as_ref(),
                            ) {
                                Ok((ref wrapped, ref key)) => Ok((wrapped, key)),
                                Err(e) => Err(e),
                            },
                        )
                        .unwrap_or_else(|e| {
                            error!("Failed to send CreateKeyWithId response {:?}", e)
                        });
                }
                Ok(CryptRequest::UnwrapKey { owner, wrapped_key, responder }) => {
                    responder
                        .send(
                            match self.unwrap_key(
                                owner,
                                wrapped_key,
                                inline_crypto_controller.as_ref(),
                            ) {
                                Ok(ref unwrapped) => Ok(unwrapped),
                                Err(e) => Err(e),
                            },
                        )
                        .unwrap_or_else(|e| error!("Failed to send UnwrapKey response {:?}", e));
                }
                Err(e) => {
                    error!("Error in CryptRequestStream: {:?}", e);
                    return Err(anyhow!(e));
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use block_client::RemoteBlockClient;
    use fidl_fuchsia_fxfs::{FxfsKey, KeyPurpose, WrappedKey};
    use fidl_fuchsia_hardware_block::BlockProxy;
    use fidl_fuchsia_hardware_inlineencryption::{
        DeviceMarker, DeviceRequest, DeviceRequestStream,
    };
    use fuchsia_async::LocalExecutor;
    use starnix_uapi::errno;
    use std::sync::Arc;
    use storage_device::block_device::BlockDevice;
    use storage_device::{Device, InlineCryptoOptions, ReadOptions, WriteOptions};
    use vmo_backed_block_server::{
        InitialContents, VmoBackedServer, VmoBackedServerOptions, VmoBackedServerTestingExt,
    };

    const BLOCK_SIZE: u32 = 4096;

    async fn handle_inline_crypto_requests(
        mut stream: DeviceRequestStream,
        server: Arc<VmoBackedServer>,
    ) {
        while let Some(Ok(request)) = stream.next().await {
            match request {
                DeviceRequest::ProgramKey { wrapped_key, data_unit_size: _, responder } => {
                    let mut main_key = [0; 64];
                    main_key[..wrapped_key.len()].copy_from_slice(&wrapped_key);
                    let slot = server.program_key(main_key);
                    responder.send(Ok(slot)).unwrap_or_else(|e| {
                        log::error!("failed to send ProgramKey response. error: {:?}", e);
                    });
                }
                DeviceRequest::DeriveRawSecret { wrapped_key: _, responder } => {
                    log::warn!("DeriveRawSecret not implemented");
                    responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw())).unwrap_or_else(|e| {
                        log::error!("failed to send DeriveRawSecret response. error: {:?}", e);
                    });
                }
            }
        }
    }

    #[test]
    fn create_key_without_setting_metadata_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));
        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");
        assert_eq!(
            service
                .create_key(0, KeyPurpose::Data)
                .expect_err("create_key should fail without a metadata key set"),
            zx::Status::BAD_STATE.into_raw()
        );
        service
            .set_active_key(u128::to_le_bytes(1), KeyPurpose::Metadata)
            .expect("failed to set metadata key");
        service.create_key(0, KeyPurpose::Metadata).expect("create_key failed");
    }

    #[test]
    fn add_and_forget_wrapping_keys() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));

        assert_eq!(
            service
                .forget_wrapping_key(u128::to_le_bytes(1), 0)
                .expect_err("forget wrapping key should fail if the key was never added"),
            errno!(ENOKEY)
        );
        // Add the wrapping key for users 0 and 1
        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));

        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 1)
            .expect("add wrapping key failed");
        // A user should be able to add the same key multiple times.
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));

        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 1)
            .expect("add wrapping key failed");

        {
            let inner = service.inner.lock();
            assert_eq!(
                inner.ciphers.get(&EncryptionKeyId::from(u128::to_le_bytes(1))).unwrap().users,
                [0, 1]
            );
        }

        // User 1 forgets the wrapping key. Since user 0 still has the key added,
        // create_key_with_id should still succeed.
        service.forget_wrapping_key(u128::to_le_bytes(1), 1).expect("forget wrapping key failed");
        service
            .create_key_with_id(
                0,
                EncryptionKeyId::from(u128::to_le_bytes(1)),
                ObjectType::File,
                None,
            )
            .expect("create key with id failed");

        // User 1 cannot forget the same key a second time.
        assert_eq!(
            service.forget_wrapping_key(u128::to_le_bytes(1), 1).expect_err(
                "forget wrapping key should fail if the key was already removed by this user"
            ),
            errno!(ENOKEY)
        );
        // Once both users remove the key, create_key_with_id should fail.
        service.forget_wrapping_key(u128::to_le_bytes(1), 0).expect("forget wrapping key failed");
        assert_eq!(
            service
                .create_key_with_id(
                    0,
                    EncryptionKeyId::from(u128::to_le_bytes(1)),
                    ObjectType::File,
                    None
                )
                .expect_err(
                    "create_key_with_id should fail if the key hasn't been added by the caller"
                ),
            zx::Status::NOT_FOUND.into_raw()
        );
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));
        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");
    }

    #[fuchsia::test]
    async fn test_derive_wrapping_key_id_and_lblk32_derived_keys() {
        const EXPECTED_WRAPPING_KEY_ID: [u8; 16] =
            [40, 205, 90, 253, 77, 129, 133, 220, 222, 25, 208, 200, 136, 101, 239, 101];
        const EXPECTED_CTS_KEY: [u8; 32] = [
            223, 72, 191, 189, 133, 62, 81, 175, 91, 93, 132, 0, 9, 246, 22, 32, 76, 91, 28, 2, 96,
            27, 182, 66, 131, 84, 218, 118, 230, 226, 142, 115,
        ];
        const EXPECTED_INO_HASH_KEY: [u8; 16] =
            [241, 22, 180, 110, 76, 135, 84, 48, 206, 33, 210, 253, 11, 10, 230, 122];

        let block_server = Arc::new(
            VmoBackedServerOptions {
                block_size: BLOCK_SIZE,
                initial_contents: InitialContents::FromCapacity(393216),
                ..Default::default()
            }
            .build()
            .expect("build failed"),
        );

        let block_server_clone = block_server.clone();
        let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        std::thread::spawn(|| {
            LocalExecutor::default().run_singlethreaded(async {
                handle_inline_crypto_requests(server.into_stream(), block_server_clone).await
            })
        });

        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let (wrapping_key_id, lblk32_keys) =
            service.derive_wrapping_key_id_and_lblk32_derived_keys(&data_key, false, &client);

        assert_eq!(wrapping_key_id, EXPECTED_WRAPPING_KEY_ID);
        assert_eq!(lblk32_keys.cts_key, EXPECTED_CTS_KEY);
        assert_eq!(lblk32_keys.ino_hash_key, EXPECTED_INO_HASH_KEY);
    }

    #[fuchsia::test]
    async fn test_derive_fxfs_wrapping_key_id_and_cipher() {
        const EXPECTED_WRAPPED_KEY: [u8; 48] = [
            179, 37, 100, 221, 49, 242, 51, 3, 45, 241, 253, 154, 56, 12, 240, 248, 220, 200, 212,
            75, 251, 44, 74, 145, 236, 136, 227, 158, 105, 14, 120, 221, 44, 229, 3, 158, 144, 64,
            202, 73, 179, 83, 224, 156, 115, 200, 126, 247,
        ];

        const EXPECTED_WRAPPING_KEY_ID: [u8; 16] =
            [180, 235, 24, 243, 150, 41, 127, 230, 2, 34, 238, 154, 60, 255, 169, 233];

        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let (wrapping_key_id, cipher) = service.derive_fxfs_wrapping_key_id_and_cipher(&data_key);

        let nonce = zero_extended_nonce(0);

        let key = vec![0xABu8; 32];

        let wrapped = cipher
            .encrypt(&nonce, &key[..])
            .map_err(|e| {
                error!("Failed to wrap key error: {:?}", e);
                zx::Status::INTERNAL.into_raw()
            })
            .expect("failed to wrap key");

        assert_eq!(wrapping_key_id, EXPECTED_WRAPPING_KEY_ID);
        assert_eq!(wrapped, EXPECTED_WRAPPED_KEY);
    }

    #[fuchsia::test]
    async fn test_create_key_with_id_with_lblk32_key() {
        let block_server = Arc::new(
            VmoBackedServerOptions {
                block_size: BLOCK_SIZE,
                initial_contents: InitialContents::FromCapacity(393216),
                ..Default::default()
            }
            .build()
            .expect("build failed"),
        );

        let block_server_clone = block_server.clone();
        let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        std::thread::spawn(|| {
            LocalExecutor::default().run_singlethreaded(async {
                handle_inline_crypto_requests(server.into_stream(), block_server_clone).await
            })
        });

        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let (wrapping_key_id, derived_keys) =
            service.derive_wrapping_key_id_and_lblk32_derived_keys(&data_key, false, &client);

        service
            .add_wrapping_key(
                wrapping_key_id,
                UserKey::InlineCryptoLblk32Key {
                    key_info: Lblk32KeyInfo {
                        hardware_wrapped: false,
                        slot: None,
                        derived_keys: derived_keys.clone(),
                        main_key: data_key,
                    },
                },
                0,
            )
            .expect("add wrapping key failed");
        service
            .set_active_key(wrapping_key_id, KeyPurpose::Data)
            .expect("set active key failed for data");
        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(0, wrapping_key_id.into(), ObjectType::File, Some(&client))
            .expect("create_key failed");
        assert!(matches!(
            wrapped_key,
            WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier {
                key_identifier: _wrapping_key_id
            })
        ));
        let expected_slot = 0;
        assert_eq!(unwrapped_key[0], expected_slot);
        assert_eq!(unwrapped_key[1..17], derived_keys.ino_hash_key);
        // Validate encrypted reads/writes with the key we just programmed.
        let device = BlockDevice::new(
            RemoteBlockClient::new(block_server.clone().connect::<BlockProxy>())
                .await
                .expect("Unable to create block client"),
            false,
        )
        .await
        .unwrap();

        let data: &[u8] = b"This is aligned sensitive data!!";
        let mut buf = device.allocate_buffer(4096).await;
        buf.as_mut_slice()[..data.len()].copy_from_slice(data);
        device
            .write_with_opts(
                0,
                buf.as_ref(),
                WriteOptions {
                    inline_crypto_options: InlineCryptoOptions { dun: 0, slot: expected_slot },
                    ..Default::default()
                },
            )
            .await
            .expect("failed to write data");

        let mut read_buf = device.allocate_buffer(4096).await;
        device
            .read_with_opts(
                0,
                read_buf.as_mut(),
                ReadOptions {
                    inline_crypto_options: InlineCryptoOptions { dun: 0, slot: expected_slot + 1 },
                },
            )
            .await
            .expect("Read failed");
        assert!(&read_buf.as_slice()[..data.len()] != data);
        device
            .read_with_opts(
                0,
                read_buf.as_mut(),
                ReadOptions {
                    inline_crypto_options: InlineCryptoOptions { dun: 0, slot: expected_slot },
                },
            )
            .await
            .expect("Read failed");
        assert_eq!(&read_buf.as_slice()[..data.len()], data);
    }

    #[fuchsia::test]
    fn unwrap_fxfs_wrapped_key_with_lblk32_key() {
        let block_server = Arc::new(
            VmoBackedServerOptions {
                block_size: BLOCK_SIZE,
                initial_contents: InitialContents::FromCapacity(393216),
                ..Default::default()
            }
            .build()
            .expect("build failed"),
        );
        let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        std::thread::spawn(|| {
            LocalExecutor::default().run_singlethreaded(async {
                handle_inline_crypto_requests(server.into_stream(), block_server).await
            })
        });

        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let (wrapping_key_id, derived_keys) =
            service.derive_wrapping_key_id_and_lblk32_derived_keys(&data_key, false, &client);

        service
            .add_wrapping_key(
                wrapping_key_id,
                UserKey::InlineCryptoLblk32Key {
                    key_info: Lblk32KeyInfo {
                        hardware_wrapped: false,
                        slot: None,
                        derived_keys,
                        main_key: data_key.clone(),
                    },
                },
                0,
            )
            .expect("add wrapping key failed");
        service
            .set_active_key(wrapping_key_id, KeyPurpose::Data)
            .expect("set active key failed for data");
        let (wrapped_key, expected_unwrapped_key) = service
            .create_key_with_id(0, wrapping_key_id.into(), ObjectType::Directory, Some(&client))
            .expect("create_key failed");
        assert!(matches!(
            wrapped_key,
            WrappedKey::Fxfs(FxfsKey { wrapping_key_id: _wrapping_key_id, wrapped_key: _data_key })
        ));
        let unwrapped_key = service.unwrap_key(0, wrapped_key, None).expect("create_key failed");
        assert_eq!(unwrapped_key, expected_unwrapped_key);
    }

    #[test]
    fn wrap_unwrap_key() {
        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&data_key));

        service
            .add_wrapping_key(u128::to_le_bytes(0), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");
        service
            .set_active_key(u128::to_le_bytes(0), KeyPurpose::Data)
            .expect("set active key failed for data");

        let (wrapping_key_id, wrapped_key, unwrapped_key) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        let unwrap_result = service
            .unwrap_key(
                0,
                WrappedKey::Fxfs(FxfsKey {
                    wrapping_key_id,
                    wrapped_key: wrapped_key.try_into().unwrap(),
                }),
                None,
            )
            .expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped_key);

        // Do it twice to make sure the service can use the same key repeatedly.
        let (wrapping_key_id, wrapped_key, unwrapped_key) =
            service.create_key(1, KeyPurpose::Data).expect("create_key failed");
        let unwrap_result = service
            .unwrap_key(
                1,
                WrappedKey::Fxfs(FxfsKey {
                    wrapping_key_id,
                    wrapped_key: wrapped_key.try_into().unwrap(),
                }),
                None,
            )
            .expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped_key);
    }

    #[test]
    fn wrap_unwrap_key_with_arbitrary_wrapping_key() {
        let service = CryptService::new();
        let key = vec![0xABu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&key));

        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");

        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(0, u128::to_le_bytes(1).into(), ObjectType::File, None)
            .expect("create_key_with_id failed");
        let wrapping_key_id = u128::to_le_bytes(1);
        // TODO(https://fxbug.dev/436902004): Switch to lkb32 wrapped key type.
        match wrapped_key {
            WrappedKey::Fxfs(fxfs_key) => {
                let unwrap_result = service
                    .unwrap_key(
                        0,
                        WrappedKey::Fxfs(FxfsKey {
                            wrapping_key_id,
                            wrapped_key: fxfs_key.wrapped_key.try_into().unwrap(),
                        }),
                        None,
                    )
                    .expect("unwrap_key failed");
                assert_eq!(unwrap_result, unwrapped_key);
            }
            _ => panic!("Found a non-FxfsKey wrapped key"),
        }

        // Do it twice to make sure the service can use the same key repeatedly.
        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(1, u128::to_le_bytes(1).into(), ObjectType::File, None)
            .expect("create_key_with_id failed");
        // TODO(https://fxbug.dev/436902004): Switch to lkb32 wrapped key type.
        match wrapped_key {
            WrappedKey::Fxfs(fxfs_key) => {
                let unwrap_result = service
                    .unwrap_key(
                        1,
                        WrappedKey::Fxfs(FxfsKey {
                            wrapping_key_id,
                            wrapped_key: fxfs_key.wrapped_key.try_into().unwrap(),
                        }),
                        None,
                    )
                    .expect("unwrap_key failed");
                assert_eq!(unwrap_result, unwrapped_key);
            }
            _ => panic!("Found a non-FxfsKey wrapped key"),
        }
    }

    #[test]
    fn create_key_with_wrapping_key_that_does_not_exist() {
        let service = CryptService::new();
        service
            .create_key_with_id(0, u128::to_le_bytes(1).into(), ObjectType::File, None)
            .expect_err("create_key_with_id should fail if the wrapping key does not exist");

        let wrapping_key = vec![0xABu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&wrapping_key));

        let wrapping_key_id = u128::to_le_bytes(1);
        service
            .add_wrapping_key(u128::to_le_bytes(1), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");

        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(0, u128::to_le_bytes(1).into(), ObjectType::File, None)
            .expect("create_key_with_id failed");

        // TODO(https://fxbug.dev/436902004): Switch to lkb32 wrapped key type.
        match wrapped_key {
            WrappedKey::Fxfs(fxfs_key) => {
                let unwrap_result = service
                    .unwrap_key(
                        0,
                        WrappedKey::Fxfs(FxfsKey {
                            wrapping_key_id,
                            wrapped_key: fxfs_key.wrapped_key.try_into().unwrap(),
                        }),
                        None,
                    )
                    .expect("unwrap_key failed");
                assert_eq!(unwrap_result, unwrapped_key);
            }
            _ => panic!("Found a non-FxfsKey wrapped key"),
        }
    }

    #[test]
    fn unwrap_key_wrong_key() {
        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&data_key));

        service
            .add_wrapping_key(u128::to_le_bytes(0), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");
        service
            .set_active_key(u128::to_le_bytes(0), KeyPurpose::Data)
            .expect("set active key failed for data");
        let (wrapping_key_id, mut wrapped_key, _) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        for byte in &mut wrapped_key {
            *byte ^= 0xff;
        }
        service
            .unwrap_key(
                0,
                WrappedKey::Fxfs(FxfsKey {
                    wrapping_key_id,
                    wrapped_key: wrapped_key.try_into().unwrap(),
                }),
                None,
            )
            .expect_err("unwrap_key should fail");
    }

    #[test]
    fn unwrap_key_wrong_owner() {
        let service = CryptService::new();
        let data_key = vec![0xCDu8; 32];
        let cipher = Aes256GcmSiv::new(Key::<Aes256GcmSiv>::from_slice(&data_key));

        service
            .add_wrapping_key(u128::to_le_bytes(0), UserKey::FxfsKey { cipher }, 0)
            .expect("add wrapping key failed");
        service
            .set_active_key(u128::to_le_bytes(0), KeyPurpose::Data)
            .expect("set active key failed for data");

        let (wrapping_key_id, wrapped_key, _) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        service
            .unwrap_key(
                1,
                WrappedKey::Fxfs(FxfsKey {
                    wrapping_key_id,
                    wrapped_key: wrapped_key.try_into().unwrap(),
                }),
                None,
            )
            .expect_err("unwrap_key should fail");
    }
}
