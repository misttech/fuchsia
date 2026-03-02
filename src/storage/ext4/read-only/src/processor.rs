// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::parser::Parser;
use crate::readers::ReaderWriter;
use crate::structs::{FIRST_BG_PADDING, InvalidAddressErrorType, ParsingError};
use std::sync::Arc;

/// A processor that wraps an ext4 parser and adds write functionality if not in read-only mode.
pub struct Ext4Processor {
    fs: Parser,
    reader_writer: Arc<dyn ReaderWriter>,
    read_only: bool,
}

impl std::ops::Deref for Ext4Processor {
    type Target = Parser;

    fn deref(&self) -> &Self::Target {
        &self.fs
    }
}

impl Ext4Processor {
    pub fn new(reader_writer: Arc<dyn ReaderWriter>, read_only: bool) -> Self {
        Self { fs: Parser::new(Box::new(reader_writer.clone())), reader_writer, read_only }
    }

    pub fn read_only(&self) -> bool {
        self.read_only
    }

    /// Writes contiguous raw data starting at a given block number.
    fn write_blocks(&self, block_number: u64, data: &[u8]) -> Result<(), ParsingError> {
        if self.read_only {
            return Err(ParsingError::Incompatible("Cannot write to read-only Ext4".to_string()));
        }
        if block_number == 0 {
            return Err(ParsingError::InvalidAddress(
                InvalidAddressErrorType::Lower,
                0,
                FIRST_BG_PADDING,
            ));
        }

        let block_size = self.block_size()?;
        if data.len() as u64 % block_size != 0 {
            return Err(ParsingError::Incompatible(format!(
                "Data length {} is not a multiple of block size {}",
                data.len(),
                block_size
            )));
        }

        let address = block_number
            .checked_mul(block_size)
            .ok_or(ParsingError::BlockNumberOutOfBounds(block_number))?;

        self.reader_writer.write(address, data)?;

        Ok(())
    }

    /// Overwrites existing contents of a file with new data. This does not require any allocations.
    /// Note that this does not update the journal with timestamps.
    pub fn overwrite_file_contents(
        &self,
        inode_num: u32,
        data: impl AsRef<[u8]>,
        offset: u64,
    ) -> Result<(), ParsingError> {
        if self.read_only {
            return Err(ParsingError::Incompatible("Cannot write to read-only Ext4".to_string()));
        }

        let inode = self.inode(inode_num)?;
        // We don't support allocation and also writing past EOF.
        if offset + data.as_ref().len() as u64 > inode.size() {
            return Err(ParsingError::NotSupported("writing past EOF".to_string()));
        }
        if data.as_ref().len() == 0 {
            return Ok(());
        }

        let root_extent_tree_node = inode.extent_tree_node()?;
        let request = offset..offset + data.as_ref().len() as u64;
        let block_size = self.block_size()?;

        self.iterate_extents_in_tree(&root_extent_tree_node, &mut |extent| {
            let range = (extent.e_blk.get() as u64 * block_size)
                ..((extent.e_blk.get() as u64 + extent.e_len.get() as u64) * block_size);
            let overlap =
                std::cmp::max(range.start, request.start)..std::cmp::min(range.end, request.end);
            if overlap.start >= overlap.end {
                // No overlap.
                return Ok(());
            }

            let mut physical_block_cursor =
                extent.target_block_num() + ((overlap.start - range.start) / block_size);
            let mut current_offset = overlap.start;
            while current_offset < overlap.end {
                let write_buf_cursor = (current_offset - request.start) as usize;
                let block_off = current_offset % block_size;
                let remaining_in_overlap = overlap.end - current_offset;

                if block_off == 0 && remaining_in_overlap >= block_size {
                    // Contiguous full blocks write
                    let full_blocks = remaining_in_overlap / block_size;
                    let write_len = full_blocks * block_size;
                    self.write_blocks(
                        physical_block_cursor,
                        &data.as_ref()[write_buf_cursor..write_buf_cursor + write_len as usize],
                    )?;

                    physical_block_cursor += full_blocks;
                    current_offset += write_len;
                } else {
                    // Write partial block by first reading the existing block and overwriting the
                    // relevant part.
                    let remaining_in_block = block_size - block_off;
                    let write_len = std::cmp::min(remaining_in_block, remaining_in_overlap);
                    let mut block_data = self.block(physical_block_cursor)?.into_vec();
                    block_data[block_off as usize..block_off as usize + write_len as usize]
                        .copy_from_slice(
                            &data.as_ref()[write_buf_cursor..write_buf_cursor + write_len as usize],
                        );
                    self.write_blocks(physical_block_cursor, &block_data)?;

                    physical_block_cursor += 1;
                    current_offset += write_len;
                }
            }
            Ok(())
        })?;

        // TODO(https://fxbug.dev/479943428): Update mtime, ctime, metadata checksum

        Ok(())
    }

    pub fn sync(&self) -> Result<(), ParsingError> {
        self.reader_writer.sync()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readers::{BlockDeviceReader, VecReader};
    use crate::structs::{FIRST_BG_PADDING, InvalidAddressErrorType, ParsingError};
    use std::fs;
    use std::path::Path;
    use vmo_backed_block_server::{InitialContents, VmoBackedServerOptions};
    use zx::Vmo;
    use {fidl_fuchsia_storage_block as fblock, fuchsia_async as fasync};

    #[fuchsia::test]
    async fn test_processor_read_only_blocks_write() {
        let data = fs::read("/pkg/data/1file.img").expect("Unable to read file");
        let read_only_processor = Ext4Processor::new(Arc::new(VecReader::new(data)), true);

        let error = read_only_processor
            .write_blocks(1, &[0u8; 1024])
            .expect_err("passed write_blocks unexpectedly");
        match error {
            ParsingError::Incompatible(_) => {}
            _ => panic!("Expected read-only error"),
        }

        // Test overwrite_file_contents
        let error = read_only_processor
            .overwrite_file_contents(2, &[0u8; 10], 0)
            .expect_err("passed overwrite_file_contents unexpectedly");
        match error {
            ParsingError::Incompatible(_) => {}
            _ => panic!("Expected read-only error"),
        }
    }

    #[fuchsia::test]
    async fn test_processor_write_block_invalid_address() {
        let data = fs::read("/pkg/data/1file.img").expect("Unable to read file");
        let processor = Ext4Processor::new(Arc::new(VecReader::new(data)), false);

        let error =
            processor.write_blocks(0, &[0u8; 1024]).expect_err("passed write_blocks unexpectedly");
        match error {
            ParsingError::InvalidAddress(InvalidAddressErrorType::Lower, 0, FIRST_BG_PADDING) => {}
            _ => panic!("Expected invalid address error, got {:?}", error),
        }
    }

    #[fuchsia::test]
    async fn test_processor_write_block_out_of_bounds() {
        let data = fs::read("/pkg/data/1file.img").expect("Unable to read file");
        let processor = Ext4Processor::new(Arc::new(VecReader::new(data)), false);

        let error = processor
            .write_blocks(u64::MAX, &[0u8; 1024])
            .expect_err("passed write_blocks unexpectedly");
        match error {
            ParsingError::BlockNumberOutOfBounds(u64::MAX) => {}
            _ => panic!("Expected out of bounds error, got {:?}", error),
        }
    }

    #[fuchsia::test]
    async fn test_processor_writeable_overwrite_extents() {
        let data = fs::read("/pkg/data/1file.img").expect("Unable to read file");
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
        let rw_processor = Arc::new(Ext4Processor::new(
            Arc::new(
                BlockDeviceReader::from_client_end(block_client_end1)
                    .expect("failed to create block device reader"),
            ),
            false,
        ));

        let file_ino = rw_processor
            .entry_at_path(Path::new("file1"))
            .expect("failed entry at path")
            .e2d_ino
            .get();

        let mut expected = rw_processor.read_data(file_ino).expect("failed to read data");
        assert_eq!(
            str::from_utf8(expected.as_slice()).expect("failed to read data"),
            "file1 contents.\n"
        );
        let original_size = rw_processor.inode(file_ino).expect("failed to read inode").size();
        assert_eq!(original_size, expected.len() as u64);

        rw_processor
            .overwrite_file_contents(file_ino, &[1u8; 1], 1)
            .expect("failed to overwrite extents");
        expected[1] = 1;

        let new_data = rw_processor.read_data(file_ino).expect("failed to read data");
        assert_eq!(new_data, expected);

        // Test writing to the allocated extent, extending past the original file size (still within
        // the allocated block).
        let error = rw_processor
            .overwrite_file_contents(file_ino, &[1u8; 2], expected.len() as u64 + 2)
            .expect_err("overwrite past EOF should fail");
        match error {
            ParsingError::NotSupported(_) => {}
            _ => panic!("Expected NotSupported error, got {:?}", error),
        }

        // Verify that the file size has not updated.
        let new_size = rw_processor.inode(file_ino).expect("failed to read inode").size();
        assert_eq!(new_size, original_size);
    }
}
