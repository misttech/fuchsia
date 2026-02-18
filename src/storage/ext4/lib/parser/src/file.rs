// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use ext4_read_only::parser::XattrMap;
use fidl_fuchsia_io as fio;
use std::sync::Arc;
use vfs::directory::entry::{DirectoryEntry, EntryInfo, GetEntryInfo, OpenRequest};
use vfs::execution_scope::ExecutionScope;
use vfs::file::{
    FidlIoConnection, File, FileIo, FileLike, FileOptions, GetVmo, StreamIoConnection, SyncMode,
};
use vfs::node::Node;
use vfs::{ObjectRequestRef, immutable_attributes};
use zx::{self, HandleBased as _, Status, Vmo};

use crate::types::ExtAttributes;
use ext4_read_only::processor::Ext4Processor;

/// An ext4 filesystem file node.
pub struct ExtFile {
    inode: u64,
    attributes: ExtAttributes,
    xattrs: XattrMap,
    vmo: Vmo,
    // ExtFile is implied to be read-only if processor is None. This is because the processor is
    // only used to persist data to the underlying device. It is not required for reading data.
    processor: Option<Arc<Ext4Processor>>,
}

impl std::fmt::Debug for ExtFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ExtFile")
            .field("inode", &self.inode)
            .field("attributes", &self.attributes)
            .field("xattrs", &self.xattrs)
            .field("vmo", &self.vmo)
            .finish_non_exhaustive()
    }
}

impl ExtFile {
    /// Creates a new [`ExtFile`] with the given `inode`, `attributes`, and `vmo`.
    pub fn new(
        inode: u64,
        attributes: ExtAttributes,
        xattrs: XattrMap,
        vmo: Vmo,
        processor: Option<Arc<Ext4Processor>>,
    ) -> Arc<Self> {
        Arc::new(Self { inode, attributes, xattrs, vmo, processor })
    }

    /// Creates a new read-only [`ExtFile`] with the given `inode`, `attributes`, and `data`.
    pub fn readonly_from_data(
        inode: u64,
        attributes: ExtAttributes,
        xattrs: XattrMap,
        data: impl AsRef<[u8]>,
    ) -> Result<Arc<Self>, Status> {
        let bytes = data.as_ref();
        let vmo = Vmo::create(bytes.len().try_into().map_err(|_| Status::OUT_OF_RANGE)?)?;
        if !bytes.is_empty() {
            vmo.write(bytes, 0)?;
        }
        Ok(Self::new(inode, attributes, xattrs, vmo, None))
    }

    /// Creates a new [`ExtFile`] from the processor with the given inode number.
    pub fn from_processor(
        processor: Arc<Ext4Processor>,
        ino: u32,
        read_only: bool,
    ) -> Result<Arc<Self>, Status> {
        let inode = processor.inode(ino).map_err(|_| Status::INTERNAL)?;
        let attributes = ExtAttributes::from_inode(inode);
        let xattrs = processor.inode_xattrs(ino).map_err(|_| Status::INTERNAL)?;
        let data = processor.read_data(ino).map_err(|_| Status::INTERNAL)?;
        let vmo = Vmo::create(data.len().try_into().map_err(|_| Status::OUT_OF_RANGE)?)?;
        if !data.is_empty() {
            vmo.write(data.as_ref(), 0)?;
        }
        if !read_only && processor.read_only() {
            return Err(Status::INTERNAL);
        }

        Ok(Self::new(
            ino as u64,
            attributes,
            xattrs,
            vmo,
            // Store the Ext4 processor for write functionalities
            if read_only { None } else { Some(processor) },
        ))
    }

    fn read_only(&self) -> bool {
        self.processor.is_none()
    }
}

impl GetEntryInfo for ExtFile {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.inode, fio::DirentType::File)
    }
}

impl DirectoryEntry for ExtFile {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), Status> {
        request.open_file(self)
    }
}

impl Node for ExtFile {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, Status> {
        let content_size = if requested_attributes.intersects(
            fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
        ) {
            Some(self.vmo.get_content_size()?)
        } else {
            None
        };

        Ok(self.attributes.overlay_node_attributes(
            requested_attributes,
            immutable_attributes!(
                requested_attributes,
                Immutable {
                    protocols: fio::NodeProtocolKinds::FILE,
                    abilities: fio::Operations::GET_ATTRIBUTES | fio::Operations::READ_BYTES,
                    content_size: content_size,
                    storage_size: content_size,
                    id: self.inode,
                }
            ),
        ))
    }

    async fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Status> {
        Ok(self.xattrs.keys().map(Clone::clone).collect())
    }

    async fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Status> {
        self.xattrs.get(&name).map(Clone::clone).ok_or(Status::NOT_FOUND)
    }
}

impl FileLike for ExtFile {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        options: FileOptions,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), Status> {
        if !self.read_only() {
            // Use a FidlIoConnection to manage writes. Note that reads will be slower because they
            // won't be using a stream.
            FidlIoConnection::create_sync(scope, self, options, object_request.take());
        } else {
            StreamIoConnection::create_sync(scope, self, options, object_request.take());
        }
        Ok(())
    }
}

impl File for ExtFile {
    fn readable(&self) -> bool {
        true
    }

    fn writable(&self) -> bool {
        !self.read_only()
    }

    fn executable(&self) -> bool {
        false
    }

    async fn open_file(&self, _options: &FileOptions) -> Result<(), Status> {
        Ok(())
    }

    async fn truncate(&self, _length: u64) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    async fn get_backing_memory(&self, flags: fio::VmoFlags) -> Result<Vmo, Status> {
        // Logic here matches fuchsia.io requirements and matches what works for memfs.
        // Shared requests are satisfied by duplicating an handle, and private shares are
        // child VMOs.
        let vmo_rights = vmo_flags_to_rights(flags)
            | zx::Rights::BASIC
            | zx::Rights::MAP
            | zx::Rights::GET_PROPERTY;
        // Unless private sharing mode is specified, we always default to shared.
        if flags.contains(fio::VmoFlags::PRIVATE_CLONE) {
            get_as_private(&self.vmo, vmo_rights)
        } else {
            self.vmo.duplicate_handle(vmo_rights)
        }
    }

    async fn get_size(&self) -> Result<u64, Status> {
        self.vmo.get_content_size()
    }

    async fn update_attributes(
        &self,
        _attributes: fio::MutableNodeAttributes,
    ) -> Result<(), Status> {
        Err(Status::NOT_SUPPORTED)
    }

    async fn sync(&self, _mode: SyncMode) -> Result<(), Status> {
        Ok(())
    }
}

// Trait required for `FidlIoConnection` to support write operations.
impl FileIo for ExtFile {
    async fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<u64, Status> {
        // TODO(https://fxbug.dev/479943428): When full write support is implemented, ensure read_at
        // handles potential race conditions with concurrent writes.
        let vmo_size = self.vmo.get_content_size()?;
        if offset >= vmo_size {
            return Ok(0);
        }

        let readable_bytes = std::cmp::min(buffer.len() as u64, vmo_size - offset);
        if readable_bytes == 0 {
            return Ok(0);
        }
        self.vmo.read(&mut buffer[..readable_bytes as usize], offset)?;
        Ok(readable_bytes)
    }

    async fn write_at(&self, offset: u64, content: &[u8]) -> Result<u64, Status> {
        // TODO(https://fxbug.dev/479943428): This is a basic WIP implementation, we'll need to
        // expand on this like adding an allocator and journalling.
        self.vmo.write(content, offset)?;

        // We can only write to pre-allocated extents currently.
        if let Some(ref processor) = self.processor {
            processor.overwrite_extents(self.inode as u32, content, offset).map_err(|error| {
                log::warn!(error:?; "Failed to overwrite extents");
                Status::INTERNAL
            })?;
        }
        Ok(content.len() as u64)
    }

    async fn append(&self, _content: &[u8]) -> Result<(u64, u64), Status> {
        // TODO(https://fxbug.dev/479943428): Implement support.
        Err(Status::NOT_SUPPORTED)
    }
}

// Required by `StreamIoConnection`.
impl GetVmo for ExtFile {
    fn get_vmo(&self) -> &Vmo {
        &self.vmo
    }
}

fn get_as_private(vmo: &zx::Vmo, mut rights: zx::Rights) -> Result<Vmo, Status> {
    const CHILD_OPTIONS: zx::VmoChildOptions =
        zx::VmoChildOptions::REFERENCE.union(zx::VmoChildOptions::NO_WRITE);

    // Allow for the child VMO's content size and name to be changed.
    rights |= zx::Rights::SET_PROPERTY;

    let new_vmo = vmo.create_child(CHILD_OPTIONS, 0, 0)?;
    new_vmo.replace_handle(rights)
}

/// Maps VMO flags to their respective rights.
fn vmo_flags_to_rights(vmo_flags: fio::VmoFlags) -> zx::Rights {
    let mut rights = zx::Rights::NONE;
    if vmo_flags.contains(fio::VmoFlags::READ) {
        rights |= zx::Rights::READ;
    }
    if vmo_flags.contains(fio::VmoFlags::WRITE) {
        rights |= zx::Rights::WRITE;
    }
    if vmo_flags.contains(fio::VmoFlags::EXECUTE) {
        rights |= zx::Rights::EXECUTE;
    }
    rights
}

#[cfg(test)]
mod tests {
    use fidl_fuchsia_io::ExtendedAttributeValue;

    use super::*;
    use ext4_read_only::readers::BlockDeviceReader;
    use std::fs;
    use std::path::Path;
    use test_case::test_case;
    use vmo_backed_block_server::{InitialContents, VmoBackedServerOptions};
    use {fidl_fuchsia_storage_block as fblock, fuchsia_async as fasync};

    #[fuchsia::test]
    async fn test_read() {
        let expected_content = b"Read only test";
        let file = ExtFile::readonly_from_data(
            fio::INO_UNKNOWN,
            ExtAttributes::default(),
            XattrMap::default(),
            expected_content,
        )
        .expect("from_data error");
        let proxy = vfs::file::serve_proxy(file, fio::PERM_READABLE);

        let content = proxy
            .read(expected_content.len() as u64)
            .await
            .expect("read FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("read error");
        assert_eq!(content.as_slice(), expected_content);

        proxy
            .close()
            .await
            .expect("close FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("close error");
    }

    #[fuchsia::test]
    async fn test_get_dac_attributes() {
        let file = ExtFile::readonly_from_data(
            123,
            ExtAttributes { mode: 0x8124, uid: 456, gid: 789 },
            XattrMap::default(),
            b"Read only test",
        )
        .expect("from_data error");
        let proxy = vfs::file::serve_proxy(file, fio::PERM_READABLE);

        let attributes_query = fio::NodeAttributesQuery::ID
            | fio::NodeAttributesQuery::MODE
            | fio::NodeAttributesQuery::UID
            | fio::NodeAttributesQuery::GID;
        let (mutable_attributes, immutable_attributes) = proxy
            .get_attributes(attributes_query)
            .await
            .expect("get_attributes FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("get_attributes error");
        assert_eq!(immutable_attributes.id.expect("missing id attribute"), 123);
        assert_eq!(mutable_attributes.mode.expect("missing mode attribute"), 0x8124);
        assert_eq!(mutable_attributes.uid.expect("missing uid attribute"), 456);
        assert_eq!(mutable_attributes.gid.expect("missing gid attribute"), 789);

        proxy
            .close()
            .await
            .expect("close FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("close error");
    }

    #[fuchsia::test]
    async fn test_get_extended_attributes() {
        let xattrs =
            [(b"attr".into(), b"value".into()), (b"attr2".into(), b"value2".into())].into();
        let file =
            ExtFile::readonly_from_data(123, ExtAttributes::default(), xattrs, b"Read only test")
                .expect("from_data error");
        let proxy = vfs::file::serve_proxy(file, fio::PERM_READABLE);

        let value = proxy
            .get_extended_attribute(b"attr2")
            .await
            .expect("get_extended_attribute FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("get_extended_attribute error");
        assert_eq!(value, ExtendedAttributeValue::Bytes(b"value2".into()));

        proxy
            .close()
            .await
            .expect("close FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("close error");
    }

    #[fuchsia::test]
    async fn test_get_backing_memory() {
        let expected_content = b"Read only test";
        let file = ExtFile::readonly_from_data(
            fio::INO_UNKNOWN,
            ExtAttributes::default(),
            XattrMap::default(),
            expected_content,
        )
        .expect("from_data error");
        let proxy = vfs::file::serve_proxy(file, fio::PERM_READABLE);

        async fn assert_get_vmo(
            proxy: &fio::FileProxy,
            flags: fio::VmoFlags,
        ) -> Result<zx::Vmo, Status> {
            proxy
                .get_backing_memory(flags)
                .await
                .expect("get_backing_memory FIDL error")
                .map_err(zx::Status::from_raw)
        }

        fn assert_vmo_content(vmo: &zx::Vmo, expected: &[u8]) {
            let size = vmo.get_content_size().unwrap() as usize;
            assert_eq!(size, expected.len());
            let mut buffer = vec![0; size];
            vmo.read(&mut buffer, 0).unwrap();
            assert_eq!(buffer, expected);
        }

        let vmo = assert_get_vmo(&proxy, fio::VmoFlags::READ).await.unwrap();
        assert_vmo_content(&vmo, expected_content);

        let vmo = assert_get_vmo(&proxy, fio::VmoFlags::READ | fio::VmoFlags::SHARED_BUFFER)
            .await
            .unwrap();
        assert_vmo_content(&vmo, expected_content);

        let vmo = assert_get_vmo(&proxy, fio::VmoFlags::READ | fio::VmoFlags::PRIVATE_CLONE)
            .await
            .unwrap();
        assert_vmo_content(&vmo, expected_content);

        assert_eq!(
            assert_get_vmo(&proxy, fio::VmoFlags::READ | fio::VmoFlags::WRITE).await.unwrap_err(),
            Status::ACCESS_DENIED
        );
        assert_eq!(
            assert_get_vmo(
                &proxy,
                fio::VmoFlags::READ | fio::VmoFlags::WRITE | fio::VmoFlags::SHARED_BUFFER
            )
            .await
            .unwrap_err(),
            Status::ACCESS_DENIED
        );
        assert_eq!(
            assert_get_vmo(
                &proxy,
                fio::VmoFlags::READ | fio::VmoFlags::WRITE | fio::VmoFlags::PRIVATE_CLONE
            )
            .await
            .unwrap_err(),
            Status::ACCESS_DENIED
        );

        proxy
            .close()
            .await
            .expect("close FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("close error");
    }

    #[fuchsia::test]
    async fn test_rw_file() {
        // Create a device that is Ext4 formatted.
        // This image only contains 1 file "file1".
        let data = fs::read("/pkg/data/1file.img").expect("failed to read file");
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
        let processor = Arc::new(Ext4Processor::new(
            Arc::new(
                BlockDeviceReader::from_client_end(block_client_end1)
                    .expect("failed to create block device reader"),
            ),
            /* read_only=*/ false,
        ));

        // Verify that we can't write to a Ext4 RO file.
        let file_ino = processor
            .entry_at_path(Path::new("file1"))
            .expect("failed entry at path")
            .e2d_ino
            .get();
        let ro_file =
            ExtFile::from_processor(processor.clone(), file_ino, /* read_only=*/ true)
                .expect("from_data error");
        let ro_proxy = vfs::file::serve_proxy(ro_file, fio::PERM_READABLE | fio::PERM_WRITABLE);
        ro_proxy.write(b"Write some stuff").await.expect_err("write FIDL request should fail");

        let rw_file = ExtFile::from_processor(processor, file_ino, /* read_only=*/ false)
            .expect("from_data error");
        let proxy = vfs::file::serve_proxy(rw_file, fio::PERM_READABLE | fio::PERM_WRITABLE);

        let expected_content = "file1 contents.\n";
        let content = proxy
            .read(expected_content.len() as u64)
            .await
            .expect("read FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("read error");
        assert_eq!(content.as_slice(), expected_content.as_bytes());

        let write_content = b"Write some stuff";
        let bytes_written = proxy
            .write(write_content)
            .await
            .expect("write FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("write error");
        assert_eq!(bytes_written, write_content.len() as u64);

        proxy
            .close()
            .await
            .expect("close FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("close error");
    }

    #[test_case(true; "read only file")]
    #[test_case(false; "read write file")]
    #[fuchsia::test]
    async fn test_read_past_end_of_file(read_only: bool) {
        let data = fs::read("/pkg/data/1file.img").expect("failed to read file");
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
        let processor = Arc::new(Ext4Processor::new(
            Arc::new(
                BlockDeviceReader::from_client_end(block_client_end1)
                    .expect("failed to create block device reader"),
            ),
            /* read_only=*/ false,
        ));

        let file_ino = processor
            .entry_at_path(Path::new("file1"))
            .expect("failed entry at path")
            .e2d_ino
            .get();
        let file =
            ExtFile::from_processor(processor, file_ino, read_only).expect("from_data error");
        let proxy = vfs::file::serve_proxy(file, fio::PERM_READABLE);

        // Read from start past the end.
        let expected_content = "file1 contents.\n";
        let content = proxy
            .read(1024)
            .await
            .expect("read FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("read error");
        assert_eq!(content.as_slice(), expected_content.as_bytes());

        // Read from exactly at the end.
        let count = 10;
        let read_buf = proxy
            .read_at(count, content.len() as u64)
            .await
            .expect("read_at FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("read_at error");
        assert_eq!(read_buf.len(), 0);

        // Read from past the end.
        let read_buf = proxy
            .read_at(count, content.len() as u64 + 1)
            .await
            .expect("read_at FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("read_at error");
        assert_eq!(read_buf.len(), 0);

        // Read from the middle past the end.
        let offset = 7;
        let read_buf = proxy
            .read_at(count, offset)
            .await
            .expect("read_at FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("read_at error");
        assert_eq!(read_buf.len(), content.len() - offset as usize);

        proxy
            .close()
            .await
            .expect("close FIDL error")
            .map_err(zx::Status::from_raw)
            .expect("close error");
    }
}
