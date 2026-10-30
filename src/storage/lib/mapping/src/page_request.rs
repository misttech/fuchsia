// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use std::fmt::{Debug, Formatter};
use std::ops::Range;
use std::sync::Arc;
use storage_ptr_slice::MutPtrByteSlice;

/// Deferred batch VMAR pre-commit operation for a prepared [`PageRequest`] buffer.
///
/// Running [`PagePrecommit::run`] after asynchronous block reads have been submitted overlaps
/// physical page allocation and writable PTE installation (`zx::VmarOp::COMMIT`) with storage DMA,
/// avoiding synchronous per-page write faults when populating the destination buffer.
#[derive(Default)]
pub struct PagePrecommit {
    vaddr: usize,
    len: usize,
    /// Retains ownership of the underlying VMAR mapping (e.g. `Arc<SyncSender>`) so the
    /// virtual address range `[vaddr .. vaddr + len)` remains valid during [`PagePrecommit::run`]
    /// even though the [`PageRequest`] itself has been moved into an asynchronous I/O completion
    /// callback.
    _keep_alive: Option<Arc<dyn Send + Sync>>,
}

impl Debug for PagePrecommit {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PagePrecommit").field("vaddr", &self.vaddr).field("len", &self.len).finish()
    }
}

impl PagePrecommit {
    /// Creates a [`PagePrecommit`] that will batch-commit `[vaddr .. vaddr + len)` in the process
    /// root VMAR while holding `keep_alive` so the underlying VMAR mapping remains valid.
    pub fn new(vaddr: usize, len: usize, keep_alive: Arc<dyn Send + Sync>) -> Self {
        Self { vaddr, len, _keep_alive: Some(keep_alive) }
    }

    /// Returns `true` if there is no virtual address range to pre-commit.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Commits physical pages and installs writable page table entries for `[vaddr .. vaddr + len)`
    /// in a single `zx::VmarOp::COMMIT` syscall.
    pub fn run(self) {
        if self.len > 0 {
            let _ = fuchsia_runtime::vmar_root_self().op_range(
                zx::VmarOp::COMMIT,
                self.vaddr,
                self.len,
            );
        }
    }
}

/// Trait for servicing a page request.
pub trait PageRequest: DataBuffer {
    /// Prepares the target buffer for reading `read_range`.
    ///
    /// `read_alignment` specifies the alignment boundary used when delivering chunks for Merkle
    /// verification (ignored by direct pager implementations that supply pages without a verifier).
    ///
    /// Must be called at most once prior to accessing the buffer slice or committing data.
    fn prepare(
        &mut self,
        read_range: Range<u64>,
        read_alignment: usize,
    ) -> Result<(), ChunkedArchiveError>;

    /// Returns a deferred [`PagePrecommit`] handle that batch-commits and maps the prepared
    /// destination pages in the root VMAR while asynchronous block I/O is in flight.
    ///
    /// Must be called after [`PageRequest::prepare`] once the destination buffer address and
    /// length have been established.
    fn precommit(&self) -> PagePrecommit {
        PagePrecommit::default()
    }
}

/// A no-op [`PageRequest`] for intermediate mapper sessions that do not run a kernel pager thread.
pub struct NullPageRequest;

impl DataBuffer for NullPageRequest {
    fn range(&self) -> Range<u64> {
        0..0
    }

    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        MutPtrByteSlice::from(&mut [][..])
    }

    fn commit(&mut self, _size: usize) -> Result<(), ChunkedArchiveError> {
        Ok(())
    }
}

impl PageRequest for NullPageRequest {
    fn prepare(
        &mut self,
        _read_range: Range<u64>,
        _read_alignment: usize,
    ) -> Result<(), ChunkedArchiveError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct ScopedVmarMapping {
        vaddr: usize,
        len: usize,
    }

    impl Drop for ScopedVmarMapping {
        fn drop(&mut self) {
            // SAFETY: Unmaps the VMAR region mapped for this test only after all `Arc` owners
            // (including `PagePrecommit`) have been dropped.
            unsafe {
                let _ = fuchsia_runtime::vmar_root_self().unmap(self.vaddr, self.len);
            }
        }
    }

    #[test]
    fn test_null_page_request() {
        let mut req = NullPageRequest;
        assert_eq!(req.range(), 0..0);
        assert!(req.prepare(0..4096, 128 * 1024).is_ok());
        assert!(req.precommit().is_empty());
        assert_eq!(req.mut_ptr_slice().len(), 0);
        assert!(req.commit(0).is_ok());
    }

    #[test]
    fn test_page_precommit_default_is_noop() {
        let precommit = PagePrecommit::default();
        assert!(precommit.is_empty());
        assert!(format!("{precommit:?}").contains("PagePrecommit"));
        precommit.run();
    }

    #[test]
    fn test_page_precommit_commits_pages_and_keeps_mapping_alive() {
        let page_size = zx::system_get_page_size() as usize;
        let len = 2 * page_size;
        let vmo = zx::Vmo::create(len as u64).expect("Vmo::create failed");
        assert_eq!(vmo.info().expect("vmo info").committed_bytes, 0);

        let vaddr = fuchsia_runtime::vmar_root_self()
            .map(0, &vmo, 0, len, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
            .expect("vmar map failed");
        let mapping = Arc::new(ScopedVmarMapping { vaddr, len });

        let precommit = PagePrecommit::new(vaddr, len, mapping.clone());
        assert!(!precommit.is_empty());

        // Drop our local `Arc` so only `precommit` keeps the VMAR mapping alive.
        drop(mapping);
        assert_eq!(vmo.info().expect("vmo info").committed_bytes, 0);

        precommit.run();
        assert_eq!(vmo.info().expect("vmo info").committed_bytes, len as u64);
    }
}
