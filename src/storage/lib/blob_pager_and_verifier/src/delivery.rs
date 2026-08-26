// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::PagerVmoCache;
use anyhow::{Error, bail};
use fuchsia_hash::{HASH_SIZE, Hash};
use fuchsia_merkle::{MerkleVerifier, ReadSizedMerkleVerifier};
use mapping::{DELIVERY_VMO_SIZE, DeliveryCommand, RawDeliveryCommand};
use std::sync::Arc;
use zx;

/// The size requirement for verifying payloads delivered from the block driver. The driver must
/// supply `DeliveryCommand::Data` chunks where `offset` and `length` aligns to `DELIVERY_DATA_SIZE`
/// boundaries (unless representing the final chunk of the original data source).
///
/// Having this requirement means we can use `ReadSizedMerkleVerifier`, which optimizes memory usage
/// when verifying reads.
pub const DELIVERY_DATA_SIZE: usize = 128 * 1024;

pub struct DeliveryQueueProcessor {
    delivery_vmo: zx::Vmo,
    delivery_thread: Option<std::thread::JoinHandle<()>>,
}

impl DeliveryQueueProcessor {
    pub fn spawn(
        mut receiver: vmo_fifo::Receiver<RawDeliveryCommand>,
        vmo_cache: &Arc<PagerVmoCache>,
        delivery_vmo: zx::Vmo,
    ) -> Result<Self, Error> {
        let thread_cache = Arc::downgrade(vmo_cache);
        let delivery_vmo_clone = delivery_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;

        let thread = std::thread::spawn(move || {
            while let Ok(msg) = receiver.peek() {
                let Some(vmo_cache) = thread_cache.upgrade() else {
                    // BlobPagerAndVerifier was destroyed.
                    break;
                };

                let raw_cmd = RawDeliveryCommand {
                    opcode: msg.opcode,
                    _padding: msg._padding,
                    key: msg.key,
                    target_offset: msg.target_offset,
                    length: msg.length,
                    offset: msg.offset,
                };

                match DeliveryCommand::try_from(raw_cmd) {
                    Ok(cmd) => {
                        if let Err(e) = Self::handle_delivery(cmd, &msg, &vmo_cache) {
                            log::warn!("Failed to handle delivery command: {:?}", e);
                        }
                    }
                    Err(e) => {
                        log::warn!("Invalid delivery command opcode: {:?}", e);
                    }
                }
                if let Err(e) = msg.pop() {
                    log::warn!("Failed to pop delivery message: {:?}", e);
                    break;
                }
            }
        });

        Ok(Self { delivery_vmo: delivery_vmo_clone, delivery_thread: Some(thread) })
    }

    fn handle_delivery(
        cmd: DeliveryCommand,
        raw_msg: &vmo_fifo::Message<'_, RawDeliveryCommand>,
        vmo_cache: &PagerVmoCache,
    ) -> Result<(), Error> {
        match cmd {
            DeliveryCommand::Data { .. } => {
                // TODO(https://fxbug.dev/535489428): Watch for data delivery events on the delivery
                // queue VMO, cryptographically verify the payloads placed into it by the block
                // driver, and finalize the page fault via `zx_pager_supply_pages`.
                todo!("https://fxbug.dev/535489428: verification not yet implemented");
            }
            DeliveryCommand::RegisterBlob { key, offset, length } => {
                let cached_blob = vmo_cache.get_by_key(key as u32);

                if let Some(blob) = cached_blob {
                    // If this blob is already registered, we can move on. We don't expect the
                    // merkle root to change since the blob is immutable.
                    if blob.merkle_verifier.get().is_some() {
                        log::warn!(
                            "RegisterBlob received for blob {} but it is already initialized",
                            key
                        );
                        return Ok(());
                    }

                    if offset.checked_add(length).unwrap_or(u32::MAX) > DELIVERY_VMO_SIZE as u32 {
                        bail!("RegisterBlob payload out of bounds");
                    }
                    if length % (HASH_SIZE as u32) != 0 {
                        bail!("RegisterBlob invalid leaf length");
                    }

                    let leaf_data = raw_msg.payload_slice(offset, length);

                    let hashes: Vec<Hash> = (0..(length as usize / HASH_SIZE))
                        .map(|i| {
                            let chunk = leaf_data.subslice(i * HASH_SIZE..(i + 1) * HASH_SIZE);
                            let mut hash_bytes = [0u8; HASH_SIZE];
                            chunk.copy_to_slice(&mut hash_bytes);
                            Hash::from(hash_bytes)
                        })
                        .collect();

                    // Instantiate the verifier, which actively hashes and verifies the provided
                    // merkle tree leaves against the trusted root identifier of the blob.
                    match MerkleVerifier::new(
                        Hash::from(blob.identifier),
                        hashes.into_boxed_slice(),
                    ) {
                        Ok(verifier) => {
                            let sized_verifier =
                                ReadSizedMerkleVerifier::new(verifier, DELIVERY_DATA_SIZE)
                                    .map_err(|e| {
                                        anyhow::anyhow!(
                                            "Failed to create ReadSizedMerkleVerifier: {:?}",
                                            e
                                        )
                                    })?;
                            let _ = blob.merkle_verifier.set(sized_verifier);
                        }
                        Err(e) => {
                            bail!("Failed to verify merkle leaves for key {}: {:?}", key, e)
                        }
                    }
                } else {
                    bail!("Unknown or expired key {} in RegisterBlob", key);
                }
            }
        }
        Ok(())
    }
}

impl Drop for DeliveryQueueProcessor {
    fn drop(&mut self) {
        let _ = self.delivery_vmo.signal(zx::Signals::empty(), vmo_fifo::SIG_SHUTDOWN);
        if let Some(thread) = self.delivery_thread.take() {
            let _ = thread.join();
        }
    }
}
