// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, bail};
use fuchsia_sync::Mutex;
use mapping::{DELIVERY_VMO_SIZE, DeliveryCommand, RawDeliveryCommand};
use std::collections::HashMap;
use std::sync::Arc;
use storage_ptr_slice::PtrByteSlice;
use zx;

/// The size requirement for verifying payloads delivered from the block driver. The driver must
/// supply `DeliveryCommand::Data` chunks where `offset` and `length` aligns to `DELIVERY_DATA_SIZE`
/// boundaries (unless representing the final chunk of the original data source).
///
/// Having this requirement means we can use `ReadSizedMerkleVerifier`, which optimizes memory usage
/// when verifying reads.
pub const DELIVERY_DATA_SIZE: usize = 128 * 1024;

/// Trait providing data delivery and blob metadata registration for incoming delivery commands.
pub trait DeliveryQueueProvider: Send + Sync + 'static {
    /// Delivers data directly from the delivery VMO into the target blob.
    fn deliver_pages(
        &self,
        key: u64,
        target_offset: u64,
        length: u64,
        delivery_offset: u64,
    ) -> Result<(), Error>;

    /// Handles a RegisterBlob command using a pointer slice to shared memory.
    fn register_blob(&self, key: u64, leaf_data: PtrByteSlice<'_>) -> Result<(), Error> {
        let _ = (key, leaf_data);
        Ok(())
    }
}

/// A simple in-memory `DeliveryQueueProvider` for testing delivery without Merkle verification.
pub struct TestVmoProvider {
    pager: Arc<zx::Pager>,
    delivery_vmo: zx::Vmo,
    vmos: Mutex<HashMap<u64, zx::Vmo>>,
}

impl TestVmoProvider {
    pub fn new(pager: Arc<zx::Pager>, delivery_vmo: zx::Vmo) -> Self {
        Self { pager, delivery_vmo, vmos: Mutex::new(HashMap::new()) }
    }

    pub fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    pub fn register_vmo(&self, key: u64, vmo: zx::Vmo) {
        self.vmos.lock().insert(key, vmo);
    }

    pub fn unregister_vmo(&self, key: u64) {
        self.vmos.lock().remove(&key);
    }
}

impl DeliveryQueueProvider for TestVmoProvider {
    fn deliver_pages(
        &self,
        key: u64,
        target_offset: u64,
        length: u64,
        delivery_offset: u64,
    ) -> Result<(), Error> {
        let vmos = self.vmos.lock();
        if let Some(target_vmo) = vmos.get(&key) {
            self.pager.supply_pages(
                target_vmo,
                target_offset..target_offset + length,
                &self.delivery_vmo,
                delivery_offset,
            )?;
            Ok(())
        } else {
            bail!("Unknown or expired key {key} in deliver_pages");
        }
    }
}

pub struct DeliveryQueueProcessor {
    delivery_vmo: zx::Vmo,
    delivery_thread: Option<std::thread::JoinHandle<()>>,
}

impl DeliveryQueueProcessor {
    pub fn spawn(
        mut receiver: vmo_fifo::Receiver<RawDeliveryCommand>,
        provider: Arc<dyn DeliveryQueueProvider>,
        delivery_vmo: zx::Vmo,
    ) -> Result<Self, Error> {
        let delivery_vmo_clone = delivery_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;

        let thread = std::thread::spawn(move || {
            while let Ok(msg) = receiver.peek() {
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
                        if let Err(e) = Self::handle_delivery(cmd, &msg, &*provider) {
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

    pub fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    fn handle_delivery(
        cmd: DeliveryCommand,
        raw_msg: &vmo_fifo::Message<'_, RawDeliveryCommand>,
        provider: &dyn DeliveryQueueProvider,
    ) -> Result<(), Error> {
        match cmd {
            DeliveryCommand::Data { key, target_offset, length, offset } => {
                if offset.checked_add(length).unwrap_or(u32::MAX) > DELIVERY_VMO_SIZE as u32 {
                    bail!("Data payload out of bounds");
                }
                let vmo_offset = raw_msg.payload_region_offset() as u64 + offset as u64;
                provider.deliver_pages(key, target_offset, length as u64, vmo_offset)?;
            }
            DeliveryCommand::RegisterBlob { key, offset, length } => {
                if offset.checked_add(length).unwrap_or(u32::MAX) > DELIVERY_VMO_SIZE as u32 {
                    bail!("RegisterBlob payload out of bounds");
                }
                let leaf_data = raw_msg.payload_slice(offset, length);
                provider.register_blob(key, leaf_data)?;
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
