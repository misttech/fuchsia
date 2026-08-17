// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use fuchsia_sync::Mutex;
use mapping::PageRequest;
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;

/// Manages data verification and page supply via the delivery queue VMO.
pub struct Verifier {
    _delivery_queue: zx::Vmo,
    pager: Mutex<Option<Arc<zx::Pager>>>,
    vmos: Mutex<HashMap<u64, zx::Vmo>>,
}

impl Verifier {
    pub fn new(delivery_queue: zx::Vmo) -> Self {
        Self {
            _delivery_queue: delivery_queue,
            pager: Mutex::new(None),
            vmos: Mutex::new(HashMap::new()),
        }
    }

    pub fn new_with_pager(delivery_queue: zx::Vmo, pager: Arc<zx::Pager>) -> Self {
        Self {
            _delivery_queue: delivery_queue,
            pager: Mutex::new(Some(pager)),
            vmos: Mutex::new(HashMap::new()),
        }
    }

    pub fn set_pager(&self, pager: Arc<zx::Pager>) {
        *self.pager.lock() = Some(pager);
    }

    pub fn register_vmo(&self, key: u64, vmo: zx::Vmo) {
        self.vmos.lock().insert(key, vmo);
    }

    pub fn unregister_vmo(&self, key: u64) {
        self.vmos.lock().remove(&key);
    }

    /// Returns a [`PageRequest`] implementation for delivering page-in data.
    pub fn get_page_request(self: &Arc<Self>, key: u64, original_range: Range<u64>) -> Buffer {
        Buffer {
            verifier: Arc::clone(self),
            key,
            original_range: original_range.clone(),
            read_range: original_range,
            committed_len: 0,
            transfer_vmo: None,
            vaddr: 0,
        }
    }
}

/// Implementation of [`PageRequest`] for verifier deliveries that supplies
/// pages to the kernel.
pub struct Buffer {
    verifier: Arc<Verifier>,
    key: u64,
    original_range: Range<u64>,
    read_range: Range<u64>,
    committed_len: usize,
    transfer_vmo: Option<zx::Vmo>,
    vaddr: usize,
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
        let total_len = (self.read_range.end - self.read_range.start) as usize;
        let remaining = total_len.saturating_sub(self.committed_len);
        if remaining > 0 {
            assert!(self.vaddr != 0, "prepare must be called before accessing mut_ptr_slice");
        }
        // SAFETY: `self.vaddr` is mapped for `total_len` bytes by `prepare()`,
        // and `self.committed_len + remaining` is bounded by `total_len`.
        unsafe {
            MutPtrByteSlice::from(std::slice::from_raw_parts_mut(
                (self.vaddr + self.committed_len) as *mut u8,
                remaining,
            ))
        }
    }

    fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        // TODO(https://fxbug.dev/530494057): Add support for verification later.
        let pager = self.verifier.pager.lock().as_ref().map(Arc::clone);
        let vmos_guard = self.verifier.vmos.lock();
        if let (Some(pager), Some(target_vmo)) = (pager, vmos_guard.get(&self.key)) {
            let offset = self.read_range.start + self.committed_len as u64;
            pager
                .supply_pages(
                    target_vmo,
                    offset..offset + size as u64,
                    self.transfer_vmo.as_ref().unwrap(),
                    self.committed_len as u64,
                )
                .map_err(|_| ChunkedArchiveError::IntegrityError)?;
        }
        self.committed_len += size;
        Ok(())
    }
}

impl PageRequest for Buffer {
    fn prepare(&mut self, read_range: Range<u64>) -> Result<(), ChunkedArchiveError> {
        assert_eq!(self.vaddr, 0, "prepare must only be called once");
        self.read_range = read_range;
        let len = (self.read_range.end - self.read_range.start) as usize;
        let transfer_vmo =
            zx::Vmo::create(len as u64).map_err(|_| ChunkedArchiveError::IntegrityError)?;
        let vaddr = fuchsia_runtime::vmar_root_self()
            .map(0, &transfer_vmo, 0, len, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
            .map_err(|_| ChunkedArchiveError::IntegrityError)?;

        self.transfer_vmo = Some(transfer_vmo);
        self.vaddr = vaddr;
        Ok(())
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        let uncommitted_start = std::cmp::max(
            self.original_range.start,
            self.read_range.start + self.committed_len as u64,
        );
        if uncommitted_start < self.original_range.end {
            let pager = self.verifier.pager.lock().as_ref().map(Arc::clone);
            let vmos_guard = self.verifier.vmos.lock();
            if let (Some(pager), Some(target_vmo)) = (pager, vmos_guard.get(&self.key)) {
                let _ = pager.op_range(
                    zx::PagerOp::Fail(zx::Status::IO),
                    target_vmo,
                    uncommitted_start..self.original_range.end,
                );
            }
        }
        if self.vaddr != 0 {
            let len = (self.read_range.end - self.read_range.start) as usize;
            // SAFETY: `self.vaddr` was mapped in `prepare` with `len` bytes in the root VMAR.
            unsafe {
                let _ = fuchsia_runtime::vmar_root_self().unmap(self.vaddr, len);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[fuchsia::test]
    fn test_verifier_buffer_incremental_commit() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let verifier = Arc::new(Verifier::new_with_pager(delivery_queue, pager.clone()));

        let key = 42u64;
        let port = zx::Port::create();
        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, 8192).unwrap();

        verifier.register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());

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

        // Verify data in paged_vmo
        let mut read_buf = vec![0u8; 8192];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo");
        assert_eq!(&read_buf[..4096], &page1_data[..]);
        assert_eq!(&read_buf[4096..8192], &page2_data[..]);
    }

    #[fuchsia::test]
    fn test_verifier_buffer_page_aligned_commits() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let verifier = Arc::new(Verifier::new_with_pager(delivery_queue, pager.clone()));

        let key = 43u64;
        let port = zx::Port::create();
        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, 8192).unwrap();

        verifier.register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());

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

        // Verify data in paged_vmo
        let mut read_buf = vec![0u8; 8192];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo");
        assert_eq!(&read_buf[..], &data[..]);
    }

    #[fuchsia::test]
    fn test_verifier_buffer_drop_fails_uncommitted_range() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let verifier = Arc::new(Verifier::new_with_pager(delivery_queue, pager.clone()));

        let key = 44u64;
        let port = zx::Port::create();
        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, 8192).unwrap();
        let paged_vmo_clone = paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        verifier.register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());

        // Spawn a background thread to attempt reading from the paged VMO, which blocks
        // on page fault.
        let reader_thread = std::thread::spawn(move || {
            let mut read_buf = vec![0u8; 8192];
            paged_vmo_clone.read(&mut read_buf, 0)
        });

        // Wait for page request packet to arrive on port.
        let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("port wait");
        assert_eq!(packet.key(), key);

        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");
        // Dropping uncommitted buffer triggers pager.op_range(Fail) on 0..8192.
        drop(request);

        // The background reader should unblock and return an IO error.
        assert_eq!(reader_thread.join().unwrap(), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    fn test_verifier_buffer_drop_fails_partial_uncommitted_range() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let verifier = Arc::new(Verifier::new_with_pager(delivery_queue, pager.clone()));

        let key = 45u64;
        let port = zx::Port::create();
        let paged_vmo = pager.create_vmo(zx::VmoOptions::empty(), &port, key, 8192).unwrap();
        let paged_vmo_clone = paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        verifier.register_vmo(key, paged_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap());

        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");

        // Commit first page (4096 bytes)
        let page1_data = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        request.commit(4096).expect("commit page 1");

        // First page should be readable immediately.
        let mut read_buf = vec![0u8; 4096];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo page 1");
        assert_eq!(&read_buf[..], &page1_data[..]);

        // Attempt reading page 2 in a background thread; it blocks on page fault.
        let reader_thread = std::thread::spawn(move || {
            let mut read_buf = vec![0u8; 4096];
            paged_vmo_clone.read(&mut read_buf, 4096)
        });

        // Wait for page request packet on port.
        let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("port wait");
        assert_eq!(packet.key(), key);

        // Drop buffer before committing page 2.
        // Should fail 4096..8192.
        drop(request);

        // Background reader on page 2 should unblock and return an IO error.
        assert_eq!(reader_thread.join().unwrap(), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    #[should_panic(expected = "prepare must only be called once")]
    fn test_verifier_buffer_prepare_multiple_times_panics() {
        let delivery_queue = zx::Vmo::create(4096).unwrap();
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let verifier = Arc::new(Verifier::new_with_pager(delivery_queue, pager.clone()));

        let key = 46u64;
        let mut request = verifier.get_page_request(key, 0..8192);
        request.prepare(0..4096).expect("first prepare");
        let _ = request.prepare(0..8192);
    }
}
