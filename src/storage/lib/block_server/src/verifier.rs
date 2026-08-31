// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use fuchsia_sync::Mutex;
use mapping::{
    DELIVERY_DATA_COMMAND, PENDING_DELIVERY_COMMANDS_CAPACITY, PageRequest, RawDeliveryCommand,
};
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;
use vmo_fifo::SyncSender;

/// Manages data verification and page supply via the delivery queue VMO.
pub struct Verifier {
    sender: Mutex<Option<SyncSender<RawDeliveryCommand>>>,
}

impl Verifier {
    pub fn new(delivery_queue: zx::Vmo) -> Self {
        let sender = if delivery_queue.get_size().unwrap_or(0) > 0 {
            // Payloads in the delivery queue are transferred to the kernel pager via
            // `zx_pager_supply_pages`, which requires the source offset to be page-aligned. Set
            // the alignment of payload offsets to be page aligned.
            SyncSender::<RawDeliveryCommand>::new(
                delivery_queue,
                zx::system_get_page_size() as usize,
                PENDING_DELIVERY_COMMANDS_CAPACITY,
            )
            .ok()
        } else {
            None
        };
        Self { sender: Mutex::new(sender) }
    }

    /// Returns a [`PageRequest`] implementation for delivering page-in data.
    pub fn get_page_request(self: &Arc<Self>, key: u64, original_range: Range<u64>) -> Buffer {
        Buffer {
            verifier: Arc::clone(self),
            key,
            read_range: original_range,
            committed_len: 0,
            data: Vec::new(),
        }
    }
}

/// Implementation of [`PageRequest`] for verifier deliveries that forwards
/// unverified chunks via the delivery queue.
pub struct Buffer {
    verifier: Arc<Verifier>,
    key: u64,
    read_range: Range<u64>,
    committed_len: usize,
    data: Vec<u8>,
}

impl DataBuffer for Buffer {
    fn range(&self) -> Range<u64> {
        self.read_range.clone()
    }

    /// Returns a mutable slice into the mapped transfer buffer.
    ///
    /// # Panics
    ///
    /// Panics if `prepare()` has not been called prior to accessing this method.
    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        assert!(!self.data.is_empty(), "prepare must be called before accessing mut_ptr_slice");
        MutPtrByteSlice::from(&mut self.data[self.committed_len..])
    }

    fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        // TODO(https://fxbug.dev/530494057): Add support for verification and optimize buffer
        // management / payload reservations.
        let chunk_data = &self.data[self.committed_len..self.committed_len + size];
        let offset = self.read_range.start + self.committed_len as u64;

        let mut sender_guard = self.verifier.sender.lock();
        if let Some(sender) = sender_guard.as_mut() {
            let page_size = zx::system_get_page_size() as usize;
            let aligned_size = size.div_ceil(page_size) * page_size;
            let mut payload = sender
                .reserve_payload(aligned_size)
                .map_err(|_| ChunkedArchiveError::IntegrityError)?;

            let payload_data = payload.data();
            payload_data.subslice_mut(0..size).copy_from_slice(chunk_data);
            payload_data.subslice_mut(size..aligned_size).fill(0);
            let cmd = RawDeliveryCommand {
                opcode: DELIVERY_DATA_COMMAND,
                _padding: 0,
                key: self.key,
                target_offset: offset,
                length: aligned_size as u32,
                offset: payload.offset(),
            };
            payload.commit(cmd).map_err(|_| ChunkedArchiveError::IntegrityError)?;
        }

        self.committed_len += size;
        Ok(())
    }
}

impl PageRequest for Buffer {
    fn prepare(&mut self, read_range: Range<u64>) -> Result<(), ChunkedArchiveError> {
        assert!(self.data.is_empty(), "prepare must only be called once");
        self.read_range = read_range;
        let len = (self.read_range.end - self.read_range.start) as usize;
        self.data = vec![0u8; len];
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_verifier_buffer_incremental_commit() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue));
        let key = 42u64;

        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");

        // Fill first page (4096 bytes) with 0xAA
        let page1_data = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        request.commit(4096).expect("commit page 1");

        // Verify mut_ptr_slice advanced to second page
        assert_eq!(request.mut_ptr_slice().len(), 4096);

        // Fill second page (4096 bytes) with 0xBB
        let page2_data = vec![0xBBu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2_data);
        request.commit(4096).expect("commit page 2");

        // Verify remaining space is 0
        assert_eq!(request.mut_ptr_slice().len(), 0);

        // Check messages delivered on queue
        let msg1 = receiver.peek().expect("msg1");
        assert_eq!(msg1.opcode, DELIVERY_DATA_COMMAND);
        assert_eq!(msg1.key, key);
        assert_eq!(msg1.length, 4096);
        assert_eq!(msg1.target_offset, 0);
        let mut buf1 = vec![0u8; 4096];
        msg1.payload_slice(msg1.offset, msg1.length).copy_to_slice(&mut buf1);
        assert_eq!(buf1, page1_data);
        msg1.pop().expect("pop msg1");

        let msg2 = receiver.peek().expect("msg2");
        assert_eq!(msg2.opcode, DELIVERY_DATA_COMMAND);
        assert_eq!(msg2.key, key);
        assert_eq!(msg2.length, 4096);
        assert_eq!(msg2.target_offset, 4096);
        let mut buf2 = vec![0u8; 4096];
        msg2.payload_slice(msg2.offset, msg2.length).copy_to_slice(&mut buf2);
        assert_eq!(buf2, page2_data);
        msg2.pop().expect("pop msg2");
    }

    #[fuchsia::test]
    fn test_verifier_buffer_page_aligned_commits() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue));
        let key = 43u64;

        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");

        let mut data = vec![0xCCu8; 5000];
        data.resize(8192, 0);

        // Commit page 1 (4096 bytes)
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&data[..4096]);
        request.commit(4096).expect("commit 4096 bytes");

        // Commit page 2 (rounded up to 4096 bytes: 904 bytes payload + padding 0s)
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&data[4096..8192]);
        request.commit(4096).expect("commit 4096 bytes");

        let msg1 = receiver.peek().expect("msg1");
        assert_eq!(msg1.target_offset, 0);
        let mut buf1 = vec![0u8; 4096];
        msg1.payload_slice(msg1.offset, msg1.length).copy_to_slice(&mut buf1);
        assert_eq!(&buf1[..], &data[..4096]);
        msg1.pop().expect("pop msg1");

        let msg2 = receiver.peek().expect("msg2");
        assert_eq!(msg2.target_offset, 4096);
        let mut buf2 = vec![0u8; 4096];
        msg2.payload_slice(msg2.offset, msg2.length).copy_to_slice(&mut buf2);
        assert_eq!(&buf2[..], &data[4096..8192]);
        msg2.pop().expect("pop msg2");
    }

    #[fuchsia::test]
    #[should_panic(expected = "prepare must only be called once")]
    fn test_verifier_buffer_prepare_multiple_times_panics() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let verifier = Arc::new(Verifier::new(delivery_queue));

        let key = 46u64;
        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..4096).expect("first prepare");
        let _ = request.prepare(0..8192);
    }

    #[fuchsia::test]
    fn test_verifier_buffer_supplies_pages_via_delivery_receiver() {
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let port = zx::Port::create();
        let key = 44u64;
        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, 8192).unwrap();

        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let vmo_provider = Arc::new(blob_pager_and_verifier::TestVmoProvider::new(
            pager.clone(),
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));
        vmo_provider
            .register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());
        let receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();
        let _processor = blob_pager_and_verifier::DeliveryQueueProcessor::spawn(
            receiver,
            vmo_provider,
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue));
        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");

        let page1_data = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        request.commit(4096).expect("commit page 1");

        let page2_data = vec![0xBBu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2_data);
        request.commit(4096).expect("commit page 2");

        let mut read_buf = vec![0u8; 8192];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo");
        assert_eq!(&read_buf[..4096], &page1_data[..]);
        assert_eq!(&read_buf[4096..8192], &page2_data[..]);
    }
}
