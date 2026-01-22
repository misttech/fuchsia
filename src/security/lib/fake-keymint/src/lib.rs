// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fake implementation of fuchsia.security.keymint.SealingKeys and
//! fuchsia.security.keymint.Admin for testing purposes.
//!
//! IMPORTANT: This implementation is insecure!

use aes_gcm_siv::aead::Aead as _;
use aes_gcm_siv::{Aes128GcmSiv, Key, KeyInit as _, Nonce};
use anyhow::{anyhow, bail};
use fidl::endpoints::{ClientEnd, create_request_stream};
use fidl_fuchsia_security_keymint::{
    AdminMarker, AdminRequest, AdminRequestStream, DeleteError, SealError, SealingKeysMarker,
    SealingKeysRequest, SealingKeysRequestStream, UnsealError, UpgradeError,
};
use fuchsia_sync::Mutex;
use futures::{FutureExt as _, TryStreamExt as _};
use log::warn;
use std::collections::BTreeMap;
use std::collections::btree_map::Entry;
use std::future::Future;
use std::pin::pin;

type KeyInfo = Vec<u8>;

struct SealingKey {
    cipher: Aes128GcmSiv,
    key_blob: Vec<u8>,
}

#[derive(Default)]
struct Inner {
    sealing_keys: BTreeMap<KeyInfo, SealingKey>,
    // Epoch is mixed into keys, and is incremented each time DeleteAllKeys is called.  This ensures
    // that previously deleted keys cannot be reused.
    epoch: u64,
}

impl Inner {
    const IV: [u8; 12] = [0u8; 12];

    // NB: A real Keymint implementation would return a different sealing key each time this is
    // called, and remember the keys that are created.  Since we don't have anywhere to persist
    // them, we just derive the sealing key from the key info directly, and then store the key info
    // as the sealed key blob.  Obviously, this is not secure.
    fn derive_key(key_info: &KeyInfo, epoch: u64) -> SealingKey {
        let mut key_bytes = [0u8; 16];
        let len = key_info.len().min(16);
        key_bytes[..len].copy_from_slice(&key_info[..len]);

        // XOR epoch into the first 8 bytes to ensure keys change with epoch.
        let epoch_bytes = epoch.to_le_bytes();
        for i in 0..8 {
            key_bytes[i] ^= epoch_bytes[i];
        }

        let cipher = Aes128GcmSiv::new(Key::<Aes128GcmSiv>::from_slice(&key_bytes));
        SealingKey { cipher, key_blob: key_info.clone() }
    }

    fn handle_create_request(&mut self, key_info: KeyInfo) -> Vec<u8> {
        match self.sealing_keys.entry(key_info.clone()) {
            Entry::Vacant(vacant) => {
                vacant.insert(Self::derive_key(&key_info, self.epoch)).key_blob.clone()
            }
            Entry::Occupied(occupied) => occupied.get().key_blob.clone(),
        }
    }

    fn handle_seal_request(
        &mut self,
        key_info: KeyInfo,
        key_blob: Vec<u8>,
        secret: Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        let sealing_key =
            self.sealing_keys.get(&key_info).ok_or_else(|| anyhow!("No sealing key"))?;
        if key_blob != sealing_key.key_blob {
            bail!("Wrong key blob");
        }
        let sealed_secret =
            sealing_key.cipher.encrypt(&Nonce::from_slice(&Self::IV), &secret[..])?;
        Ok(sealed_secret)
    }

    fn handle_unseal_request(
        &mut self,
        key_info: KeyInfo,
        key_blob: Vec<u8>,
        sealed_secret: Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        let sealing_key = self
            .sealing_keys
            .entry(key_info.clone())
            .or_insert_with(|| Self::derive_key(&key_info, self.epoch));
        if key_blob != sealing_key.key_blob {
            bail!("Wrong key blob");
        }
        let secret =
            sealing_key.cipher.decrypt(&Nonce::from_slice(&Self::IV), &sealed_secret[..])?;
        Ok(secret)
    }

    fn handle_delete_all_keys_request(&mut self) {
        self.sealing_keys.clear();
        self.epoch += 1;
    }

    fn handle_upgrade_request(
        &mut self,
        _key_info: KeyInfo,
        _key_blob: Vec<u8>,
    ) -> anyhow::Result<Vec<u8>> {
        // Since we store key blobs in a way that is 1-to-1 with `key_info`, do not upgrade keys.
        // This is consistent with keymint behaviour when a client attempts to upgrade a key that
        // does not require an upgrade.
        Ok(vec![])
    }

    fn handle_delete_request(&mut self, key_blob: Vec<u8>) -> anyhow::Result<()> {
        let initial_len = self.sealing_keys.len();
        self.sealing_keys.retain(|_info, sealing_key| &sealing_key.key_blob != &key_blob);
        let modified_len = self.sealing_keys.len();
        if initial_len == modified_len {
            bail!("Failed to locate matching key for deletion");
        }
        if modified_len < initial_len - 1 {
            bail!("Key blob matched multiple key entries");
        }
        Ok(())
    }
}

/// A fake (insecure) implementation of the Keymint FIDL.
#[derive(Default)]
pub struct FakeKeymint {
    inner: Mutex<Inner>,
}

impl FakeKeymint {
    /// Handles [`SealingKeysRequestStream`] to completion.
    pub async fn run_sealing_keys_service(
        &self,
        stream: SealingKeysRequestStream,
    ) -> Result<(), fidl::Error> {
        stream
            .try_for_each_concurrent(None, move |request| async move {
                match request {
                    SealingKeysRequest::CreateSealingKey { key_info, responder } => {
                        responder.send(Ok(&*self.inner.lock().handle_create_request(key_info)))?;
                    }
                    SealingKeysRequest::Seal { key_info, key_blob, secret, responder } => {
                        match self.inner.lock().handle_seal_request(key_info, key_blob, secret) {
                            Ok(sealed_secret) => responder.send(Ok(&*sealed_secret))?,
                            Err(err) => {
                                warn!(err:?; "Failed to seal secret");
                                responder.send(Err(SealError::FailedSeal))?
                            }
                        }
                    }
                    SealingKeysRequest::Unseal { key_info, key_blob, sealed_secret, responder } => {
                        match self.inner.lock().handle_unseal_request(
                            key_info,
                            key_blob,
                            sealed_secret,
                        ) {
                            Ok(secret) => responder.send(Ok(&*secret))?,
                            Err(err) => {
                                warn!(err:?; "Failed to unseal secret");
                                responder.send(Err(UnsealError::FailedUnseal))?
                            }
                        }
                    }
                    SealingKeysRequest::UpgradeSealingKey { key_info, key_blob, responder } => {
                        match self.inner.lock().handle_upgrade_request(key_info, key_blob) {
                            Ok(key) => responder.send(Ok(&*key))?,
                            Err(err) => {
                                warn!(err:?; "Failed to upgrade key");
                                responder.send(Err(UpgradeError::FailedUpgrade))?
                            }
                        }
                    }
                    SealingKeysRequest::DeleteSealingKey { key_blob, responder } => {
                        match self.inner.lock().handle_delete_request(key_blob) {
                            Ok(()) => responder.send(Ok(()))?,
                            Err(err) => {
                                warn!(err:?; "Failed to delete key");
                                responder.send(Err(DeleteError::FailedDelete))?
                            }
                        }
                    }
                }
                Ok(())
            })
            .await
    }

    /// Handles [`AdminRequestStream`] to completion.
    pub async fn run_admin_service(&self, stream: AdminRequestStream) -> Result<(), fidl::Error> {
        stream
            .try_for_each_concurrent(None, move |request| async move {
                match request {
                    AdminRequest::DeleteAllKeys { responder } => {
                        responder.send(Ok(self.inner.lock().handle_delete_all_keys_request()))?;
                    }
                }
                Ok(())
            })
            .await
    }
}

/// Runs `f` with a scoped FakeKeymint instance.  The instance will be automatically terminated on
/// completion.
pub async fn with_keymint_service<R, Fut: Future<Output = anyhow::Result<R>>>(
    f: impl FnOnce(ClientEnd<SealingKeysMarker>, ClientEnd<AdminMarker>) -> Fut,
) -> anyhow::Result<R> {
    let (sealing_keys_client, sealing_keys_stream) = create_request_stream::<SealingKeysMarker>();
    let (admin_client, admin_stream) = create_request_stream::<AdminMarker>();
    let fake_keymint = FakeKeymint::default();
    let mut sealing_keys_service =
        pin!(async { fake_keymint.run_sealing_keys_service(sealing_keys_stream).await }.fuse());
    let mut admin_service =
        pin!(async { fake_keymint.run_admin_service(admin_stream).await }.fuse());
    let mut fut = pin!(f(sealing_keys_client, admin_client).fuse());

    loop {
        futures::select! {
            _ = sealing_keys_service => {}
            _ = admin_service => {}
            result = fut => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn create_seal_unseal() {
        with_keymint_service(|keymint, _| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");
            const SECRET: [u8; 16] = [0xffu8; 16];
            let sealed = keymint
                .seal(&KEY_INFO[..], &key_blob[..], &SECRET[..])
                .await
                .expect("FIDL error")
                .expect("seal error");
            assert_ne!(sealed, SECRET);
            let unsealed = keymint
                .unseal(&KEY_INFO[..], &key_blob[..], &sealed[..])
                .await
                .expect("FIDL error")
                .expect("unseal error");
            assert_eq!(unsealed, SECRET);
            Ok(())
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn seal_failure_on_wrong_key_info() {
        with_keymint_service(|keymint, _| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");
            const SECRET: [u8; 16] = [0xffu8; 16];
            keymint
                .seal(&[2u8; 16], &key_blob[..], &SECRET[..])
                .await
                .expect("FIDL error")
                .expect_err("seal should fail");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn unseal_failure_on_wrong_sealing_key() {
        with_keymint_service(|keymint, _| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");
            const SECRET: [u8; 16] = [0xffu8; 16];
            let sealed = keymint
                .seal(&KEY_INFO[..], &key_blob[..], &SECRET[..])
                .await
                .expect("FIDL error")
                .expect("seal error");
            assert_ne!(sealed, SECRET);
            keymint
                .unseal(&[2u8; 16], &key_blob[..], &sealed[..])
                .await
                .expect("FIDL error")
                .expect_err("unseal should fail");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn unseal_failure_on_wrong_secret_blob() {
        with_keymint_service(|keymint, _| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");
            const SECRET: [u8; 16] = [0xffu8; 16];
            let _ = keymint
                .seal(&KEY_INFO[..], &key_blob[..], &SECRET[..])
                .await
                .expect("FIDL error")
                .expect("seal error");
            keymint
                .unseal(&KEY_INFO[..], &key_blob[..], &[0u8; 16])
                .await
                .expect("FIDL error")
                .expect_err("unseal should fail");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn delete_all_keys_renders_key_unusable() {
        with_keymint_service(|keymint, admin| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");

            let admin = admin.into_proxy();
            admin.delete_all_keys().await.expect("FIDL error").expect("create error");

            const SECRET: [u8; 16] = [0xffu8; 16];
            let _ = keymint
                .seal(&KEY_INFO[..], &key_blob[..], &SECRET[..])
                .await
                .expect("FIDL error")
                .expect_err("seal should fail");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn delete_keys_succeeds() {
        with_keymint_service(|keymint, _admin| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");

            keymint.delete_sealing_key(&key_blob).await.expect("FIDL error").expect("delete error");
            Ok(())
        })
        .await
        .unwrap();
    }

    #[fuchsia::test]
    async fn delete_keys_fails_bad_key_blob() {
        with_keymint_service(|keymint, _admin| async {
            let keymint = keymint.into_proxy();
            const KEY_INFO: [u8; 16] = [1u8; 16];
            let mut key_blob = keymint
                .create_sealing_key(&KEY_INFO[..])
                .await
                .expect("FIDL error")
                .expect("create error");

            key_blob[0] ^= 0xffu8;

            keymint
                .delete_sealing_key(&key_blob)
                .await
                .expect("FIDL error")
                .expect_err("delete success");
            Ok(())
        })
        .await
        .unwrap();
    }
}
