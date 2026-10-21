// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow, bail};
use mapped_vmo::Mapping;
use std::fmt::Write as _;
use std::mem::size_of;
use std::slice;
use zbi::zbi_format::{
    ZBI_CONTAINER_MAGIC, ZBI_FLAGS_CRC32, ZBI_FLAGS_STORAGE_COMPRESSED, ZBI_ITEM_MAGIC,
    ZBI_ITEM_NO_CRC32,
};
use zbi::{ZBI_ALIGNMENT_USIZE, ZbiFlags, ZbiHeader, ZbiType};
use zerocopy::IntoBytes as _;
use zx::{Name, VmarFlags, Vmo};
use zx_libc::sanitizer::Log;

const HEADER_SIZE: u64 = size_of::<ZbiHeader>() as u64;

/// Header and VMO payload offset of an item in a ZBI VMO.
#[derive(Debug, Clone, Copy)]
pub struct ZbiItem {
    /// The validated ZBI item header.
    pub header: ZbiHeader,
    /// Byte offset of the item's payload within the ZBI VMO.
    pub payload_offset: u64,
}

fn check_item_header(header: &ZbiHeader) -> Result<(), Error> {
    if header.magic != ZBI_ITEM_MAGIC {
        bail!("ZBI header has bad magic");
    }
    if !header.get_flags().contains(ZbiFlags::VERSION) {
        bail!("ZBI header has bad version");
    }
    // Per <lib/zbi-format/zbi.h>, when ZBI_FLAGS_CRC32 is not set, the crc32 field must contain
    // the sentinel value ZBI_ITEM_NO_CRC32.
    if !header.get_flags().contains(ZbiFlags::CRC32) && header.crc32 != ZBI_ITEM_NO_CRC32 {
        bail!("ZBI header without CRC32 flag must have ZBI_ITEM_NO_CRC32");
    }
    Ok(())
}

/// Iterator over items in a ZBI VMO that reads item headers via `zx_vmo_read`
/// without mapping the entire ZBI VMO into memory.
#[derive(Debug)]
pub struct ZbiItemIterator<'a> {
    zbi_vmo: &'a Vmo,
    offset: u64,
    container_end: u64,
    failed: bool,
}

impl<'a> ZbiItemIterator<'a> {
    /// Validates the ZBI container header in `zbi_vmo` and returns an iterator over its items
    /// using `zx_vmo_read` rather than mapping the VMO.
    pub fn new(zbi_vmo: &'a Vmo) -> Result<Self, Error> {
        let vmo_size = zbi_vmo.get_size()?;
        if vmo_size < HEADER_SIZE {
            bail!("ZBI VMO is too small for container header");
        }
        let header: ZbiHeader = zbi_vmo.read_to_object(0)?;
        if header.type_ != ZbiType::Container as u32 {
            bail!("ZBI container header has bad type: {:#x}", header.type_);
        }
        if header.extra != ZBI_CONTAINER_MAGIC {
            bail!("ZBI container header has bad magic");
        }
        if (header.flags & ZBI_FLAGS_CRC32) != 0 {
            bail!("ZBI container header must not have CRC32 flag");
        }
        if (header.length as usize) % ZBI_ALIGNMENT_USIZE != 0 {
            bail!("ZBI container length is not aligned");
        }
        check_item_header(&header)?;
        let container_end = HEADER_SIZE
            .checked_add(u64::from(header.length))
            .ok_or_else(|| anyhow!("ZBI container length overflow"))?;
        if container_end > vmo_size {
            bail!("ZBI container is truncated");
        }
        Ok(Self { zbi_vmo, offset: HEADER_SIZE, container_end, failed: false })
    }
}

impl Iterator for ZbiItemIterator<'_> {
    type Item = Result<ZbiItem, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.failed || self.offset == self.container_end {
            return None;
        }
        let step = (|| -> Result<ZbiItem, Error> {
            let payload_offset = self
                .offset
                .checked_add(HEADER_SIZE)
                .ok_or_else(|| anyhow!("ZBI item header offset overflow"))?;
            if payload_offset > self.container_end {
                bail!("ZBI item header is truncated");
            }
            let header: ZbiHeader = self.zbi_vmo.read_to_object(self.offset)?;
            check_item_header(&header)?;
            let payload_end = payload_offset
                .checked_add(u64::from(header.length))
                .ok_or_else(|| anyhow!("ZBI item length overflow"))?;
            if payload_end > self.container_end {
                bail!("ZBI item payload is truncated");
            }
            let next_offset = payload_end
                .checked_next_multiple_of(ZBI_ALIGNMENT_USIZE as u64)
                .ok_or_else(|| anyhow!("ZBI item alignment overflow"))?;
            if next_offset > self.container_end {
                bail!("ZBI item alignment padding is truncated");
            }
            self.offset = next_offset;
            Ok(ZbiItem { header, payload_offset })
        })();
        if step.is_err() {
            self.failed = true;
        }
        Some(step)
    }
}

/// Extracts the BOOTFS VMO from the ZBI VMO, mapping only the pages containing the
/// `STORAGE_BOOTFS` item payload and decompressing it if needed.
pub fn get_bootfs_vmo(zbi_vmo: &Vmo, check_crc: bool, log: &mut Log) -> Result<Vmo, Error> {
    let mut bootfs_item = None;
    for item in ZbiItemIterator::new(zbi_vmo)? {
        let item = item?;
        if item.header.type_ == ZbiType::StorageBootFs as u32 {
            bootfs_item = Some(item);
            break;
        }
    }
    let bootfs_item = bootfs_item.ok_or_else(|| anyhow!("StorageBootFs item not found in ZBI"))?;

    let payload_len = bootfs_item.header.length as usize;
    if payload_len == 0 {
        bail!("StorageBootFs item has empty payload");
    }
    let page_size = u64::from(zx::system_get_page_size());
    let map_offset = bootfs_item.payload_offset & !(page_size - 1);
    let offset_in_mapping = (bootfs_item.payload_offset - map_offset) as usize;
    let map_size = (offset_in_mapping as u64)
        .checked_add(payload_len as u64)
        .and_then(|end| end.checked_next_multiple_of(page_size))
        .ok_or_else(|| anyhow!("BOOTFS mapping size overflow"))?;
    let mut payload_mapping = Mapping::create_from_vmo_with_offset(
        zbi_vmo,
        map_offset,
        map_size as usize,
        VmarFlags::PERM_READ | VmarFlags::MAP_RANGE,
    )?;
    let (payload_ptr, payload_map_len) = payload_mapping.as_ptr_len();
    // SAFETY: `zbi_vmo` is not modified while `get_bootfs_vmo` is running, and `payload_mapping`
    // keeps `payload_ptr..payload_ptr + payload_map_len` mapped read-only for the duration of this
    // function.
    let payload = unsafe {
        &slice::from_raw_parts(payload_ptr.cast_const(), payload_map_len)
            [offset_in_mapping..offset_in_mapping + payload_len]
    };

    if check_crc && (bootfs_item.header.flags & ZBI_FLAGS_CRC32) != 0 {
        writeln!(log, "Checking BOOTFS item CRC32 {:#x}...", bootfs_item.header.crc32)?;
        let mut header_without_crc = bootfs_item.header;
        header_without_crc.crc32 = 0;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(header_without_crc.as_bytes());
        hasher.update(payload);
        if hasher.finalize() == bootfs_item.header.crc32 {
            writeln!(log, "BOOTFS payload matches item CRC32.")?;
        } else {
            writeln!(log, "*** BOOTFS payload DOES NOT MATCH item CRC32! ***")?;
            bail!("BOOTFS payload CRC32 mismatch");
        }
    }

    let is_compressed = (bootfs_item.header.flags & ZBI_FLAGS_STORAGE_COMPRESSED) != 0;
    let bootfs_vmo = if is_compressed {
        let uncompressed_size = bootfs_item.header.extra as usize;
        if uncompressed_size == 0 {
            bail!("Compressed BOOTFS item has uncompressed size 0");
        }
        let vmo = Vmo::create(uncompressed_size as u64)?;
        // Pre-commit pages and map with MAP_RANGE so zstd decompression writes into pre-populated
        // page tables instead of taking individual page faults on each page of the output VMO.
        vmo.op_range(zx::VmoOp::COMMIT, 0, uncompressed_size as u64)?;
        let mut mapping = Mapping::create_from_vmo(
            &vmo,
            uncompressed_size,
            VmarFlags::PERM_READ | VmarFlags::PERM_WRITE | VmarFlags::MAP_RANGE,
        )?;
        let (ptr, len) = mapping.as_ptr_len();
        // SAFETY: `vmo` was just created above and is not shared with any other thread or process
        // during decompression, and `mapping` keeps `ptr..ptr + len` mapped readable and writable
        // for the duration of this block.
        let buffer = unsafe { slice::from_raw_parts_mut(ptr, len) };

        zstd_safe::decompress(buffer, payload)
            .map_err(|e| anyhow!("zstd decompression failed with error code: {}", e))?;
        vmo
    } else {
        let vmo = Vmo::create(payload.len() as u64)?;
        vmo.write(payload, 0)?;
        vmo
    };

    bootfs_vmo.set_name(&Name::new("uncompressed-bootfs")?)?;
    Ok(bootfs_vmo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use zbi::{ZbiContainer, align_buffer};

    fn create_test_zbi_vmo_with_prefix(
        prefix_items: &[(ZbiType, &[u8])],
        payload: &[u8],
        compressed: bool,
        uncompressed_len_override: Option<u32>,
        with_crc: bool,
        corrupt_crc: bool,
    ) -> Vmo {
        let mut raw_buf = vec![0u8; 32768];
        let aligned = align_buffer(&mut raw_buf[..]).unwrap();
        let size = {
            let mut container = ZbiContainer::new(&mut aligned[..]).unwrap();
            for &(ty, data) in prefix_items {
                container.create_entry_with_payload(ty, 0, ZbiFlags::default(), data).unwrap();
            }
            let extra = uncompressed_len_override.unwrap_or(payload.len() as u32);
            if compressed {
                let mut compressed_buf = vec![0u8; zstd_safe::compress_bound(payload.len())];
                let compressed_len = zstd_safe::compress(&mut compressed_buf, payload, 1).unwrap();
                container
                    .create_entry_with_payload(
                        ZbiType::StorageBootFs,
                        extra,
                        ZbiFlags::default(),
                        &compressed_buf[..compressed_len],
                    )
                    .unwrap();
                container.iter_mut().last().unwrap().header.flags |= ZBI_FLAGS_STORAGE_COMPRESSED;
            } else {
                container
                    .create_entry_with_payload(
                        ZbiType::StorageBootFs,
                        extra,
                        ZbiFlags::default(),
                        payload,
                    )
                    .unwrap();
            }
            if with_crc {
                let mut item = container.iter_mut().last().unwrap();
                item.header.flags |= ZBI_FLAGS_CRC32;
                let mut header_without_crc = *item.header;
                header_without_crc.crc32 = 0;
                let mut hasher = crc32fast::Hasher::new();
                hasher.update(header_without_crc.as_bytes());
                hasher.update(item.payload.as_bytes());
                let crc = hasher.finalize();
                item.header.crc32 = if corrupt_crc { crc ^ 0xffff_ffff } else { crc };
            }
            container.container_size().unwrap()
        };
        let zbi_bytes = &aligned[..size];
        let vmo = Vmo::create(zbi_bytes.len() as u64).unwrap();
        vmo.write(zbi_bytes, 0).unwrap();
        vmo
    }

    fn create_test_zbi_vmo(
        payload: &[u8],
        compressed: bool,
        uncompressed_len_override: Option<u32>,
        with_crc: bool,
        corrupt_crc: bool,
    ) -> Vmo {
        create_test_zbi_vmo_with_prefix(
            &[],
            payload,
            compressed,
            uncompressed_len_override,
            with_crc,
            corrupt_crc,
        )
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_uncompressed() {
        let payload = b"uncompressed bootfs payload data";
        let zbi_vmo = create_test_zbi_vmo(payload, false, None, false, false);
        let mut log = Log::new();

        let bootfs_vmo = get_bootfs_vmo(&zbi_vmo, false, &mut log).unwrap();

        assert_eq!(zbi_vmo.info().unwrap().num_mappings, 0);
        assert_eq!(zbi_vmo.info().unwrap().num_children, 0);
        assert_eq!(bootfs_vmo.get_name().unwrap(), "uncompressed-bootfs");
        let mut read_back = vec![0u8; payload.len()];
        bootfs_vmo.read(&mut read_back, 0).unwrap();
        assert_eq!(&read_back, payload);
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_compressed_with_crc() {
        let payload = vec![0x5au8; 8192];
        let zbi_vmo = create_test_zbi_vmo(&payload, true, None, true, false);
        let mut log = Log::new();

        let bootfs_vmo = get_bootfs_vmo(&zbi_vmo, true, &mut log).unwrap();

        assert_eq!(zbi_vmo.info().unwrap().num_mappings, 0);
        assert_eq!(zbi_vmo.info().unwrap().num_children, 0);
        assert_eq!(bootfs_vmo.info().unwrap().num_mappings, 0);
        assert_eq!(bootfs_vmo.get_name().unwrap(), "uncompressed-bootfs");
        let mut read_back = vec![0u8; payload.len()];
        bootfs_vmo.read(&mut read_back, 0).unwrap();
        assert_eq!(read_back, payload);
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_after_non_page_aligned_prefix_items() {
        let dummy_kernel = vec![0xa5u8; 5000];
        let cmdline = b"userboot.next=bin/test";
        let payload = vec![0x3cu8; 6000];
        let zbi_vmo = create_test_zbi_vmo_with_prefix(
            &[(ZbiType::Discard, &dummy_kernel), (ZbiType::CmdLine, cmdline)],
            &payload,
            true,
            None,
            true,
            false,
        );
        let mut log = Log::new();

        let items = ZbiItemIterator::new(&zbi_vmo).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[1].header.type_, ZbiType::CmdLine as u32);
        let read_cmdline = zbi_vmo
            .read_to_vec::<u8>(items[1].payload_offset, items[1].header.length.into())
            .unwrap();
        assert_eq!(&read_cmdline, cmdline);

        let bootfs_vmo = get_bootfs_vmo(&zbi_vmo, true, &mut log).unwrap();
        assert_eq!(zbi_vmo.info().unwrap().num_mappings, 0);
        assert_eq!(zbi_vmo.info().unwrap().num_children, 0);
        let mut read_back = vec![0u8; payload.len()];
        bootfs_vmo.read(&mut read_back, 0).unwrap();
        assert_eq!(read_back, payload);
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_crc_mismatch() {
        let payload = b"bootfs payload with bad crc";
        let zbi_vmo = create_test_zbi_vmo(payload, false, None, true, true);
        let mut log = Log::new();

        let res = get_bootfs_vmo(&zbi_vmo, true, &mut log);
        assert!(res.is_err());
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_compressed_zero_size() {
        let payload = b"non-empty";
        let zbi_vmo = create_test_zbi_vmo(payload, true, Some(0), false, false);
        let mut log = Log::new();

        let res = get_bootfs_vmo(&zbi_vmo, false, &mut log);
        assert!(res.is_err());
    }

    #[fuchsia::test]
    fn test_zbi_container_header_validation() {
        let payload = b"bootfs payload";
        let zbi_vmo = create_test_zbi_vmo(payload, false, None, false, false);
        let orig_header: ZbiHeader = zbi_vmo.read_to_object(0).unwrap();

        // Container header must not have ZBI_FLAGS_CRC32 set.
        let mut header_with_crc = orig_header;
        header_with_crc.flags |= ZBI_FLAGS_CRC32;
        header_with_crc.crc32 = 0x1234_5678;
        zbi_vmo.write(header_with_crc.as_bytes(), 0).unwrap();
        assert!(ZbiItemIterator::new(&zbi_vmo).is_err());

        // Container header length must be a multiple of ZBI_ALIGNMENT.
        let mut header_unaligned_len = orig_header;
        header_unaligned_len.length -= 1;
        zbi_vmo.write(header_unaligned_len.as_bytes(), 0).unwrap();
        assert!(ZbiItemIterator::new(&zbi_vmo).is_err());
    }
}
