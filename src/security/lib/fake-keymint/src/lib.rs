// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! A fake implementation of fuchsia.security.keymint.SealingKeys for testing purposes.
//!
//! IMPORTANT: This implementation is insecure!

use aes_gcm_siv::aead::Aead as _;
use aes_gcm_siv::{Aes128GcmSiv, Key, KeyInit as _, Nonce};
use anyhow::{anyhow, bail};
use fidl::endpoints::{ClientEnd, create_request_stream};
use fidl_fuchsia_security_keymint::{
    SealError, SealingKeysMarker, SealingKeysRequest, SealingKeysRequestStream, UnsealError,
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
}

impl Inner {
    const IV: [u8; 12] = [0u8; 12];

    // NB: A real Keymint implementation would return a different sealing key each time this is
    // called, and remember the keys that are created.  Since we don't have anywhere to persist
    // them, just directly use the KeyInfo as the key.  Obviously, this is not secure.
    fn derive_key(key_info: &KeyInfo) -> SealingKey {
        let mut extended_key: [u8; 16] = [0; 16];
        extended_key[..key_info.len()].copy_from_slice(&key_info[..]);
        let cipher = Aes128GcmSiv::new(Key::<Aes128GcmSiv>::from_slice(&extended_key));
        SealingKey { cipher, key_blob: key_info.clone() }
    }

    fn handle_create_request(&mut self, key_info: KeyInfo) -> Vec<u8> {
        match self.sealing_keys.entry(key_info.clone()) {
            Entry::Vacant(vacant) => vacant.insert(Self::derive_key(&key_info)).key_blob.clone(),
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
            .or_insert_with(|| Self::derive_key(&key_info));
        if key_blob != sealing_key.key_blob {
            bail!("Wrong key blob");
        }
        let secret =
            sealing_key.cipher.decrypt(&Nonce::from_slice(&Self::IV), &sealed_secret[..])?;
        Ok(secret)
    }
}

/// A fake (insecure) implementation of the Keymint FIDL.
#[derive(Default)]
pub struct FakeKeymint {
    inner: Mutex<Inner>,
}

impl FakeKeymint {
    /// Handles requests from `stream` to completion.
    pub async fn run_keymint_service(
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
                }
                Ok(())
            })
            .await
    }
}

/// Runs `f` with a scoped FakeKeymint instance.  The instance will be automatically terminated on
/// completion.
pub async fn with_keymint_service<R, Fut: Future<Output = anyhow::Result<R>>>(
    f: impl FnOnce(ClientEnd<SealingKeysMarker>) -> Fut,
) -> anyhow::Result<R> {
    let (client, stream) = create_request_stream::<SealingKeysMarker>();
    let mut service =
        pin!(async { FakeKeymint::default().run_keymint_service(stream).await }.fuse());
    let mut fut = pin!(f(client).fuse());

    loop {
        futures::select! {
            _ = service => {}
            result = fut => return result,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    async fn create_seal_unseal() {
        with_keymint_service(|keymint| async {
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
        with_keymint_service(|keymint| async {
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
        with_keymint_service(|keymint| async {
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
        with_keymint_service(|keymint| async {
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
}
