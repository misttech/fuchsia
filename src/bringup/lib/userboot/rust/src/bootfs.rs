// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, anyhow, bail};
use mapped_vmo::Mapping;
use std::fmt::Write as _;
use std::slice;
use zbi::zbi_format::{ZBI_FLAGS_CRC32, ZBI_FLAGS_STORAGE_COMPRESSED};
use zbi::{ZbiContainer, ZbiType};
use zerocopy::IntoBytes as _;
use zx::{Name, VmarFlags, Vmo};
use zx_libc::sanitizer::Log;

/// Extracts the BOOTFS VMO from the ZBI container, decompressing it if needed.
pub fn get_bootfs_vmo(
    container: &ZbiContainer<&[u8]>,
    check_crc: bool,
    log: &mut Log,
) -> Result<Vmo, Error> {
    let bootfs_item = container
        .iter()
        .find(|item| item.header.type_ == ZbiType::StorageBootFs as u32)
        .ok_or_else(|| anyhow!("StorageBootFs item not found in ZBI"))?;

    if check_crc && (bootfs_item.header.flags & ZBI_FLAGS_CRC32) != 0 {
        writeln!(log, "Checking BOOTFS item CRC32 {:#x}...", bootfs_item.header.crc32)?;
        let mut header_without_crc = *bootfs_item.header;
        header_without_crc.crc32 = 0;
        let mut hasher = crc32fast::Hasher::new();
        hasher.update(header_without_crc.as_bytes());
        hasher.update(bootfs_item.payload.as_bytes());
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

        let payload = bootfs_item.payload.as_bytes();
        zstd_safe::decompress(buffer, payload)
            .map_err(|e| anyhow!("zstd decompression failed with error code: {}", e))?;
        vmo
    } else {
        let payload_bytes = bootfs_item.payload.as_bytes();
        let vmo = Vmo::create(payload_bytes.len() as u64)?;
        vmo.write(payload_bytes, 0)?;
        vmo
    };

    bootfs_vmo.set_name(&Name::new("uncompressed-bootfs")?)?;
    Ok(bootfs_vmo)
}

#[cfg(test)]
mod tests {
    use super::*;
    use mapped_vmo::ImmutableMapping;
    use zbi::{ZbiFlags, align_buffer};

    fn create_test_zbi_vmo(
        payload: &[u8],
        compressed: bool,
        uncompressed_len_override: Option<u32>,
        with_crc: bool,
        corrupt_crc: bool,
    ) -> Vmo {
        let mut raw_buf = vec![0u8; 16384];
        let aligned = align_buffer(&mut raw_buf[..]).unwrap();
        let size = {
            let mut container = ZbiContainer::new(&mut aligned[..]).unwrap();
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
                container.iter_mut().next().unwrap().header.flags |= ZBI_FLAGS_STORAGE_COMPRESSED;
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
                let mut item = container.iter_mut().next().unwrap();
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

    #[fuchsia::test]
    fn test_get_bootfs_vmo_uncompressed() {
        let payload = b"uncompressed bootfs payload data";
        let zbi_vmo = create_test_zbi_vmo(payload, false, None, false, false);
        let mut log = Log::new();

        let zbi = ImmutableMapping::create_from_vmo(&zbi_vmo, true).unwrap();
        let container = ZbiContainer::parse(&zbi[..]).unwrap();
        let bootfs_vmo = get_bootfs_vmo(&container, false, &mut log).unwrap();

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

        let zbi = ImmutableMapping::create_from_vmo(&zbi_vmo, true).unwrap();
        let container = ZbiContainer::parse(&zbi[..]).unwrap();
        let bootfs_vmo = get_bootfs_vmo(&container, true, &mut log).unwrap();

        assert_eq!(bootfs_vmo.info().unwrap().num_mappings, 0);
        assert_eq!(bootfs_vmo.get_name().unwrap(), "uncompressed-bootfs");
        let mut read_back = vec![0u8; payload.len()];
        bootfs_vmo.read(&mut read_back, 0).unwrap();
        assert_eq!(read_back, payload);
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_crc_mismatch() {
        let payload = b"bootfs payload with bad crc";
        let zbi_vmo = create_test_zbi_vmo(payload, false, None, true, true);
        let mut log = Log::new();

        let zbi = ImmutableMapping::create_from_vmo(&zbi_vmo, true).unwrap();
        let container = ZbiContainer::parse(&zbi[..]).unwrap();
        let res = get_bootfs_vmo(&container, true, &mut log);
        assert!(res.is_err());
    }

    #[fuchsia::test]
    fn test_get_bootfs_vmo_compressed_zero_size() {
        let payload = b"non-empty";
        let zbi_vmo = create_test_zbi_vmo(payload, true, Some(0), false, false);
        let mut log = Log::new();

        let zbi = ImmutableMapping::create_from_vmo(&zbi_vmo, true).unwrap();
        let container = ZbiContainer::parse(&zbi[..]).unwrap();
        let res = get_bootfs_vmo(&container, false, &mut log);
        assert!(res.is_err());
    }
}
