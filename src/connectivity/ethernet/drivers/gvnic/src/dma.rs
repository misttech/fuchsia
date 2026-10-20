// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use dma_buffer::{ContiguousBuffer, PagedBuffer};
use mapped_vmo::Mapping;
use std::mem::size_of;
use zerocopy::byteorder::network_endian::{U32, U64};
use zx::{Bti, Status, Vmar, system_get_page_size};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PageListId(pub(crate) u32);

impl From<PageListId> for U32 {
    fn from(PageListId(id): PageListId) -> Self {
        Self::new(id)
    }
}

pub(crate) struct PageList {
    id: PageListId,
    length: u32,
    pages: PagedBuffer,
    paddr_table: ContiguousBuffer,
}

impl PageList {
    pub(crate) fn new(vmar: &Vmar, bti: &Bti, id: PageListId, length: u32) -> Result<Self, Status> {
        let page_size = system_get_page_size() as usize;
        let len_usize = usize::try_from(length).map_err(|_| Status::INVALID_ARGS)?;
        let pages_bytes = len_usize.checked_mul(page_size).ok_or(Status::INVALID_ARGS)?;
        let pages = PagedBuffer::new_with_vmar(bti, vmar, pages_bytes)?;

        let table_bytes = len_usize.checked_mul(size_of::<U64>()).ok_or(Status::INVALID_ARGS)?;
        let paddr_table = ContiguousBuffer::new_with_vmar(bti, vmar, table_bytes)?;
        for (i, &paddr) in pages.phys().iter().enumerate() {
            paddr_table.write_element(i, &U64::new(paddr as u64));
        }

        Ok(Self { id, length, pages, paddr_table })
    }

    /// Unpins the page list's physical pages and physical address table, consuming the page list.
    ///
    /// # Safety
    ///
    /// The caller must ensure the hardware is no longer accessing this page list (e.g. after
    /// resetting the device).
    pub(crate) unsafe fn unpin(self) {
        // SAFETY: The caller guarantees hardware DMA to this page list and its address table has
        // ceased.
        unsafe {
            let _ = self.pages.unpin();
            let _ = self.paddr_table.unpin();
        }
    }

    pub(crate) fn id(&self) -> PageListId {
        self.id
    }

    pub(crate) fn length(&self) -> u32 {
        self.length
    }

    pub(crate) fn paddr_table_phys(&self) -> u64 {
        self.paddr_table.phys() as u64
    }

    pub(crate) fn copy_to(
        &self,
        src_offset: usize,
        dst: &mut Mapping,
        dst_offset: usize,
        len: usize,
    ) -> Result<(), Status> {
        if src_offset.checked_add(len).is_none_or(|end| end > self.pages.size())
            || dst_offset.checked_add(len).is_none_or(|end| end > dst.len())
        {
            return Err(Status::OUT_OF_RANGE);
        }
        if dst.copy_from_at(dst_offset, &self.pages, src_offset, len) != len {
            return Err(Status::OUT_OF_RANGE);
        }
        Ok(())
    }

    pub(crate) fn copy_from(
        &mut self,
        dst_offset: usize,
        src: &Mapping,
        src_offset: usize,
        len: usize,
    ) -> Result<(), Status> {
        if dst_offset.checked_add(len).is_none_or(|end| end > self.pages.size())
            || src_offset.checked_add(len).is_none_or(|end| end > src.len())
        {
            return Err(Status::OUT_OF_RANGE);
        }
        if self.pages.copy_from_at(dst_offset, src, src_offset, len) != len {
            return Err(Status::OUT_OF_RANGE);
        }
        Ok(())
    }
}
