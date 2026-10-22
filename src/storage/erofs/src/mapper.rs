// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::{ErofsError, ErofsFilesystem, FileNode, InodeDataLayout};
use fidl_fuchsia_storage_block as fblock;
use fuchsia_sync::Mutex;
use mapping::{
    DeviceExtent, Extent, Extents, MAPPING_VMO_SIZE, MappingCommand, PENDING_COMMANDS_CAPACITY,
    RawMappingCommand,
};
use std::collections::HashMap;
use vmo_fifo::SyncSender;

/// Manages a driver-backed `MapperSession` for an EROFS volume.
pub struct VolumeMapper {
    session: fblock::MapperSessionSynchronousProxy,
    sender: Mutex<SyncSender<RawMappingCommand>>,
    vmos_by_nid: Mutex<HashMap<u64, zx::Vmo>>,
    file_offset: u64,
}

impl VolumeMapper {
    /// Creates a new `VolumeMapper` over an established `MapperSession` and its `mapping_vmo`.
    pub fn new(
        session: fblock::MapperSessionSynchronousProxy,
        mapping_vmo: zx::Vmo,
        file_offset: u64,
    ) -> Result<Self, zx::Status> {
        let sender =
            SyncSender::<RawMappingCommand>::new(mapping_vmo, 8, PENDING_COMMANDS_CAPACITY)?;
        Ok(Self {
            session,
            sender: Mutex::new(sender),
            vmos_by_nid: Mutex::new(HashMap::new()),
            file_offset,
        })
    }

    /// Allocates a `mapping_vmo`, invokes `open_fn` to bind the server end of a `MapperSession`,
    /// and returns the initialized `VolumeMapper`.
    pub fn open_with_session(
        file_offset: u64,
        open_fn: impl FnOnce(
            zx::Vmo,
            fidl::endpoints::ServerEnd<fblock::MapperSessionMarker>,
        ) -> Result<(), zx::Status>,
    ) -> Result<Self, zx::Status> {
        let mapping_vmo = zx::Vmo::create(MAPPING_VMO_SIZE)?;
        let mapping_vmo_dup = mapping_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let (session_client, session_server) =
            fidl::endpoints::create_endpoints::<fblock::MapperSessionMarker>();
        open_fn(mapping_vmo_dup, session_server)?;
        Self::new(session_client.into_sync_proxy(), mapping_vmo, file_offset)
    }

    /// Returns a read-only VMO handle for `node`.
    ///
    /// Driver-backed pager VMOs (`FlatPlain` and `FlatInline`) are cached in `vmos_by_nid` for the
    /// lifetime of the `VolumeMapper` so that their driver mappings stay registered even if a VFS
    /// node is evicted while a process still has the VMO mapped, and so subsequent opens reuse the
    /// existing VMO. Anonymous VMOs created for empty or compressed files (`key == None`) are
    /// returned directly so their memory can be reclaimed once the caller drops the VMO.
    pub fn get_or_create_vmo(
        &self,
        fs: &ErofsFilesystem,
        node: &FileNode,
    ) -> Result<zx::Vmo, zx::Status> {
        let nid = node.nid();
        if let Some(vmo) = self.vmos_by_nid.lock().get(&nid) {
            return vmo.duplicate_handle(zx::Rights::SAME_RIGHTS);
        }

        let (vmo, key) = self.create_vmo_internal(fs, node)?;
        let Some(key) = key else {
            return Ok(vmo);
        };
        let mut vmos = self.vmos_by_nid.lock();
        if let Some(existing) = vmos.get(&nid) {
            self.close_file(key);
            return existing.duplicate_handle(zx::Rights::SAME_RIGHTS);
        }
        let dup = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        vmos.insert(nid, vmo);
        Ok(dup)
    }

    fn create_vmo_internal(
        &self,
        fs: &ErofsFilesystem,
        node: &FileNode,
    ) -> Result<(zx::Vmo, Option<u64>), zx::Status> {
        let size = node.size();
        let block_size = fs.block_size();
        let read_only_rights = zx::Rights::VMO_DEFAULT - zx::Rights::WRITE;

        if size == 0 {
            let vmo = zx::Vmo::create(0)?.replace_handle(read_only_rights)?;
            return Ok((vmo, None));
        }

        let extents = match node.data_layout() {
            InodeDataLayout::FlatPlain => {
                let offset = node
                    .blkaddr_offset(block_size, 0)
                    .map_err(|e| ErofsError::from(e).to_status())?;
                let aligned_size = size.next_multiple_of(block_size);
                vec![Extent::new(0..aligned_size, DeviceExtent::Block(self.file_offset + offset))]
            }
            InodeDataLayout::FlatInline => {
                let full_blocks_len = (size / block_size) * block_size;
                let tail_len = size - full_blocks_len;
                let mut extents = Vec::with_capacity(2);
                if full_blocks_len > 0 {
                    let offset = node
                        .blkaddr_offset(block_size, 0)
                        .map_err(|e| ErofsError::from(e).to_status())?;
                    extents.push(Extent::new(
                        0..full_blocks_len,
                        DeviceExtent::Block(self.file_offset + offset),
                    ));
                }
                if tail_len > 0 {
                    let inline_offset =
                        node.metadata_end_offset().map_err(|e| ErofsError::from(e).to_status())?;
                    extents.push(Extent::new(
                        full_blocks_len..(full_blocks_len + block_size),
                        DeviceExtent::InlineTail(self.file_offset + inline_offset),
                    ));
                }
                extents
            }
            InodeDataLayout::CompressedFull | InodeDataLayout::CompressedCompact => {
                let vmo = zx::Vmo::create(size)?;
                let mut buf = vec![0u8; size as usize];
                fs.read_file_range(node, 0, &mut buf).map_err(ErofsError::to_status)?;
                vmo.write(&buf, 0)?;
                let vmo = vmo.replace_handle(read_only_rights)?;
                return Ok((vmo, None));
            }
        };

        let encoded_extents = Extents::try_new(&extents, self.file_offset).map_err(|error| {
            log::error!(error:?; "Failed to build Extents for EROFS nid {}", node.nid());
            zx::Status::IO_DATA_INTEGRITY
        })?;
        let extent_count = encoded_extents.len() as u32;
        let allocation_size = extent_count as usize * 8;

        let (key, vmo) = self
            .session
            .create_vmo(size, fblock::CreateVmoOptions::empty(), zx::MonotonicInstant::INFINITE)
            .map_err(|_| zx::Status::PEER_CLOSED)
            .and_then(|r| r.map_err(zx::Status::err_from_raw))?;

        let commit_res = (|| -> Result<zx::Vmo, zx::Status> {
            let mut sender = self.sender.lock();
            let mut payload = sender.reserve_payload(allocation_size)?;
            for (mut chunk, val) in
                payload.data().chunks_mut(8).zip(Extents::encode_extents(&encoded_extents))
            {
                chunk.copy_from_slice(&val.to_le_bytes());
            }
            let cmd = MappingCommand::Mappings {
                key,
                offset: payload.offset(),
                stored_size: size,
                device_offset: self.file_offset,
                metadata_count: 0,
                extent_count,
                encrypted: false,
            };
            payload.commit(cmd.into())?;
            drop(sender);
            vmo.replace_handle(read_only_rights)
        })();

        match commit_res {
            Ok(vmo) => Ok((vmo, Some(key))),
            Err(status) => {
                self.close_file(key);
                Err(status)
            }
        }
    }

    /// Closes a previously mapped file key in both the block driver and `BlobPagerAndVerifier`.
    pub fn close_file(&self, key: u64) {
        let _ = self.sender.lock().push(MappingCommand::CloseBlob { key }.into());
        let _ = self.session.close_vmo(key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readers::VecReader;
    use crate::{Node, format};
    use futures::TryStreamExt as _;
    use std::sync::Arc;
    use zerocopy::IntoBytes as _;
    use zerocopy::byteorder::little_endian::{U16 as LEU16, U32 as LEU32, U64 as LEU64};

    fn make_test_fs() -> ErofsFilesystem {
        let block_size = 4096usize;
        let num_blocks = 8;
        let mut image = vec![0u8; num_blocks * block_size];

        let sb = format::SuperBlock {
            magic: LEU32::new(format::EROFS_MAGIC),
            checksum: LEU32::new(0),
            feature_compat: LEU32::new(0),
            block_size_bits: 12,
            sb_ext_slots: 0,
            root_nid: LEU16::new(0),
            inode_count: LEU64::new(5),
            epoch: LEU64::new(0),
            fixed_nsec: LEU32::new(0),
            blocks: LEU32::new(num_blocks as u32),
            meta_block_addr: LEU32::new(1),
            xattr_block_addr: LEU32::new(0),
            uuid: [0; 16],
            volume_name: [0; 16],
            feature_incompat: LEU32::new(0),
            available_compr_algs: LEU16::new(0),
            extra_devices: LEU32::new(0),
            dirblkbits: 0,
            reserved: [0; 37],
        };
        image[1024..1024 + 128].copy_from_slice(sb.as_bytes());

        // Root dir at nid 0 (offset 4096)
        let root_inode = format::InodeCompact {
            format: LEU16::new(0),
            xattr_icount: LEU16::new(0),
            mode: LEU16::new(0o040755),
            link_count: LEU16::new(2),
            size: LEU32::new(0),
            reserved_1: [0; 4],
            i_u: [0; 4],
            ino: LEU32::new(1),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        image[4096..4096 + 32].copy_from_slice(root_inode.as_bytes());

        // nid 1 (offset 4128): FlatPlain file, 5000 bytes, starting at block 2 (offset 8192)
        let plain_inode = format::InodeCompact {
            format: LEU16::new((InodeDataLayout::FlatPlain as u16) << 1),
            xattr_icount: LEU16::new(0),
            mode: LEU16::new(0o100644),
            link_count: LEU16::new(1),
            size: LEU32::new(5000),
            reserved_1: [0; 4],
            i_u: 2u32.to_le_bytes(),
            ino: LEU32::new(2),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        image[4128..4128 + 32].copy_from_slice(plain_inode.as_bytes());

        // nid 2 (offset 4160): FlatInline tail-only file, 500 bytes (inline data at 4192..4692)
        let inline_tail_only = format::InodeCompact {
            format: LEU16::new((InodeDataLayout::FlatInline as u16) << 1),
            xattr_icount: LEU16::new(0),
            mode: LEU16::new(0o100644),
            link_count: LEU16::new(1),
            size: LEU32::new(500),
            reserved_1: [0; 4],
            i_u: 0u32.to_le_bytes(),
            ino: LEU32::new(3),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        image[4160..4160 + 32].copy_from_slice(inline_tail_only.as_bytes());

        // nid 20 (offset 4096 + 20 * 32 = 4736): FlatInline multi-extent file, 4096 + 300 bytes,
        // full block at block 4 (offset 16384), inline tail at 4768..5068
        let inline_multi = format::InodeCompact {
            format: LEU16::new((InodeDataLayout::FlatInline as u16) << 1),
            xattr_icount: LEU16::new(0),
            mode: LEU16::new(0o100644),
            link_count: LEU16::new(1),
            size: LEU32::new(4096 + 300),
            reserved_1: [0; 4],
            i_u: 4u32.to_le_bytes(),
            ino: LEU32::new(4),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        image[4736..4736 + 32].copy_from_slice(inline_multi.as_bytes());

        // nid 40 (offset 4096 + 40 * 32 = 5376): Empty file (0 bytes)
        let empty_inode = format::InodeCompact {
            format: LEU16::new((InodeDataLayout::FlatPlain as u16) << 1),
            xattr_icount: LEU16::new(0),
            mode: LEU16::new(0o100644),
            link_count: LEU16::new(1),
            size: LEU32::new(0),
            reserved_1: [0; 4],
            i_u: 0u32.to_le_bytes(),
            ino: LEU32::new(5),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        image[5376..5376 + 32].copy_from_slice(empty_inode.as_bytes());

        ErofsFilesystem::new(Arc::new(VecReader::new(image))).expect("parse test fs")
    }

    #[fuchsia::test]
    fn test_volume_mapper_extents_and_caching() {
        let fs = make_test_fs();
        let file_offset = 4096u64;
        let created_vmos = Arc::new(Mutex::new(Vec::<(u64, u64)>::new()));
        let created_vmos_clone = created_vmos.clone();

        let (receiver_tx, receiver_rx) = std::sync::mpsc::channel();
        let mapper =
            VolumeMapper::open_with_session(file_offset, move |mapping_vmo, server_end| {
                let receiver = vmo_fifo::Receiver::<RawMappingCommand>::new(
                    mapping_vmo,
                    PENDING_COMMANDS_CAPACITY,
                )
                .expect("create Receiver");
                receiver_tx.send(receiver).unwrap();

                std::thread::spawn(move || {
                    let mut executor = fuchsia_async::LocalExecutor::default();
                    executor.run_singlethreaded(async move {
                        let mut stream = server_end.into_stream();
                        let mut next_key = 1u64;
                        while let Ok(Some(req)) = stream.try_next().await {
                            match req {
                                fblock::MapperSessionRequest::CreateVmo {
                                    size, responder, ..
                                } => {
                                    let key = next_key;
                                    next_key += 1;
                                    created_vmos_clone.lock().push((key, size));
                                    let vmo = zx::Vmo::create(size).unwrap();
                                    let _ = responder.send(Ok((key, vmo)));
                                }
                                fblock::MapperSessionRequest::CloseVmo { .. } => {}
                                fblock::MapperSessionRequest::Close { responder } => {
                                    let _ = responder.send(Ok(()));
                                    break;
                                }
                                _ => unreachable!(),
                            }
                        }
                    });
                });
                Ok(())
            })
            .expect("open_with_session");

        let receiver = receiver_rx.recv().unwrap();

        // 1. Empty file (nid 40): creates 0-byte read-only VMO without any driver RPC, and does
        // not retain the anonymous VMO in `vmos_by_nid`.
        let Node::File(empty_node) = fs.node(40).unwrap() else { panic!() };
        let empty_vmo = mapper.get_or_create_vmo(&fs, &empty_node).unwrap();
        assert_eq!(empty_vmo.get_size().unwrap(), 0);
        assert!(!empty_vmo.basic_info().unwrap().rights.contains(zx::Rights::WRITE));
        assert!(created_vmos.lock().is_empty());
        assert!(!mapper.vmos_by_nid.lock().contains_key(&40));

        // 2. FlatPlain file (nid 1, 5000 bytes at block 2 = 8192):
        let Node::File(plain_node) = fs.node(1).unwrap() else { panic!() };
        let plain_vmo1 = mapper.get_or_create_vmo(&fs, &plain_node).unwrap();
        let plain_vmo2 = mapper.get_or_create_vmo(&fs, &plain_node).unwrap();
        assert_eq!(plain_vmo1.koid().unwrap(), plain_vmo2.koid().unwrap());
        assert!(mapper.vmos_by_nid.lock().contains_key(&1));
        assert!(!plain_vmo1.basic_info().unwrap().rights.contains(zx::Rights::WRITE));
        assert_eq!(*created_vmos.lock(), vec![(1, 5000)]);

        let msg = receiver.claim().unwrap();
        let MappingCommand::Mappings {
            key, offset, stored_size, device_offset, extent_count, ..
        } = MappingCommand::try_from(*msg).unwrap()
        else {
            panic!("expected Mappings command");
        };
        assert_eq!(key, 1);
        assert_eq!(stored_size, 5000);
        assert_eq!(device_offset, file_offset);
        let slice = msg.payload_slice(offset, extent_count * 8);
        let mut words = vec![0u64; extent_count as usize];
        for (i, word) in words.iter_mut().enumerate() {
            let mut bytes = [0u8; 8];
            slice.subslice(i * 8..(i + 1) * 8).copy_to_slice(&mut bytes);
            *word = u64::from_le_bytes(bytes);
        }
        let decoded = Extents::from_encoded(words, device_offset).unwrap();
        assert_eq!(
            decoded,
            Extents::try_new(
                &[Extent::new(0..8192, DeviceExtent::Block(file_offset + 2 * 4096))],
                device_offset
            )
            .unwrap()
        );
        msg.pop().unwrap();

        // 3. FlatInline tail-only file (nid 2, 500 bytes at inline offset 4160 + 32 = 4192):
        let Node::File(inline_node) = fs.node(2).unwrap() else { panic!() };
        let _inline_vmo = mapper.get_or_create_vmo(&fs, &inline_node).unwrap();
        assert_eq!(*created_vmos.lock(), vec![(1, 5000), (2, 500)]);

        let msg = receiver.claim().unwrap();
        let MappingCommand::Mappings {
            key, offset, stored_size, device_offset, extent_count, ..
        } = MappingCommand::try_from(*msg).unwrap()
        else {
            panic!("expected Mappings command");
        };
        assert_eq!(key, 2);
        assert_eq!(stored_size, 500);
        let slice = msg.payload_slice(offset, extent_count * 8);
        let mut words = vec![0u64; extent_count as usize];
        for (i, word) in words.iter_mut().enumerate() {
            let mut bytes = [0u8; 8];
            slice.subslice(i * 8..(i + 1) * 8).copy_to_slice(&mut bytes);
            *word = u64::from_le_bytes(bytes);
        }
        let decoded = Extents::from_encoded(words, device_offset).unwrap();
        assert_eq!(
            decoded,
            Extents::try_new(
                &[Extent::new(0..4096, DeviceExtent::InlineTail(file_offset + 4192))],
                device_offset
            )
            .unwrap()
        );
        msg.pop().unwrap();

        // 4. FlatInline multi-extent file (nid 20, 4396 bytes: block 4 + inline offset 4768):
        let Node::File(multi_node) = fs.node(20).unwrap() else { panic!() };
        let _multi_vmo = mapper.get_or_create_vmo(&fs, &multi_node).unwrap();
        assert_eq!(*created_vmos.lock(), vec![(1, 5000), (2, 500), (3, 4396)]);

        let msg = receiver.claim().unwrap();
        let MappingCommand::Mappings {
            key, offset, stored_size, device_offset, extent_count, ..
        } = MappingCommand::try_from(*msg).unwrap()
        else {
            panic!("expected Mappings command");
        };
        assert_eq!(key, 3);
        assert_eq!(stored_size, 4396);
        let slice = msg.payload_slice(offset, extent_count * 8);
        let mut words = vec![0u64; extent_count as usize];
        for (i, word) in words.iter_mut().enumerate() {
            let mut bytes = [0u8; 8];
            slice.subslice(i * 8..(i + 1) * 8).copy_to_slice(&mut bytes);
            *word = u64::from_le_bytes(bytes);
        }
        let decoded = Extents::from_encoded(words, device_offset).unwrap();
        assert_eq!(
            decoded,
            Extents::try_new(
                &[
                    Extent::new(0..4096, DeviceExtent::Block(file_offset + 4 * 4096)),
                    Extent::new(4096..8192, DeviceExtent::InlineTail(file_offset + 4768)),
                ],
                device_offset
            )
            .unwrap()
        );
        msg.pop().unwrap();
    }
}
