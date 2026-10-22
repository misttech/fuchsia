// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use fuchsia_sync::Mutex;
use mapping::{
    DeliveryCommand, DeliveryHandler, PENDING_DELIVERY_COMMANDS_CAPACITY, PageRequest,
    RawDeliveryCommand,
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
}

impl DeliveryHandler for Verifier {
    type Request = Buffer;

    /// Returns a [`PageRequest`] implementation for delivering page-in data.
    fn get_page_request(self: &Arc<Self>, key: u64, original_range: Range<u64>) -> Buffer {
        Buffer {
            verifier: Arc::clone(self),
            key,
            read_range: original_range,
            read_alignment: 0,
            committed_len: 0,
            delivered_len: 0,
            data: Vec::new(),
            child_target: None,
        }
    }

    /// Returns a [`Buffer`] configured to deliver parent blob chunks for a child file extent via
    /// [`DeliveryCommand::ChildData`].
    fn get_child_page_request(
        self: &Arc<Self>,
        child_key: u64,
        child_target_offset: u64,
        parent_byte_range: Range<u64>,
    ) -> Buffer {
        Buffer {
            verifier: Arc::clone(self),
            key: child_key,
            read_range: parent_byte_range.clone(),
            read_alignment: 0,
            committed_len: 0,
            delivered_len: 0,
            data: Vec::new(),
            child_target: Some(ChildTarget { child_target_offset, parent_byte_range }),
        }
    }

    /// Pushes a header-only [`DeliveryCommand::ChildInlineTail`] command onto the delivery queue
    /// so the verifier can supply the child tail page from the verified parent VMO.
    fn deliver_child_inline_tail(
        self: &Arc<Self>,
        parent_byte_offset: u64,
        child_key: u64,
        child_target_offset: u64,
        tail_length: u32,
    ) {
        let mut sender = self.sender.lock();
        if let Some(sender) = sender.as_mut() {
            let cmd = DeliveryCommand::ChildInlineTail {
                parent_byte_offset,
                child_key,
                child_target_offset,
                tail_length,
            };
            let _ = sender.push(cmd.into());
        }
    }

    /// Pushes a [`DeliveryCommand::ChildData`] command with `length == 0` onto the delivery queue
    /// to supply a zero-filled sparse range to `child_key`.
    fn deliver_child_sparse(
        &self,
        child_key: u64,
        child_target_offset: u64,
        child_length: u64,
    ) -> Result<(), anyhow::Error> {
        let mut sender = self.sender.lock();
        if let Some(sender) = sender.as_mut() {
            let cmd = DeliveryCommand::ChildData {
                parent_offset: 0,
                offset: 0,
                length: 0,
                child_key,
                child_target_offset,
                in_chunk_offset: 0,
                child_length: child_length as u32,
            };
            sender.push(cmd.into())?;
        }
        Ok(())
    }

    /// Registers a blob's Merkle tree leaf hashes with the verifier via the delivery queue.
    fn register_blob(
        &self,
        key: u64,
        merkle_leaves: &[[u8; 32]],
        read_alignment: usize,
    ) -> Result<(), anyhow::Error> {
        let mut sender = self.sender.lock();
        if let Some(sender) = sender.as_mut() {
            let leaf_bytes: &[u8] = merkle_leaves.as_flattened();
            let mut payload = sender.reserve_payload(leaf_bytes.len())?;
            payload.data().copy_from_slice(leaf_bytes);
            let cmd = DeliveryCommand::RegisterBlob {
                key,
                offset: payload.offset(),
                length: leaf_bytes.len() as u32,
                read_alignment: read_alignment as u32,
            };
            payload.commit(cmd.into())?;
        }
        Ok(())
    }
}

/// Target metadata for a [`Buffer`] servicing a page request on a child file mapped over a
/// parent blob.
struct ChildTarget {
    /// Starting page-aligned logical byte offset in the child VMO for this request.
    child_target_offset: u64,
    /// Byte range in the parent blob corresponding to this child extent slice.
    parent_byte_range: Range<u64>,
}

/// Implementation of [`PageRequest`] for verifier deliveries that forwards
/// unverified chunks via the delivery queue.
pub struct Buffer {
    verifier: Arc<Verifier>,
    key: u64,
    read_range: Range<u64>,
    // Alignment boundary (matching the blob's `read_alignment`) at which data is flushed to the
    // verifier queue.
    read_alignment: usize,
    // Bytes committed into `self.data` so far via calls to [`DataBuffer::commit`].
    committed_len: usize,
    // Bytes delivered to the verifier queue so far.
    delivered_len: usize,
    data: Vec<u8>,
    child_target: Option<ChildTarget>,
}

impl Buffer {
    fn deliver_chunk(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        // TODO(https://fxbug.dev/530494057): Optimize buffer management / payload reservations.
        let chunk_data = &self.data[self.delivered_len..self.delivered_len + size];
        let offset = self.read_range.start + self.delivered_len as u64;

        let mut sender = self.verifier.sender.lock();
        if let Some(sender) = sender.as_mut() {
            let page_size = zx::system_get_page_size() as usize;
            let aligned_size = size.next_multiple_of(page_size);
            let mut payload = sender.reserve_payload(aligned_size).map_err(|e| {
                log::error!(e:?; "Verifier::deliver_chunk: reserve_payload failed");
                ChunkedArchiveError::IntegrityError
            })?;

            let payload_data = payload.data();
            payload_data.subslice_mut(0..size).copy_from_slice(chunk_data);
            payload_data.subslice_mut(size..aligned_size).fill(0);
            let cmd = if let Some(target) = &self.child_target {
                let chunk_end = offset + aligned_size as u64;
                let slice_start = std::cmp::max(offset, target.parent_byte_range.start);
                let slice_end = std::cmp::min(chunk_end, target.parent_byte_range.end);
                let in_chunk_offset = (slice_start - offset) as u32;
                let child_target_offset =
                    target.child_target_offset + (slice_start - target.parent_byte_range.start);
                let child_length = (slice_end.saturating_sub(slice_start)) as u32;
                DeliveryCommand::ChildData {
                    parent_offset: offset,
                    offset: payload.offset(),
                    length: aligned_size as u32,
                    child_key: self.key,
                    child_target_offset,
                    in_chunk_offset,
                    child_length,
                }
            } else {
                DeliveryCommand::Data {
                    key: self.key,
                    target_offset: offset,
                    length: aligned_size as u32,
                    offset: payload.offset(),
                }
            };
            payload.commit(cmd.into()).map_err(|_| ChunkedArchiveError::IntegrityError)?;
        }

        self.delivered_len += size;
        Ok(())
    }
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
        const VERIFICATION_FLUSH_THRESHOLD: usize = 96 * 1024;

        self.committed_len += size;

        if self.committed_len < self.data.len() {
            // Once at least `VERIFICATION_FLUSH_THRESHOLD` (96 KiB, rounded to a multiple of
            // `read_alignment`) has been committed, deliver complete `read_alignment` chunks so
            // verification overlaps with decompressing the remainder of the readahead window.
            let flush_threshold =
                (VERIFICATION_FLUSH_THRESHOLD / self.read_alignment).max(1) * self.read_alignment;
            if self.committed_len - self.delivered_len >= flush_threshold {
                let to_deliver = ((self.committed_len - self.delivered_len) / self.read_alignment)
                    * self.read_alignment;
                self.deliver_chunk(to_deliver)?;
            }
        } else if self.delivered_len < self.committed_len {
            // Deliver any remaining bytes once the entire prepared range has been committed.
            self.deliver_chunk(self.committed_len - self.delivered_len)?;
        }

        Ok(())
    }
}

impl PageRequest for Buffer {
    fn prepare(
        &mut self,
        read_range: Range<u64>,
        read_alignment: usize,
    ) -> Result<(), ChunkedArchiveError> {
        assert!(self.data.is_empty(), "prepare must only be called once");
        assert!(read_alignment > 0, "read_alignment must be non-zero");
        self.read_range = read_range;
        self.read_alignment = read_alignment;
        let len = (self.read_range.end - self.read_range.start) as usize;
        self.data = vec![0u8; len];
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mapping::{DELIVERY_DATA_COMMAND, TARGET_READ_AHEAD_SIZE};
    use test_case::test_case;

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
        request.prepare(0..8192, TARGET_READ_AHEAD_SIZE as usize).expect("prepare");

        // Fill first page (4096 bytes) with 0xAA
        let page1_data = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        request.commit(4096).expect("commit page 1");

        // Verify mut_ptr_slice advanced to second page
        assert_eq!(request.mut_ptr_slice().len(), 4096);
        // Not all data has been committed yet, so no command should be delivered yet.
        assert!(receiver.is_empty());

        // Fill second page (4096 bytes) with 0xBB
        let page2_data = vec![0xBBu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2_data);
        request.commit(4096).expect("commit page 2");

        // Verify remaining space is 0
        assert_eq!(request.mut_ptr_slice().len(), 0);

        // Check single consolidated message delivered on queue
        let msg = receiver.peek().expect("msg");
        assert_eq!(msg.opcode, DELIVERY_DATA_COMMAND);
        assert_eq!(msg.key, key);
        assert_eq!(msg.length, 8192);
        assert_eq!(msg.target_offset, 0);
        let mut buf = vec![0u8; 8192];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..4096], &page1_data[..]);
        assert_eq!(&buf[4096..], &page2_data[..]);
        msg.pop().expect("pop msg");
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

        let mut request = verifier.get_page_request(key, 0..5000);
        request.prepare(0..5000, TARGET_READ_AHEAD_SIZE as usize).expect("prepare");

        let data = vec![0xCCu8; 5000];

        // Commit first 4096 bytes
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&data[..4096]);
        request.commit(4096).expect("commit 4096 bytes");
        assert!(receiver.is_empty());

        // Commit remaining 904 bytes (completes the prepared range)
        request.mut_ptr_slice().subslice_mut(0..904).copy_from_slice(&data[4096..5000]);
        request.commit(904).expect("commit 904 bytes");

        // Check single message delivered, page-aligned to 8192
        let msg = receiver.peek().expect("msg");
        assert_eq!(msg.target_offset, 0);
        assert_eq!(msg.length, 8192);
        let mut buf = vec![0u8; 8192];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..5000], &data[..]);
        assert_eq!(&buf[5000..], &[0u8; 3192]);
        msg.pop().expect("pop msg");
    }

    #[test_case(32 * 1024, 128 * 1024; "zstd_32k_read_128k")]
    #[test_case(48 * 1024, 96 * 1024; "zstd_48k_read_96k")]
    #[test_case(152 * 1024, 152 * 1024; "zstd_152k_read_152k")]
    #[fuchsia::test]
    fn test_verifier_buffer_delivery_data_size_chunking(
        zstd_chunk_size: usize,
        read_alignment: usize,
    ) {
        let total_size = read_alignment * 2;
        let delivery_queue = zx::Vmo::create(total_size as u64 + 65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue));
        let key = 44u64;

        let mut request = verifier.get_page_request(key, 0..total_size as u64);
        request.prepare(0..total_size as u64, read_alignment).expect("prepare");

        let chunk = vec![0x55u8; zstd_chunk_size];
        let chunks_per_read = read_alignment / zstd_chunk_size;

        // Push `chunks_per_read - 1` chunks - less than `read_alignment`
        for _ in 0..(chunks_per_read - 1) {
            request.mut_ptr_slice().subslice_mut(0..zstd_chunk_size).copy_from_slice(&chunk);
            request.commit(zstd_chunk_size).expect("commit");
        }
        assert!(receiver.is_empty());

        // Push the final chunk of the first `read_alignment` window
        request.mut_ptr_slice().subslice_mut(0..zstd_chunk_size).copy_from_slice(&chunk);
        request.commit(zstd_chunk_size).expect("commit");

        // 1st `read_alignment` message delivered
        let msg1 = receiver.peek().expect("msg1");
        assert_eq!(msg1.target_offset, 0);
        assert_eq!(msg1.length, read_alignment as u32);
        msg1.pop().expect("pop msg1");

        // Push remaining `chunks_per_read` chunks (second `read_alignment` window)
        for _ in 0..chunks_per_read {
            request.mut_ptr_slice().subslice_mut(0..zstd_chunk_size).copy_from_slice(&chunk);
            request.commit(zstd_chunk_size).expect("commit");
        }

        // 2nd `read_alignment` message delivered
        let msg2 = receiver.peek().expect("msg2");
        assert_eq!(msg2.target_offset, read_alignment as u64);
        assert_eq!(msg2.length, read_alignment as u32);
        msg2.pop().expect("pop msg2");
    }

    #[fuchsia::test]
    #[should_panic(expected = "prepare must only be called once")]
    fn test_verifier_buffer_prepare_multiple_times_panics() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let verifier = Arc::new(Verifier::new(delivery_queue));

        let key = 46u64;
        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..4096, TARGET_READ_AHEAD_SIZE as usize).expect("first prepare");
        let _ = request.prepare(0..8192, TARGET_READ_AHEAD_SIZE as usize);
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
        request.prepare(0..8192, TARGET_READ_AHEAD_SIZE as usize).expect("prepare");

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

    #[fuchsia::test]
    fn test_verifier_register_blob() {
        let delivery_queue = zx::Vmo::create(65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue));
        let key = 42u64;
        let leaves = [[0xABu8; 32], [0xCDu8; 32]];

        verifier.register_blob(key, &leaves, 96 * 1024).expect("register_blob failed");

        let msg = receiver.peek().expect("peek msg");
        assert_eq!(msg.opcode, mapping::DELIVERY_REGISTER_BLOB_COMMAND);
        assert_eq!(msg.key, key);
        assert_eq!(msg.length, 64);
        assert_eq!(msg.read_alignment, 96 * 1024);
        let mut buf = vec![0u8; 64];
        msg.payload_slice(msg.offset, msg.length).copy_to_slice(&mut buf);
        assert_eq!(&buf[..32], &[0xABu8; 32]);
        assert_eq!(&buf[32..], &[0xCDu8; 32]);
        msg.pop().expect("pop msg failed");
    }

    #[fuchsia::test]
    fn test_verifier_buffer_96k_overlap() {
        let total_size = 128 * 1024;
        let chunk_size = 32 * 1024;
        let delivery_queue = zx::Vmo::create(total_size as u64 * 2 + 65536).unwrap();
        let mut receiver = vmo_fifo::Receiver::<RawDeliveryCommand>::new(
            delivery_queue.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            PENDING_DELIVERY_COMMANDS_CAPACITY,
        )
        .unwrap();

        let verifier = Arc::new(Verifier::new(delivery_queue));
        let key = 45u64;

        let mut request = verifier.get_page_request(key, 0..total_size as u64);
        request.prepare(0..total_size as u64, chunk_size).expect("prepare");

        let chunk = vec![0x55u8; chunk_size];

        // First two 32 KiB chunks (64 KiB total) should not flush yet.
        for _ in 0..2 {
            request.mut_ptr_slice().subslice_mut(0..chunk_size).copy_from_slice(&chunk);
            request.commit(chunk_size).expect("commit");
        }
        assert!(receiver.is_empty());

        // Third 32 KiB chunk reaches 96 KiB and flushes 96 KiB so verification overlaps with
        // decompressing the final 32 KiB.
        request.mut_ptr_slice().subslice_mut(0..chunk_size).copy_from_slice(&chunk);
        request.commit(chunk_size).expect("commit");

        let msg1 = receiver.peek().expect("msg1");
        assert_eq!(msg1.target_offset, 0);
        assert_eq!(msg1.length, 96 * 1024);
        msg1.pop().expect("pop msg1");

        // Fourth 32 KiB chunk completes the 128 KiB window and flushes the remaining 32 KiB.
        request.mut_ptr_slice().subslice_mut(0..chunk_size).copy_from_slice(&chunk);
        request.commit(chunk_size).expect("commit");

        let msg2 = receiver.peek().expect("msg2");
        assert_eq!(msg2.target_offset, 96 * 1024);
        assert_eq!(msg2.length, 32 * 1024);
        msg2.pop().expect("pop msg2");
    }
}
