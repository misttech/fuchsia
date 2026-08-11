// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::DataBuffer;
use delivery_blob::compression::ChunkedArchiveError;
use fuchsia_sync::Mutex;
use std::collections::HashMap;
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

    /// Returns a [`DataBuffer`] implementation for delivering page-in data.
    pub fn get_buffer(self: &Arc<Self>, key: u64, offset: u64, len: usize) -> Buffer {
        let page_size = zx::system_get_page_size() as usize;
        let page_aligned_len = (len + page_size - 1) & !(page_size - 1);
        let transfer_vmo = zx::Vmo::create(page_aligned_len as u64).expect("create transfer vmo");
        let vaddr = fuchsia_runtime::vmar_root_self()
            .map(
                0,
                &transfer_vmo,
                0,
                page_aligned_len,
                zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE,
            )
            .expect("map transfer vmo");

        Buffer {
            verifier: Arc::clone(self),
            key,
            offset,
            len,
            committed_len: 0,
            page_aligned_len,
            transfer_vmo,
            vaddr,
        }
    }
}

/// Implementation of [`DataBuffer`] for verifier deliveries that supplies
/// pages to the kernel.
pub struct Buffer {
    verifier: Arc<Verifier>,
    key: u64,
    offset: u64,
    len: usize,
    committed_len: usize,
    page_aligned_len: usize,
    transfer_vmo: zx::Vmo,
    vaddr: usize,
}

impl DataBuffer for Buffer {
    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        let remaining = self.len.saturating_sub(self.committed_len);
        // SAFETY: `self.vaddr` is mapped for `self.page_aligned_len` bytes,
        // and `self.committed_len + remaining` is bounded by
        // `self.len <= self.page_aligned_len`.
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
            pager
                .supply_pages(
                    target_vmo,
                    self.offset..self.offset + size as u64,
                    &self.transfer_vmo,
                    self.committed_len as u64,
                )
                .map_err(|_| ChunkedArchiveError::IntegrityError)?;
        }
        self.offset += size as u64;
        self.committed_len += size;
        Ok(())
    }
}

impl Drop for Buffer {
    fn drop(&mut self) {
        // SAFETY: `self.vaddr` was mapped in `get_buffer` with
        // `self.page_aligned_len` bytes in the root VMAR.
        unsafe {
            let _ = fuchsia_runtime::vmar_root_self().unmap(self.vaddr, self.page_aligned_len);
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

        let mut buffer = verifier.get_buffer(key, 0, 8192);

        // Fill first page (4096 bytes) with 0xAA
        let page1_data = vec![0xAAu8; 4096];
        buffer.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1_data);
        buffer.commit(4096).expect("commit page 1");

        // Verify mut_ptr_slice advanced to second page
        assert_eq!(buffer.mut_ptr_slice().len(), 4096);

        // Fill second page (4096 bytes) with 0xBB
        let page2_data = vec![0xBBu8; 4096];
        buffer.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2_data);
        buffer.commit(4096).expect("commit page 2");

        // Verify remaining space is 0
        assert_eq!(buffer.mut_ptr_slice().len(), 0);

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

        let mut buffer = verifier.get_buffer(key, 0, 5000);

        let data = vec![0xCCu8; 5000];

        // Commit page 1 (4096 bytes)
        buffer.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&data[..4096]);
        buffer.commit(4096).expect("commit 4096 bytes");

        // Commit page 2 (rounded up to 4096 bytes: 904 bytes payload + padding 0s)
        buffer.mut_ptr_slice().subslice_mut(0..904).copy_from_slice(&data[4096..5000]);
        buffer.commit(4096).expect("commit 4096 bytes");

        // Verify data in paged_vmo
        let mut read_buf = vec![0u8; 5000];
        paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo");
        assert_eq!(&read_buf[..], &data[..]);
    }
}
