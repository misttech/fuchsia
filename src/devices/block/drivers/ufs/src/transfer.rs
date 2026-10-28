// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physical Region Description Table (PRDT) entries and the UTP Command Descriptor (UCD) layout
//! (UFSHCI 3.0/4.0 section 6.1.2).
//!
//! UFSHCI host-memory structures are little-endian; UPIUs inside the UCD are big-endian.

use crate::UfsError;
use crate::constants::{
    DWORD_ALIGN_MASK, INLINE_DATA_SIZE, MAX_PRDT_ENTRIES, MAX_PRDT_ENTRY_BYTES, PRDT_DBC_MASK,
    UTP_COMMAND_DESCRIPTOR_SIZE,
};
use static_assertions::const_assert_eq;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// PRDT entry (16 bytes, UFSHCI 3.0/4.0 section 6.1.2).
#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable,
)]
#[repr(C)]
pub struct PrdtEntry {
    /// Data base address, bits 31:0 (`DBA`, bits 1:0 must be 0).
    pub base_addr: u32,
    /// Data base address, bits 63:32 (`DBAU`).
    pub base_addr_upper: u32,
    pub reserved: u32,
    /// 0-based data byte count (`DBC`, bits 17:0, bits 1:0 must be 11b).
    pub data_byte_count: u32,
}

const_assert_eq!(size_of::<PrdtEntry>(), 16);

impl PrdtEntry {
    /// Returns the 64-bit physical address represented by this entry.
    pub fn dma_address(&self) -> u64 {
        u64::from(self.base_addr) | (u64::from(self.base_addr_upper) << 32)
    }

    /// Returns the transfer length in bytes represented by this entry (`DBC + 1`).
    pub fn byte_count(&self) -> u32 {
        (self.data_byte_count & PRDT_DBC_MASK) + 1
    }

    /// Sets the physical address and byte count.
    ///
    /// # Errors
    ///
    /// Returns `InvalidParameter` unless `addr` is dword-aligned and `byte_count` is a multiple
    /// of 4 in `4..=262144`.
    pub fn set_dma_address(&mut self, addr: u64, byte_count: u32) -> Result<(), UfsError> {
        if addr & DWORD_ALIGN_MASK != 0
            || u64::from(byte_count) & DWORD_ALIGN_MASK != 0
            || byte_count == 0
            || byte_count > MAX_PRDT_ENTRY_BYTES
        {
            return Err(UfsError::InvalidParameter);
        }
        self.base_addr = addr as u32;
        self.base_addr_upper = (addr >> 32) as u32;
        self.reserved = 0;
        self.data_byte_count = byte_count - 1;
        Ok(())
    }
}

/// Fills `prdt_table` with scatter-gather entries for a pinned buffer and returns the entry count.
///
/// Adjacent pages with physically contiguous addresses are coalesced into a single entry up to
/// [`MAX_PRDT_ENTRY_BYTES`] (256 KiB). `buffer_phys[0]` must be the physical address of the page
/// that `vmo_offset` falls in, and the following entries the subsequent pages, as returned by
/// `zx_bti_pin` for the page-aligned range that covers `[vmo_offset, vmo_offset + data_length)`.
///
/// # Errors
///
/// Returns `InvalidParameter` if the table or page list is too short, or an entry violates
/// [`PrdtEntry::set_dma_address`] alignment rules.
pub fn fill_prdt(
    prdt_table: &mut [PrdtEntry],
    buffer_phys: &[zx::sys::zx_paddr_t],
    vmo_offset: u64,
    data_length: u32,
) -> Result<usize, UfsError> {
    fill_prdt_with_page_size(
        prdt_table,
        buffer_phys,
        vmo_offset,
        data_length,
        u64::from(zx::system_get_page_size()),
    )
}

/// [`fill_prdt`] with an explicit `page_size` so tests can exercise coalescing deterministically.
///
/// Also returns `InvalidParameter` if `page_size` is not a power of two or exceeds
/// [`MAX_PRDT_ENTRY_BYTES`].
fn fill_prdt_with_page_size(
    prdt_table: &mut [PrdtEntry],
    buffer_phys: &[zx::sys::zx_paddr_t],
    vmo_offset: u64,
    mut data_length: u32,
    page_size: u64,
) -> Result<usize, UfsError> {
    if !page_size.is_power_of_two() || page_size > MAX_PRDT_ENTRY_BYTES as u64 {
        return Err(UfsError::InvalidParameter);
    }
    let mut offset_in_page = vmo_offset & (page_size - 1);
    let mut entry_count: usize = 0;
    let mut page_index: usize = 0;

    while data_length > 0 {
        let Some(&page) = buffer_phys.get(page_index) else {
            return Err(UfsError::InvalidParameter);
        };
        let addr = page as u64 + offset_in_page;
        let byte_count = data_length.min((page_size - offset_in_page) as u32);

        // After the first page (`entry_count > 0`), `offset_in_page` has been reset to 0, so every
        // subsequent page starts on a page boundary and can coalesce with the preceding entry when
        // physically contiguous.
        if entry_count > 0 {
            let prev = &mut prdt_table[entry_count - 1];
            let prev_addr = prev.dma_address();
            let prev_len = prev.byte_count();
            if prev_addr + u64::from(prev_len) == addr
                && prev_len + byte_count <= MAX_PRDT_ENTRY_BYTES
            {
                prev.set_dma_address(prev_addr, prev_len + byte_count)?;
                data_length -= byte_count;
                page_index += 1;
                continue;
            }
        }

        let Some(entry) = prdt_table.get_mut(entry_count) else {
            return Err(UfsError::InvalidParameter);
        };
        entry.set_dma_address(addr, byte_count)?;

        data_length -= byte_count;
        entry_count += 1;
        page_index += 1;
        offset_in_page = 0;
    }
    Ok(entry_count)
}

/// Byte layout of a UCD: request UPIU at offset 0, response UPIU right after it, then the PRDT;
/// the last [`INLINE_DATA_SIZE`] bytes hold small inline write payloads.
///
/// Offsets are multiples of 8 (UFSHCI requires dword alignment; 8 keeps 64-bit accesses aligned)
/// and request + response + a full PRDT + the inline region always fit in
/// [`UTP_COMMAND_DESCRIPTOR_SIZE`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CommandDescriptorLayout {
    request_length: u16,
    response_length: u16,
}

impl CommandDescriptorLayout {
    /// SCSI COMMAND UPIU (32 B) + RESPONSE UPIU (56 B): response at 32, PRDT at 88.
    pub const SCSI_COMMAND: Self = Self::new(
        std::mem::size_of::<crate::upiu::CommandUpiu>(),
        std::mem::size_of::<crate::upiu::ResponseUpiu>(),
    );
    /// QUERY REQUEST UPIU (288 B) + QUERY RESPONSE UPIU (288 B).
    pub const QUERY: Self = Self::new(
        std::mem::size_of::<crate::query::QueryUpiu>(),
        std::mem::size_of::<crate::query::QueryUpiu>(),
    );
    /// NOP OUT UPIU (32 B) + NOP IN UPIU (32 B).
    pub const NOP: Self = Self::new(
        std::mem::size_of::<crate::upiu::NopOutUpiu>(),
        std::mem::size_of::<crate::upiu::NopInUpiu>(),
    );

    /// Creates a layout.
    ///
    /// # Panics
    ///
    /// Panics (at compile time for `const` uses) if either length is not a multiple of 8 or the
    /// request, response, a full PRDT and the inline region do not fit in a UCD.
    const fn new(request_length: usize, response_length: usize) -> Self {
        assert!(
            request_length % 8 == 0 && response_length % 8 == 0,
            "UPIU lengths must be multiples of 8 bytes"
        );
        assert!(
            request_length
                + response_length
                + MAX_PRDT_ENTRIES * std::mem::size_of::<PrdtEntry>()
                + INLINE_DATA_SIZE
                <= UTP_COMMAND_DESCRIPTOR_SIZE,
            "request, response, PRDT and inline data exceed the UCD size"
        );
        Self { request_length: request_length as u16, response_length: response_length as u16 }
    }

    /// Length of the request UPIU in bytes.
    pub const fn request_length(&self) -> usize {
        self.request_length as usize
    }

    /// Byte offset of the response UPIU.
    pub const fn response_offset(&self) -> usize {
        self.request_length as usize
    }

    /// Length of the response UPIU in bytes.
    pub const fn response_length(&self) -> usize {
        self.response_length as usize
    }

    /// Byte offset of the PRDT.
    pub const fn prdt_offset(&self) -> usize {
        self.response_offset() + self.response_length()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PAGE_SIZE: u64 = 4096;

    #[fuchsia::test]
    fn test_prdt_entry() {
        let entry = PrdtEntry { data_byte_count: 0x0FFF, ..Default::default() };
        assert_eq!(entry.byte_count(), 4096);
    }

    #[fuchsia::test]
    fn test_prdt_entry_limits() {
        let mut prdt = PrdtEntry::default();
        assert_eq!(prdt.set_dma_address(0x1234_5678_9ABC_DEF0, 4096), Ok(()));
        assert_eq!(prdt.base_addr, 0x9ABC_DEF0);
        assert_eq!(prdt.base_addr_upper, 0x1234_5678);
        assert_eq!(prdt.dma_address(), 0x1234_5678_9ABC_DEF0);
        assert_eq!(prdt.byte_count(), 4096);
        assert_eq!(prdt.set_dma_address(0x1000, 256 * 1024), Ok(()));

        assert_eq!(prdt.set_dma_address(0x1002, 4096), Err(UfsError::InvalidParameter));
        assert_eq!(prdt.set_dma_address(0x1000, 4094), Err(UfsError::InvalidParameter));
        assert_eq!(prdt.set_dma_address(0x1000, 0), Err(UfsError::InvalidParameter));
        assert_eq!(prdt.set_dma_address(0x1000, 256 * 1024 + 4), Err(UfsError::InvalidParameter));
    }

    #[fuchsia::test]
    fn test_command_descriptor_layouts() {
        let scsi = CommandDescriptorLayout::SCSI_COMMAND;
        assert_eq!(
            (scsi.response_offset(), scsi.response_length(), scsi.prdt_offset()),
            (32, 56, 88)
        );
        let query = CommandDescriptorLayout::QUERY;
        assert_eq!(
            (query.response_offset(), query.response_length(), query.prdt_offset()),
            (288, 288, 576)
        );
        let nop = CommandDescriptorLayout::NOP;
        assert_eq!((nop.response_offset(), nop.response_length(), nop.prdt_offset()), (32, 32, 64));
    }

    #[fuchsia::test]
    fn test_fill_prdt() {
        let mut prdt_table = [PrdtEntry::default(); 256];
        let non_contig = [0x1000usize, 0x3000usize, 0x5000usize];

        // Non-contiguous pages: 10 KiB from offset 0 -> 4 KiB, 4 KiB, 2 KiB.
        let count =
            fill_prdt_with_page_size(&mut prdt_table, &non_contig, 0, 10240, PAGE_SIZE).unwrap();
        assert_eq!(count, 3);
        assert_eq!((prdt_table[0].base_addr, prdt_table[0].byte_count()), (0x1000, 4096));
        assert_eq!((prdt_table[1].base_addr, prdt_table[1].byte_count()), (0x3000, 4096));
        assert_eq!((prdt_table[2].base_addr, prdt_table[2].byte_count()), (0x5000, 2048));

        // 10 KiB from offset 512 across non-contiguous pages: 3584, 4096 and 2560 bytes.
        let count =
            fill_prdt_with_page_size(&mut prdt_table, &non_contig, 512, 10240, PAGE_SIZE).unwrap();
        assert_eq!(count, 3);
        assert_eq!((prdt_table[0].base_addr, prdt_table[0].byte_count()), (0x1200, 3584));
        assert_eq!((prdt_table[1].base_addr, prdt_table[1].byte_count()), (0x3000, 4096));
        assert_eq!((prdt_table[2].base_addr, prdt_table[2].byte_count()), (0x5000, 2560));

        // Physically contiguous pages coalesce into a single PRDT entry up to 256 KiB.
        let contig = [0x1000usize, 0x2000usize, 0x3000usize];
        let count =
            fill_prdt_with_page_size(&mut prdt_table, &contig, 0, 10240, PAGE_SIZE).unwrap();
        assert_eq!(count, 1);
        assert_eq!((prdt_table[0].base_addr, prdt_table[0].byte_count()), (0x1000, 10240));

        // Unaligned first page also coalesces with following contiguous pages because its end
        // (`0x1200 + 3584 = 0x2000`) meets the start of the next page.
        let count =
            fill_prdt_with_page_size(&mut prdt_table, &contig, 512, 10240, PAGE_SIZE).unwrap();
        assert_eq!(count, 1);
        assert_eq!((prdt_table[0].base_addr, prdt_table[0].byte_count()), (0x1200, 10240));

        // 70 contiguous 4 KiB pages (280 KiB) split into 256 KiB + 24 KiB (2 entries).
        let many_contig: Vec<usize> = (0..70).map(|i| 0x10_0000 + i * 4096).collect();
        let count =
            fill_prdt_with_page_size(&mut prdt_table, &many_contig, 0, 70 * 4096, PAGE_SIZE)
                .unwrap();
        assert_eq!(count, 2);
        assert_eq!((prdt_table[0].base_addr, prdt_table[0].byte_count()), (0x10_0000, 256 * 1024));
        assert_eq!((prdt_table[1].base_addr, prdt_table[1].byte_count()), (0x14_0000, 6 * 4096));
    }

    #[fuchsia::test]
    fn test_fill_prdt_uses_system_page_size() {
        let page_size = u64::from(zx::system_get_page_size());
        let pages: Vec<usize> = (0..3).map(|i| (0x10_0000 + i * 2 * page_size) as usize).collect();
        let data_length = (2 * page_size + 512) as u32;

        let mut expected = [PrdtEntry::default(); 4];
        let expected_count =
            fill_prdt_with_page_size(&mut expected, &pages, 256, data_length, page_size).unwrap();
        let mut actual = [PrdtEntry::default(); 4];
        let actual_count = fill_prdt(&mut actual, &pages, 256, data_length).unwrap();
        assert_eq!(actual_count, expected_count);
        assert_eq!(actual_count, 3);
        assert_eq!(actual, expected);
    }

    #[fuchsia::test]
    fn test_fill_prdt_rejects_invalid_input() {
        let mut prdt_table = [PrdtEntry::default(); 4];
        let buffer_phys = [0x1000usize, 0x3000usize];

        // Unaligned address, byte count not a multiple of 4, too few pages or table entries, and
        // a bad page size.
        let err = Err(UfsError::InvalidParameter);
        assert_eq!(fill_prdt_with_page_size(&mut prdt_table, &buffer_phys, 2, 512, PAGE_SIZE), err);
        assert_eq!(fill_prdt_with_page_size(&mut prdt_table, &buffer_phys, 0, 510, PAGE_SIZE), err);
        assert_eq!(
            fill_prdt_with_page_size(&mut prdt_table, &buffer_phys, 0, 3 * 4096, PAGE_SIZE),
            err
        );
        assert_eq!(
            fill_prdt_with_page_size(&mut prdt_table[..1], &buffer_phys, 0, 8192, PAGE_SIZE),
            err
        );
        assert_eq!(fill_prdt_with_page_size(&mut prdt_table, &buffer_phys, 0, 512, 3000), err);
        assert_eq!(fill_prdt(&mut prdt_table, &buffer_phys, 2, 512), err);
    }
}
