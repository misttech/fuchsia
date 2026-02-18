// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::directory::ExtDirectory;
use crate::file::ExtFile;
use crate::types::ExtAttributes;
use ext4_read_only::processor::Ext4Processor;
use ext4_read_only::readers::{BlockDeviceReader, ReaderWriter, VmoReader};
use ext4_read_only::structs::{self, EntryType, MIN_EXT4_SIZE};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_storage_block::BlockMarker;
use log::error;
use std::sync::Arc;

mod directory;
mod file;
mod node;
mod types;

pub enum FsSourceType {
    BlockDevice(ClientEnd<BlockMarker>),
    Vmo(zx::Vmo),
}

#[derive(Debug, PartialEq)]
pub enum ConstructFsError {
    VmoReadError(zx::Status),
    ParsingError(structs::ParsingError),
    FileVmoError(zx::Status),
    NodeError(zx::Status),
}

impl From<structs::ParsingError> for ConstructFsError {
    fn from(value: structs::ParsingError) -> Self {
        Self::ParsingError(value)
    }
}

// Default is to create read-only fs
pub fn construct_fs(source: FsSourceType) -> Result<Arc<ExtDirectory>, ConstructFsError> {
    // TODO(https://fxbug.dev/479943428): Enable creating writeable fs when this is fully supported.
    construct_fs_internal(source, true)
}

fn construct_fs_internal(
    source: FsSourceType,
    read_only: bool,
) -> Result<Arc<ExtDirectory>, ConstructFsError> {
    let reader: Arc<dyn ReaderWriter> = match source {
        FsSourceType::BlockDevice(block_device) => {
            Arc::new(BlockDeviceReader::from_client_end(block_device).map_err(|e| {
                error!("Error constructing file system: {}", e);
                ConstructFsError::VmoReadError(zx::Status::IO_INVALID)
            })?)
        }
        FsSourceType::Vmo(vmo) => {
            let size = vmo.get_size().map_err(ConstructFsError::VmoReadError)?;
            if size < MIN_EXT4_SIZE as u64 {
                // Too small to even fit the first copy of the ext4 Super Block.
                return Err(ConstructFsError::VmoReadError(zx::Status::NO_SPACE));
            }

            Arc::new(VmoReader::new(Arc::new(vmo)))
        }
    };
    let processor = Ext4Processor::new(reader, read_only);
    build_fs_dir(Arc::new(processor), structs::ROOT_INODE_NUM, read_only)
}

fn build_fs_dir(
    processor: Arc<Ext4Processor>,
    ino: u32,
    read_only: bool,
) -> Result<Arc<ExtDirectory>, ConstructFsError> {
    let inode = processor.inode(ino)?;
    let entries = processor.entries_from_inode(&inode)?;
    let attributes = ExtAttributes::from_inode(inode);
    let xattrs = processor.inode_xattrs(ino)?;
    let dir = ExtDirectory::new(ino as u64, attributes, xattrs);

    for entry in entries {
        let entry_name = entry.name()?;
        if entry_name == "." || entry_name == ".." {
            continue;
        }

        let entry_ino = u32::from(entry.e2d_ino);
        match EntryType::from_u8(entry.e2d_type)? {
            EntryType::Directory => {
                dir.insert_child(
                    entry_name,
                    build_fs_dir(processor.clone(), entry_ino, read_only)?,
                )
                .map_err(ConstructFsError::NodeError)?;
            }
            EntryType::RegularFile => {
                dir.insert_child(
                    entry_name,
                    ExtFile::from_processor(processor.clone(), entry_ino, read_only)
                        .map_err(ConstructFsError::NodeError)?,
                )
                .map_err(ConstructFsError::NodeError)?;
            }
            _ => {
                // TODO(https://fxbug.dev/42073143): Handle other types.
            }
        }
    }

    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::{FsSourceType, construct_fs, construct_fs_internal};

    use ext4_read_only::structs::MIN_EXT4_SIZE;
    use fuchsia_fs::directory::{DirEntry, DirentKind, open_file, open_node, readdir};
    use fuchsia_fs::file::{read_to_string, write};
    use std::fs;
    use std::sync::Arc;
    use vmo_backed_block_server::{InitialContents, VmoBackedServerOptions};
    use zx::{Status, Vmo};
    use {fidl_fuchsia_io as fio, fidl_fuchsia_storage_block as fblock, fuchsia_async as fasync};

    #[fuchsia::test]
    fn image_too_small() {
        let vmo = Vmo::create(10).expect("VMO is created");
        vmo.write(b"too small", 0).expect("VMO write() succeeds");
        let buffer = FsSourceType::Vmo(vmo);

        assert!(construct_fs(buffer).is_err(), "Expected failed parsing of VMO.");
    }

    #[fuchsia::test]
    fn invalid_fs() {
        let vmo = Vmo::create(MIN_EXT4_SIZE as u64).expect("VMO is created");
        vmo.write(b"not ext4", 0).expect("VMO write() succeeds");
        let buffer = FsSourceType::Vmo(vmo);

        assert!(construct_fs(buffer).is_err(), "Expected failed parsing of VMO.");
    }

    #[fuchsia::test]
    async fn list_root() {
        let data = fs::read("/pkg/data/nest.img").expect("Unable to read file");
        let vmo = Vmo::create(data.len() as u64).expect("VMO is created");
        vmo.write(data.as_slice(), 0).expect("VMO write() succeeds");
        let buffer = FsSourceType::Vmo(vmo);

        let tree = construct_fs(buffer).expect("construct_fs parses the vmo");
        let root = vfs::directory::serve(tree, fio::PERM_READABLE);

        let expected = vec![
            DirEntry { name: String::from("file1"), kind: DirentKind::File },
            DirEntry { name: String::from("inner"), kind: DirentKind::Directory },
            DirEntry { name: String::from("lost+found"), kind: DirentKind::Directory },
        ];
        assert_eq!(readdir(&root).await.unwrap(), expected);

        let file = open_file(&root, "file1", fio::PERM_READABLE).await.unwrap();
        assert_eq!(read_to_string(&file).await.unwrap(), "file1 contents.\n");
        file.close().await.unwrap().map_err(zx::Status::from_raw).unwrap();
        root.close().await.unwrap().map_err(zx::Status::from_raw).unwrap();
    }

    #[fuchsia::test]
    async fn get_dac_attributes() {
        let data = fs::read("/pkg/data/dac_attributes.img").expect("Unable to read file");
        let vmo = Vmo::create(data.len() as u64).expect("VMO is created");
        vmo.write(data.as_slice(), 0).expect("VMO write() succeeds");
        let buffer = FsSourceType::Vmo(vmo);

        let tree = construct_fs(buffer).expect("construct_fs parses the VMO");
        let root = vfs::directory::serve(tree, fio::PERM_READABLE);

        let expected_entries = vec![
            DirEntry { name: String::from("dir_1000"), kind: DirentKind::Directory },
            DirEntry { name: String::from("dir_root"), kind: DirentKind::Directory },
            DirEntry { name: String::from("file_1000"), kind: DirentKind::File },
            DirEntry { name: String::from("file_root"), kind: DirentKind::File },
            DirEntry { name: String::from("lost+found"), kind: DirentKind::Directory },
        ];
        assert_eq!(readdir(&root).await.unwrap(), expected_entries);

        #[derive(Debug, PartialEq)]
        struct Node {
            name: String,
            mode: u32,
            uid: u32,
            gid: u32,
        }

        let expected_attributes = vec![
            Node { name: String::from("dir_1000"), mode: 0x416D, uid: 1000, gid: 1000 },
            Node { name: String::from("dir_root"), mode: 0x4140, uid: 0, gid: 0 },
            Node { name: String::from("file_1000"), mode: 0x8124, uid: 1000, gid: 1000 },
            Node { name: String::from("file_root"), mode: 0x8100, uid: 0, gid: 0 },
        ];

        let attributes_query = fio::NodeAttributesQuery::MODE
            | fio::NodeAttributesQuery::UID
            | fio::NodeAttributesQuery::GID;
        for expected_node in &expected_attributes {
            let node_proxy = open_node(&root, expected_node.name.as_str(), fio::PERM_READABLE)
                .await
                .expect("node open failed");
            let (mut_attrs, _immut_attrs) = node_proxy
                .get_attributes(attributes_query)
                .await
                .expect("node get_attributes() failed")
                .map_err(Status::from_raw)
                .expect("node get_attributes() error");

            let node = Node {
                name: expected_node.name.clone(),
                mode: mut_attrs.mode.expect("node attributes missing mode"),
                uid: mut_attrs.uid.expect("node attributes missing uid"),
                gid: mut_attrs.gid.expect("node attributes missing gid"),
            };

            node_proxy
                .close()
                .await
                .expect("node close failed")
                .map_err(Status::from_raw)
                .expect("node close error");

            assert_eq!(node, *expected_node);
        }

        root.close().await.unwrap().map_err(Status::from_raw).unwrap();
    }

    #[fuchsia::test]
    async fn test_constructing_writeable_fs_and_writing_to_allocated_region() {
        // Create a device that is Ext4 formatted.
        let data = fs::read("/pkg/data/nest.img").expect("failed to read file");
        let vmo = Vmo::create(data.len() as u64).expect("failed to create VMO");
        vmo.write(data.as_slice(), 0).expect("failed to write to VMO");
        let server = Arc::new(
            VmoBackedServerOptions {
                block_size: 512,
                initial_contents: InitialContents::FromVmo(vmo),
                ..Default::default()
            }
            .build()
            .expect("build from VmoBackedServerOptions failed"),
        );

        let server_clone = server.clone();
        let (block_client_end1, block_server_end1) =
            fidl::endpoints::create_endpoints::<fblock::BlockMarker>();
        std::thread::spawn(move || {
            let mut executor = fasync::TestExecutor::new();
            let _task =
                executor.run_singlethreaded(server_clone.serve(block_server_end1.into_stream()));
        });

        // Write to the allocated extent of this file.
        let tree = construct_fs_internal(
            FsSourceType::BlockDevice(block_client_end1),
            /* read_only= */ false,
        )
        .expect("failed to parse the vmo");
        let root = vfs::directory::serve(tree, fio::PERM_READABLE | fio::PERM_WRITABLE);
        let file = open_file(&root, "file1", fio::PERM_READABLE | fio::PERM_WRITABLE)
            .await
            .expect("failed to open file");
        let original_contents = "file1 contents.\n";
        assert_eq!(read_to_string(&file).await.expect("failed to read file"), original_contents);
        let new_contents = "new contents.";
        let offset = 5;
        file.seek(fio::SeekOrigin::Start, offset)
            .await
            .expect("failed FIDL seek")
            .map_err(zx::Status::from_raw)
            .expect("failed to seek file");
        write(&file, new_contents).await.expect("failed to write to file");
        file.close()
            .await
            .expect("failed FIDL file close")
            .map_err(zx::Status::from_raw)
            .expect("failed to close file");
        root.close()
            .await
            .expect("failed FIDL dir close")
            .map_err(zx::Status::from_raw)
            .expect("failed to close root");

        // Construct Ext4 fs again, and verify that the written data is still there.
        let server_clone = server.clone();
        let (block_client_end2, block_server_end2) =
            fidl::endpoints::create_endpoints::<fblock::BlockMarker>();
        std::thread::spawn(move || {
            let mut executor = fasync::TestExecutor::new();
            let _task =
                executor.run_singlethreaded(server_clone.serve(block_server_end2.into_stream()));
        });
        let tree = construct_fs_internal(
            FsSourceType::BlockDevice(block_client_end2),
            /* read_only= */ true,
        )
        .expect("construct_fs parses the vmo");
        let root = vfs::directory::serve(tree, fio::PERM_READABLE);
        let file =
            open_file(&root, "file1", fio::PERM_READABLE).await.expect("failed to open file");
        let mut expected_bytes = original_contents.as_bytes().to_vec();
        expected_bytes.resize(offset as usize + new_contents.len(), 0);
        expected_bytes[offset as usize..].copy_from_slice(new_contents.as_bytes());
        assert_eq!(
            read_to_string(&file).await.expect("failed to read file"),
            String::from_utf8(expected_bytes).unwrap()
        );
        file.close()
            .await
            .expect("failed FIDL file close")
            .map_err(zx::Status::from_raw)
            .expect("failed to close file");
        root.close()
            .await
            .expect("failed FIDL dir close")
            .map_err(zx::Status::from_raw)
            .expect("failed to close root");
    }
}
