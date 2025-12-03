// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use aes_gcm_siv::aead::Aead;
use aes_gcm_siv::{Aes256GcmSiv, KeyInit as _, Nonce};
use anyhow::{Error, bail};
use fidl_fuchsia_fxfs::{
    CryptCreateKeyResult, CryptRequest, CryptRequestStream, FscryptKeyIdentifier,
    FscryptKeyIdentifierAndNonce, FxfsKey, KeyPurpose, ObjectType, WrappedKey,
};
use fidl_fuchsia_hardware_inlineencryption::DeviceSynchronousProxy;
use fscrypt::hkdf::{HKDF_CONTEXT_INODE_HASH_KEY, HKDF_CONTEXT_KEY_IDENTIFIER, fscrypt_hkdf};
use fuchsia_sync::Mutex;
use futures::stream::StreamExt;
use hkdf::Hkdf;
use linux_uapi::FSCRYPT_KEY_IDENTIFIER_SIZE;
use starnix_uapi::errors::Errno;
use starnix_uapi::{errno, error};
use std::collections::hash_map::{Entry, HashMap};
use std::sync::OnceLock;

// In this implementation of fscrypt, we use a HKDF (Hmac Key Derivation Function) to derive a
// a wrapping key and wrapping key id from the raw key bytes passed in by a user on
// FS_IOC_ADD_ENCRYPTION_KEY. HKDFs requires an input "info" string. We define constants for the
// respective "info" strings here.
const FXFS_FSCRYPT_KEY_IDENTIFIER_INFO: &str = "fscrypt0";
const FXFS_FSCRYPT_WRAPPING_KEY_INFO: &str = "fscrypt1";
const FSCRYPT_HKDF_NONCE_PREFIX: &[u8] = b"fscrypt\0";

const DATA_UNIT_SIZE: u32 = 4096;
const AES256_KEY_SIZE: usize = 32;

/// An fscrypt wrapping key id.
pub type EncryptionKeyId = [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize];

struct KeyInfo {
    users: Vec<u32>,
    key: Box<[u8]>,
    slot: Option<u8>,
}

struct CryptServiceInner {
    keys: HashMap<EncryptionKeyId, KeyInfo>,
    metadata_key: Option<EncryptionKeyId>,
    data_key: Option<EncryptionKeyId>,
}

pub struct CryptService {
    inner: Mutex<CryptServiceInner>,
    use_lblk32_identifiers: bool,
    inline_crypto_proxy: Option<DeviceSynchronousProxy>,
    uuid: OnceLock<[u8; 16]>,
}

impl CryptService {
    /// Creates a new crypt service that supports Starnix user volumes. If `use_lblk32_identifiers`
    /// is true, the key identifiers that are derived for `add_wrapping_key` will use the algorithm
    /// that is used with the the lblk32 fscrypt mode. If false, a deprecated Fuchsia specific
    /// algorithm is used. If `inline_crypto_proxy` is set, hardware wrapped keys will be used.
    pub fn new(
        raw_metadata_key: &[u8],
        raw_data_key: &[u8],
        use_lblk32_identifiers: bool,
        inline_crypto_proxy: Option<DeviceSynchronousProxy>,
    ) -> Self {
        let metadata_wrapping_key_id = if use_lblk32_identifiers {
            derive_lblk32_wrapping_key_id(raw_metadata_key)
        } else {
            derive_fxfs_wrapping_key_id(raw_metadata_key)
        };
        let data_wrapping_key_id = if use_lblk32_identifiers {
            derive_lblk32_wrapping_key_id(raw_data_key)
        } else {
            derive_fxfs_wrapping_key_id(raw_data_key)
        };

        fn to_key_info(key: &[u8]) -> KeyInfo {
            KeyInfo { users: Vec::new(), key: key.into(), slot: None }
        }

        Self {
            inner: Mutex::new(CryptServiceInner {
                keys: HashMap::from_iter([
                    (metadata_wrapping_key_id, to_key_info(raw_metadata_key)),
                    (data_wrapping_key_id, to_key_info(raw_data_key)),
                ]),
                metadata_key: Some(metadata_wrapping_key_id),
                data_key: Some(data_wrapping_key_id),
            }),
            use_lblk32_identifiers,
            inline_crypto_proxy,
            uuid: OnceLock::new(),
        }
    }

    /// Returns true if `key` is registered with the service.
    pub fn contains_key(&self, key: EncryptionKeyId) -> bool {
        let inner = self.inner.lock();
        inner.keys.contains_key(&key)
    }

    /// Returns the users registered for `key`.
    pub fn get_users_for_key(&self, key: EncryptionKeyId) -> Option<Vec<u32>> {
        let inner = self.inner.lock();
        inner.keys.get(&key).map(|x| x.users.clone())
    }

    /// Adds the specified wrapping key for user `uid`.
    pub fn add_wrapping_key(&self, raw_key: &[u8], uid: u32) -> Result<EncryptionKeyId, Errno> {
        let (key, slot) = if let Some(proxy) = self.inline_crypto_proxy.as_ref() {
            let key = Box::from(
                proxy
                    .derive_raw_secret(raw_key, zx::MonotonicInstant::INFINITE)
                    .map_err(|_| errno!(EPIPE))?
                    .map_err(|_| errno!(EPIPE))?,
            );
            let slot = proxy
                .program_key(raw_key, DATA_UNIT_SIZE, zx::MonotonicInstant::INFINITE)
                .map_err(|_| errno!(EPIPE))?
                .map_err(|_| errno!(EPIPE))?;
            (key, Some(slot))
        } else {
            (Box::from(raw_key), None)
        };
        let key_identifier = if self.use_lblk32_identifiers {
            derive_lblk32_wrapping_key_id(&key)
        } else {
            derive_fxfs_wrapping_key_id(&key)
        };
        let mut inner = self.inner.lock();
        match inner.keys.entry(key_identifier) {
            Entry::Occupied(mut e) => {
                let users = &mut e.get_mut().users;
                if !users.contains(&uid) {
                    users.push(uid);
                }
                Ok(key_identifier)
            }
            Entry::Vacant(vacant) => {
                vacant.insert(KeyInfo { users: vec![uid], key, slot });
                Ok(key_identifier)
            }
        }
    }

    /// Serves crypt requests.
    pub async fn handle_connection(&self, mut stream: CryptRequestStream) -> Result<(), Error> {
        while let Some(request) = stream.next().await {
            match request {
                Ok(CryptRequest::CreateKey { owner, purpose, responder }) => {
                    responder
                        .send(match &self.create_key(owner, purpose) {
                            Ok((id, wrapped, key)) => Ok((id, wrapped, key)),
                            Err(e) => Err(*e),
                        })
                        .unwrap_or_else(
                            |error| log::error!(error:?; "Failed to send CreateKey response"),
                        );
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
                            ) {
                                Ok((ref wrapped, ref key)) => Ok((wrapped, key)),
                                Err(e) => Err(e.into_raw()),
                            },
                        )
                        .unwrap_or_else(
                            |error| log::error!(error:?; "Failed to send CreateKeyWithId response"),
                        );
                }
                Ok(CryptRequest::UnwrapKey { owner, wrapped_key, responder }) => {
                    responder
                        .send(match self.unwrap_key(owner, wrapped_key) {
                            Ok(ref unwrapped) => Ok(unwrapped),
                            Err(e) => Err(e.into_raw()),
                        })
                        .unwrap_or_else(
                            |error| log::error!(error:?; "Failed to send UnwrapKey response"),
                        );
                }
                Err(error) => {
                    log::error!(error:?; "Error in CryptRequestStream");
                    bail!(error);
                }
            }
        }
        Ok(())
    }

    /// Removes `wrapping_key_id` for user `uid`.
    pub fn forget_wrapping_key(
        &self,
        wrapping_key_id: EncryptionKeyId,
        uid: u32,
    ) -> Result<(), Errno> {
        let mut inner = self.inner.lock();
        match inner.keys.entry(EncryptionKeyId::from(wrapping_key_id)) {
            Entry::Occupied(mut e) => {
                let user_ids = &mut e.get_mut().users;
                if !user_ids.contains(&uid) {
                    return error!(ENOKEY);
                } else {
                    let index = user_ids.iter().position(|x: &u32| *x == uid).unwrap();
                    user_ids.remove(index);
                    if user_ids.is_empty() {
                        e.remove();
                    }
                }
            }
            Entry::Vacant(_) => {
                return error!(ENOKEY);
            }
        }
        Ok(())
    }

    pub fn set_uuid(&self, uuid: [u8; 16]) {
        self.uuid.set(uuid).unwrap();
    }

    fn derive_directory_key(&self, key: &[u8], nonce: &[u8]) -> Result<Vec<u8>, zx::Status> {
        Ok(fscrypt::to_directory_keys(key, self.uuid.get().ok_or(zx::Status::BAD_STATE)?, nonce)
            .to_unwrapped_key())
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
        let key =
            &inner.keys.get(wrapping_key_id).ok_or_else(|| zx::Status::BAD_STATE.into_raw())?.key;
        let cipher = get_fxfs_cipher(key);
        let nonce = zero_extended_nonce(owner);

        let mut key = [0u8; 32];
        rand::fill(&mut key[..]);

        let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|error| {
            log::error!(error:?; "Failed to wrap key");
            zx::Status::INTERNAL.into_raw()
        })?;

        Ok((*wrapping_key_id, wrapped.into(), key.into()))
    }

    fn create_key_with_id(
        &self,
        owner: u64,
        wrapping_key_id: EncryptionKeyId,
        object_type: ObjectType,
    ) -> Result<(WrappedKey, Vec<u8>), zx::Status> {
        let mut inner = self.inner.lock();
        let key_info = inner.keys.get_mut(&wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;
        match object_type {
            ObjectType::Directory | ObjectType::Symlink => {
                let mut nonce = [0; 16];
                zx::cprng_draw(&mut nonce);
                let unwrapped_key = self.derive_directory_key(&key_info.key, &nonce)?;
                Ok((
                    WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                        key_identifier: wrapping_key_id,
                        nonce,
                    }),
                    unwrapped_key,
                ))
            }
            ObjectType::File => {
                if let Some(slot) = key_info.slot {
                    let unwrapped_key = derive_file_key(slot, &key_info.key);
                    Ok((
                        WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier {
                            key_identifier: wrapping_key_id,
                        }),
                        unwrapped_key,
                    ))
                } else {
                    // Use a software backed key.
                    let cipher = get_fxfs_cipher(&key_info.key);
                    let nonce = zero_extended_nonce(owner);

                    let mut key = [0u8; 32];
                    rand::fill(&mut key[..]);

                    let wrapped = cipher.encrypt(&nonce, &key[..]).map_err(|error| {
                        log::error!(error:?; "Failed to wrap key");
                        zx::Status::INTERNAL
                    })?;

                    Ok((
                        WrappedKey::Fxfs(FxfsKey {
                            wrapping_key_id,
                            wrapped_key: wrapped.try_into().expect("wrapped key wrong size"),
                        }),
                        key.into(),
                    ))
                }
            }
            _ => Err(zx::Status::NOT_SUPPORTED),
        }
    }

    fn unwrap_key(&self, owner: u64, key: WrappedKey) -> Result<Vec<u8>, zx::Status> {
        let mut inner = self.inner.lock();
        match key {
            WrappedKey::Fxfs(FxfsKey { wrapping_key_id, wrapped_key }) => {
                let wrapping_key_id = EncryptionKeyId::from(wrapping_key_id);
                let key_info = inner.keys.get(&wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;

                let cipher = get_fxfs_cipher(&key_info.key);
                let nonce = zero_extended_nonce(owner);

                Ok(cipher
                    .decrypt(&nonce, &wrapped_key[..])
                    .map_err(|_| zx::Status::IO_DATA_INTEGRITY)?)
            }
            WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier { key_identifier }) => {
                let wrapping_key_id = EncryptionKeyId::from(key_identifier);
                let key_info =
                    inner.keys.get_mut(&wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;
                let Some(slot) = key_info.slot else {
                    // The caller is trying to use a hardware wrapped key, but we
                    // don't have access to the wrapping hardware.
                    return Err(zx::Status::UNAVAILABLE);
                };
                Ok(derive_file_key(slot, &key_info.key))
            }
            WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                key_identifier,
                nonce,
            }) => {
                let wrapping_key_id = EncryptionKeyId::from(key_identifier);
                let key_info = inner.keys.get(&wrapping_key_id).ok_or(zx::Status::UNAVAILABLE)?;

                self.derive_directory_key(&key_info.key, &nonce)
            }
            _ => Err(zx::Status::NOT_SUPPORTED),
        }
    }
}

fn zero_extended_nonce(val: u64) -> Nonce {
    let mut nonce = Nonce::default();
    nonce.as_mut_slice()[..8].copy_from_slice(&val.to_le_bytes());
    nonce
}

fn derive_file_key(slot: u8, key: &[u8]) -> Vec<u8> {
    let mut unwrapped_key = Vec::with_capacity(17);
    unwrapped_key.push(slot);
    let ino_hash_key: [u8; 16] = fscrypt_hkdf(key, &[], HKDF_CONTEXT_INODE_HASH_KEY);
    unwrapped_key.extend_from_slice(&ino_hash_key);
    unwrapped_key
}

fn derive_fxfs_wrapping_key_id(raw_key: &[u8]) -> [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize] {
    let hk = Hkdf::<sha2::Sha256>::new(None, raw_key);
    let mut key_identifier: [u8; 16] = [0u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize];
    hk.expand(FXFS_FSCRYPT_KEY_IDENTIFIER_INFO.as_bytes(), &mut key_identifier).unwrap();
    key_identifier
}

fn get_fxfs_cipher(raw_key: &[u8]) -> Aes256GcmSiv {
    let hk = Hkdf::<sha2::Sha256>::new(None, raw_key);
    let mut wrapping_key = [0u8; AES256_KEY_SIZE];
    hk.expand(FXFS_FSCRYPT_WRAPPING_KEY_INFO.as_bytes(), &mut wrapping_key).unwrap();
    Aes256GcmSiv::new((&wrapping_key).into())
}

fn derive_lblk32_wrapping_key_id(raw_key: &[u8]) -> [u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize] {
    let hk = Hkdf::<sha2::Sha512>::new(None, raw_key);
    let mut key_identifier = [0u8; FSCRYPT_KEY_IDENTIFIER_SIZE as usize];
    let mut hkdf_info = FSCRYPT_HKDF_NONCE_PREFIX.to_vec();
    hkdf_info.push(HKDF_CONTEXT_KEY_IDENTIFIER);
    hk.expand(&hkdf_info, &mut key_identifier).unwrap();
    key_identifier
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
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
    const TEST_UUID: [u8; 16] =
        [75, 146, 230, 48, 132, 165, 68, 97, 141, 247, 22, 242, 153, 171, 153, 38];

    async fn handle_inline_crypto_requests(
        mut stream: DeviceRequestStream,
        server: Arc<VmoBackedServer>,
    ) {
        while let Some(Ok(request)) = stream.next().await {
            match request {
                DeviceRequest::ProgramKey { wrapped_key, data_unit_size: _, responder } => {
                    responder
                        .send(Ok(server.program_key(&fscrypt::to_xts_key(&wrapped_key, TEST_UUID))))
                        .unwrap_or_else(|e| {
                            log::error!("failed to send ProgramKey response. error: {:?}", e);
                        });
                }
                DeviceRequest::DeriveRawSecret { mut wrapped_key, responder } => {
                    // Swap the nibbles.
                    for b in &mut wrapped_key {
                        *b = *b >> 4 | *b << 4;
                    }
                    responder.send(Ok(&wrapped_key)).unwrap();
                }
            }
        }
    }

    #[test]
    fn add_and_forget_wrapping_keys() {
        let service = CryptService::new(&[0; 32], &[1; 32], true, None);

        // Add the wrapping key for users 0 and 1
        let wrapping_key_id =
            service.add_wrapping_key(&u128::to_le_bytes(1), 0).expect("add wrapping key failed");

        let wrapping_key_id2 =
            service.add_wrapping_key(&u128::to_le_bytes(1), 1).expect("add wrapping key failed");

        assert_eq!(wrapping_key_id2, wrapping_key_id);

        // A user should be able to add the same key multiple times.
        let wrapping_key_id3 =
            service.add_wrapping_key(&u128::to_le_bytes(1), 1).expect("add wrapping key failed");

        assert_eq!(wrapping_key_id3, wrapping_key_id);

        {
            let inner = service.inner.lock();
            assert_eq!(inner.keys.get(&wrapping_key_id).unwrap().users, [0, 1]);
        }

        // User 1 forgets the wrapping key. Since user 0 still has the key added,
        // create_key_with_id should still succeed.
        service.forget_wrapping_key(wrapping_key_id, 1).expect("forget wrapping key failed");
        service
            .create_key_with_id(0, wrapping_key_id, ObjectType::File)
            .expect("create key with id failed");

        // User 1 cannot forget the same key a second time.
        assert_eq!(
            service.forget_wrapping_key(wrapping_key_id, 1).expect_err(
                "forget wrapping key should fail if the key was already removed by this user"
            ),
            errno!(ENOKEY)
        );
        // Once both users remove the key, create_key_with_id should fail.
        service.forget_wrapping_key(wrapping_key_id, 0).expect("forget wrapping key failed");
        assert_eq!(
            service
                .create_key_with_id(
                    0,
                    EncryptionKeyId::from(u128::to_le_bytes(1)),
                    ObjectType::File,
                )
                .expect_err(
                    "create_key_with_id should fail if the key hasn't been added by the caller"
                ),
            zx::Status::UNAVAILABLE
        );
        service.add_wrapping_key(&u128::to_le_bytes(1), 0).expect("add wrapping key failed");
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

        let service = CryptService::new(&[0; 32], &[1; 32], true, Some(client));
        service.set_uuid(TEST_UUID);
        let wrapping_key_id = service.add_wrapping_key(&[0xdc; 32], 0).unwrap();
        assert_eq!(wrapping_key_id, EXPECTED_WRAPPING_KEY_ID);

        let (_, unwrapped_key) =
            service.create_key_with_id(0, wrapping_key_id, ObjectType::Directory).unwrap();
        let (cts_key, remainder) = unwrapped_key.split_at(EXPECTED_CTS_KEY.len());
        let (ino_hash_key, _dir_hash_key) = remainder.split_at(EXPECTED_INO_HASH_KEY.len());

        assert_eq!(cts_key, &EXPECTED_CTS_KEY);
        assert_eq!(ino_hash_key, &EXPECTED_INO_HASH_KEY);
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

        let data_key = vec![0xCDu8; 32];
        let wrapping_key_id = derive_fxfs_wrapping_key_id(&data_key);
        let cipher = get_fxfs_cipher(&data_key);

        let nonce = zero_extended_nonce(0);

        let key = vec![0xABu8; 32];

        let wrapped = cipher
            .encrypt(&nonce, &key[..])
            .map_err(|e| {
                log::error!("Failed to wrap key error: {:?}", e);
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

        let service = CryptService::new(&[0; 32], &[1; 32], true, Some(client));
        let wrapping_key_id = service.add_wrapping_key(&[0xcd; 32], 0).unwrap();

        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(0, wrapping_key_id, ObjectType::File)
            .expect("create_key failed");
        assert_matches!(wrapped_key, WrappedKey::FscryptInoLblk32File(FscryptKeyIdentifier { .. }));
        let expected_slot = 0;
        assert_eq!(unwrapped_key[0], expected_slot);

        let mut key = [0xcd; 32];
        for b in &mut key {
            *b = *b >> 4 | *b << 4;
        }
        let expected_ino_hash_key: [u8; 16] = fscrypt_hkdf(&key, &[], HKDF_CONTEXT_INODE_HASH_KEY);
        assert_eq!(unwrapped_key[1..17], expected_ino_hash_key);
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

        let service = CryptService::new(&[0; 32], &[1; 32], true, Some(client));
        service.set_uuid(TEST_UUID);
        let wrapping_key_id =
            service.add_wrapping_key(&[0xcd; 32], 0).expect("add wrapping key failed");
        let (wrapped_key, expected_unwrapped_key) = service
            .create_key_with_id(0, wrapping_key_id, ObjectType::Directory)
            .expect("create_key failed");
        assert_matches!(
            wrapped_key,
            WrappedKey::FscryptInoLblk32Dir(FscryptKeyIdentifierAndNonce {
                key_identifier,
                ..
            }) if key_identifier == wrapping_key_id
        );
        let unwrapped_key = service.unwrap_key(0, wrapped_key).expect("create_key failed");
        assert_eq!(unwrapped_key, expected_unwrapped_key);
    }

    #[test]
    fn wrap_unwrap_key() {
        let service = CryptService::new(&[0; 32], &[0xcd; 32], true, None);

        let (wrapping_key_id, wrapped_key, unwrapped_key) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        let unwrap_result = service
            .unwrap_key(
                0,
                WrappedKey::Fxfs(FxfsKey {
                    wrapping_key_id,
                    wrapped_key: wrapped_key.try_into().unwrap(),
                }),
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
            )
            .expect("unwrap_key failed");
        assert_eq!(unwrap_result, unwrapped_key);
    }

    #[test]
    fn wrap_unwrap_key_with_arbitrary_wrapping_key() {
        let service = CryptService::new(&[0; 32], &[1; 32], true, None);

        let wrapping_key_id =
            service.add_wrapping_key(&[2; 32], 0).expect("add wrapping key failed");

        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(0, wrapping_key_id, ObjectType::File)
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
                    )
                    .expect("unwrap_key failed");
                assert_eq!(unwrap_result, unwrapped_key);
            }
            _ => panic!("Found a non-FxfsKey wrapped key"),
        }

        // Do it twice to make sure the service can use the same key repeatedly.
        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(1, wrapping_key_id, ObjectType::File)
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
                    )
                    .expect("unwrap_key failed");
                assert_eq!(unwrap_result, unwrapped_key);
            }
            _ => panic!("Found a non-FxfsKey wrapped key"),
        }
    }

    #[test]
    fn create_key_with_wrapping_key_that_does_not_exist() {
        let service = CryptService::new(&[0; 32], &[1; 32], true, None);

        let wrapping_key_id =
            service.add_wrapping_key(&[2; 32], 0).expect("add wrapping key failed");

        let (wrapped_key, unwrapped_key) = service
            .create_key_with_id(0, wrapping_key_id, ObjectType::File)
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
                    )
                    .expect("unwrap_key failed");
                assert_eq!(unwrap_result, unwrapped_key);
            }
            _ => panic!("Found a non-FxfsKey wrapped key"),
        }

        service.forget_wrapping_key(wrapping_key_id, 0).unwrap();

        service
            .create_key_with_id(0, wrapping_key_id, ObjectType::File)
            .expect_err("create_key_with_id should fail if the wrapping key does not exist");
    }

    #[test]
    fn unwrap_key_wrong_key() {
        let service = CryptService::new(&[0; 32], &[0xcd; 32], true, None);
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
            )
            .expect_err("unwrap_key should fail");
    }

    #[test]
    fn unwrap_key_wrong_owner() {
        let service = CryptService::new(&[0; 32], &[0xcd; 32], true, None);

        let (wrapping_key_id, wrapped_key, _) =
            service.create_key(0, KeyPurpose::Data).expect("create_key failed");
        service
            .unwrap_key(
                1,
                WrappedKey::Fxfs(FxfsKey {
                    wrapping_key_id,
                    wrapped_key: wrapped_key.try_into().unwrap(),
                }),
            )
            .expect_err("unwrap_key should fail");
    }

    #[fuchsia::test]
    async fn test_add_wrapping_key_uses_raw_key() {
        let (client, server) = fidl::endpoints::create_sync_proxy::<DeviceMarker>();
        let raw_key_bytes = [0xAB; 32];
        let expected_key = raw_key_bytes.clone();

        std::thread::spawn(move || {
            LocalExecutor::default().run_singlethreaded(async move {
                let mut stream = server.into_stream();
                while let Some(Ok(request)) = stream.next().await {
                    match request {
                        DeviceRequest::DeriveRawSecret { wrapped_key, responder } => {
                            let mut derived = wrapped_key.clone();
                            derived[0] ^= 0xFF;
                            responder.send(Ok(&derived)).unwrap();
                        }
                        DeviceRequest::ProgramKey { wrapped_key, responder, .. } => {
                            if wrapped_key == expected_key {
                                responder.send(Ok(0)).unwrap();
                            } else {
                                responder.send(Err(zx::Status::INVALID_ARGS.into_raw())).unwrap();
                            }
                        }
                    }
                }
            })
        });

        let service = CryptService::new(&[0; 32], &[1; 32], true, Some(client));
        service.add_wrapping_key(&raw_key_bytes, 0).expect("add_wrapping_key failed");
    }
}
