// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow, bail};
use fuchsia_sync::Mutex;
use mapping::{DELIVERY_VMO_SIZE, DeliveryCommand, RawDeliveryCommand};
use std::collections::HashMap;
use std::sync::Arc;
use storage_ptr_slice::{MutPtrByteSlice, PtrByteSlice};
use zx;

/// Unverified data pages delivered from the driver to be verified.
#[derive(Copy, Clone)]
pub struct UnverifiedPages<'a> {
    slice: PtrByteSlice<'a>,
    /// Byte offset within the shared delivery VMO.
    delivery_offset: u64,
}

impl<'a> UnverifiedPages<'a> {
    pub(crate) fn new(slice: PtrByteSlice<'a>, delivery_offset: u64) -> Result<Self, zx::Status> {
        let page_size = zx::system_get_page_size() as usize;
        if (slice.len() & (page_size - 1)) != 0 || (delivery_offset & (page_size as u64 - 1)) != 0 {
            return Err(zx::Status::INVALID_ARGS);
        }
        Ok(Self { slice, delivery_offset })
    }

    /// Returns the length of the payload in bytes.
    pub fn len_in_bytes(&self) -> usize {
        self.slice.len()
    }

    /// Returns the length of the payload in pages.
    pub fn len_in_pages(&self) -> usize {
        self.slice.len() / (zx::system_get_page_size() as usize)
    }

    /// Returns the byte offset within the shared delivery VMO.
    pub fn delivery_offset(&self) -> u64 {
        self.delivery_offset
    }

    /// Returns the underlying raw pointer byte slice to the unverified data.
    pub fn as_ptr_byte_slice(&self) -> PtrByteSlice<'a> {
        self.slice
    }
}

/// Parameters for delivering verified parent blob data (or sparse zeroes) into a child VMO.
pub struct ChildData<'a> {
    pub parent_offset: u64,
    pub offset: u32,
    pub length: u32,
    pub payload_region_offset: u64,
    pub payload: Option<MutPtrByteSlice<'a>>,
    pub child_key: u64,
    pub child_target_offset: u64,
    pub in_chunk_offset: u32,
    pub child_length: u32,
}

/// Parameters for delivering an EROFS inline tail directly from the parent blob's VMO in
/// `pkg-cache`.
#[derive(Copy, Clone, Debug)]
pub struct ChildInlineTail {
    pub parent_byte_offset: u64,
    pub child_key: u64,
    pub child_target_offset: u64,
    pub tail_length: u32,
}

/// Trait providing data delivery and blob metadata registration for incoming delivery commands.
pub trait DeliveryQueueProvider: Send + Sync + 'static {
    /// Delivers data directly from the delivery VMO into the target blob.
    fn deliver_pages(
        &self,
        key: u64,
        target_offset: u64,
        unverified_pages: UnverifiedPages<'_>,
    ) -> Result<(), Error>;

    /// Handles a RegisterBlob command using a pointer slice to shared memory.
    fn register_blob(
        &self,
        key: u64,
        leaf_data: PtrByteSlice<'_>,
        read_alignment: usize,
    ) -> Result<(), Error> {
        let _ = (key, leaf_data, read_alignment);
        Ok(())
    }

    /// Verifies a parent blob chunk (when `data.length > 0`), applies file-tail zeroing in-place
    /// in the delivery queue VMO, and supplies the resulting pages to the child VMO.
    fn deliver_child_pages(&self, _data: ChildData<'_>) -> Result<(), Error> {
        unreachable!()
    }

    /// Supplies an EROFS inline tail page by reading directly from the parent blob's VMO.
    fn deliver_child_inline_tail(
        &self,
        _data: ChildInlineTail,
        _receiver: &vmo_fifo::Receiver<RawDeliveryCommand>,
    ) -> Result<(), Error> {
        unreachable!()
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
        unverified_pages: UnverifiedPages<'_>,
    ) -> Result<(), Error> {
        let vmos = self.vmos.lock();
        if let Some(target_vmo) = vmos.get(&key) {
            let length = unverified_pages.len_in_bytes() as u64;
            if length == 0 {
                return Ok(());
            }
            self.pager.supply_pages(
                target_vmo,
                target_offset..target_offset + length,
                &self.delivery_vmo,
                unverified_pages.delivery_offset(),
            )?;
            Ok(())
        } else {
            bail!("Unknown or expired key {key} in deliver_pages");
        }
    }
}

const NUM_DELIVERY_THREADS: usize = 2;
const DELIVERY_THREAD_ROLE: &str = "fuchsia.storage.blob.delivery";

pub struct DeliveryQueueProcessor {
    delivery_vmo: zx::Vmo,
    delivery_threads: Vec<std::thread::JoinHandle<()>>,
}

impl DeliveryQueueProcessor {
    pub fn spawn(
        receiver: vmo_fifo::Receiver<RawDeliveryCommand>,
        provider: Arc<dyn DeliveryQueueProvider>,
        delivery_vmo: zx::Vmo,
    ) -> Result<Self, Error> {
        let receiver = Arc::new(receiver);
        let mut delivery_threads = Vec::with_capacity(NUM_DELIVERY_THREADS);

        for _ in 0..NUM_DELIVERY_THREADS {
            let receiver = receiver.clone();
            let provider = provider.clone();
            let thread =
                std::thread::Builder::new().name("blob-delivery".to_string()).spawn(move || {
                    Self::worker_loop(receiver, provider);
                })?;
            delivery_threads.push(thread);
        }

        Ok(Self { delivery_vmo, delivery_threads })
    }

    pub fn delivery_vmo(&self) -> &zx::Vmo {
        &self.delivery_vmo
    }

    fn worker_loop(
        receiver: Arc<vmo_fifo::Receiver<RawDeliveryCommand>>,
        provider: Arc<dyn DeliveryQueueProvider>,
    ) {
        if let Err(error) = fuchsia_scheduler::set_role_for_this_thread(DELIVERY_THREAD_ROLE) {
            log::warn!(error:?; "Failed to set delivery thread role");
        }
        while let Ok((mut msg, maybe_cmd)) = receiver.claim_with(|msg| {
            match DeliveryCommand::try_from(**msg) {
                // Process `RegisterBlob` before releasing the claim lock so that any subsequent
                // `Data` commands for this blob claimed by other worker threads observe the
                // initialized Merkle verifier.
                Ok(cmd @ DeliveryCommand::RegisterBlob { .. }) => {
                    if let Err(error) = Self::handle_delivery(cmd, msg, &*provider) {
                        log::warn!(error:?; "Failed to handle delivery command");
                    }
                    None
                }
                Ok(cmd) => Some(cmd),
                Err(error) => {
                    log::warn!(error:?; "Invalid delivery command opcode");
                    None
                }
            }
        }) {
            if let Some(DeliveryCommand::ChildInlineTail {
                parent_byte_offset,
                child_key,
                child_target_offset,
                tail_length,
            }) = maybe_cmd
            {
                // Pop the header-only command from the FIFO before reading from the parent blob's
                // VMO so `read_head` can advance even if the parent VMO read faults on an evicted
                // or not-yet-supplied parent page.
                if let Err(error) = msg.pop() {
                    log::warn!(error:?; "Failed to pop delivery message");
                    break;
                }
                if let Err(error) = provider.deliver_child_inline_tail(
                    ChildInlineTail {
                        parent_byte_offset,
                        child_key,
                        child_target_offset,
                        tail_length,
                    },
                    &receiver,
                ) {
                    log::warn!(error:?; "Failed to handle ChildInlineTail command");
                }
                continue;
            }

            if let Some(cmd) = maybe_cmd
                && let Err(error) = Self::handle_delivery(cmd, &mut msg, &*provider)
            {
                log::warn!(error:?; "Failed to handle delivery command");
            }
            if let Err(error) = msg.pop() {
                log::warn!(error:?; "Failed to pop delivery message");
                break;
            }
        }
    }

    fn is_payload_in_bounds(
        raw_msg: &vmo_fifo::Message<'_, RawDeliveryCommand>,
        offset: u32,
        length: u32,
    ) -> bool {
        raw_msg
            .payload_region_offset()
            .checked_add(offset as usize)
            .and_then(|start| start.checked_add(length as usize))
            .is_some_and(|end| end <= DELIVERY_VMO_SIZE as usize)
    }

    fn handle_delivery(
        cmd: DeliveryCommand,
        raw_msg: &mut vmo_fifo::Message<'_, RawDeliveryCommand>,
        provider: &dyn DeliveryQueueProvider,
    ) -> Result<(), Error> {
        match cmd {
            DeliveryCommand::Data { key, target_offset, length, offset } => {
                if !Self::is_payload_in_bounds(raw_msg, offset, length) {
                    bail!("Data payload out of bounds");
                }
                let vmo_offset = raw_msg.payload_region_offset() as u64 + offset as u64;
                // The sender is expected to push page-aligned payloads. If the payload is the
                // final chunk of a blob and the data does not end on a page boundary, the sender
                // must extend `length` to the next page boundary and pad the trailing bytes with
                // zeros. This is required because `fuchsia-merkle`'s `verify_aligned` expects
                // the buffer length to be a multiple of the system page size.
                let page_size = zx::system_get_page_size() as u32;
                let page_mask = page_size - 1;
                if (offset & page_mask) != 0 {
                    bail!("Delivery payload offset must be page aligned: {offset}");
                }
                if (length & page_mask) != 0 {
                    bail!("Delivery payload length must be page aligned: {length}");
                }
                if (target_offset & (page_mask as u64)) != 0 {
                    bail!("Delivery payload target_offset must be page aligned: {target_offset}");
                }
                let unverified_pages =
                    UnverifiedPages::new(raw_msg.payload_slice(offset, length), vmo_offset)
                        .map_err(|s| anyhow!("Invalid unverified pages: {s}"))?;
                provider.deliver_pages(key, target_offset, unverified_pages)?;
            }
            DeliveryCommand::RegisterBlob { key, offset, length, read_alignment } => {
                if !Self::is_payload_in_bounds(raw_msg, offset, length) {
                    bail!("RegisterBlob payload out of bounds");
                }
                let leaf_data = raw_msg.payload_slice(offset, length);
                provider.register_blob(key, leaf_data, read_alignment as usize)?;
            }
            DeliveryCommand::ChildData {
                parent_offset,
                offset,
                length,
                child_key,
                child_target_offset,
                in_chunk_offset,
                child_length,
            } => {
                let payload_region_offset = raw_msg.payload_region_offset() as u64;
                let payload = (length > 0 && Self::is_payload_in_bounds(raw_msg, offset, length))
                    .then(|| raw_msg.payload_mut_slice(offset, length));
                provider.deliver_child_pages(ChildData {
                    parent_offset,
                    offset,
                    length,
                    payload_region_offset,
                    payload,
                    child_key,
                    child_target_offset,
                    in_chunk_offset,
                    child_length,
                })?;
            }
            DeliveryCommand::ChildInlineTail { .. } => {
                unreachable!("ChildInlineTail is handled before handle_delivery");
            }
        }
        Ok(())
    }
}

impl Drop for DeliveryQueueProcessor {
    fn drop(&mut self) {
        let _ = self.delivery_vmo.signal(zx::Signals::empty(), vmo_fifo::SIG_SHUTDOWN);
        for thread in self.delivery_threads.drain(..) {
            let _ = thread.join();
        }
    }
}
