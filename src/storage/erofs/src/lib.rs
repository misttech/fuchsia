// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! EROFS filesystem.

use bitflags::bitflags;
use crc::{CRC_32_ISCSI, Crc};
use std::sync::Arc;
use thiserror::Error;

pub mod readers;
use readers::{Reader, ReaderError, ReaderExt};

pub mod format;

bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub struct FeatureCompat: u32 {
        /// If this feature is set, the checksum field in the superblock is valid and should be
        /// used to verify the superblock integrity.
        const SB_CHKSUM = 0x00000001;
    }
}

/// Errors that can occur while interacting with an EROFS image.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ErofsError {
    #[error("Unsupported compression algorithms: 0x{:X}", _0)]
    UnsupportedCompressionAlgs(u16),
    #[error("Unsupported feature incompat flags: 0x{:X}. Only 0x{:X} is supported", _0, _1)]
    UnsupportedFeatureIncompat(u32, u32),

    #[error("Parsing error: {}", _0)]
    Parse(#[from] ParsingError),
    #[error("Reader error: {}", _0)]
    ReadError(#[from] ReaderError),
}

#[cfg(target_os = "fuchsia")]
impl ErofsError {
    pub fn to_status(self) -> zx::Status {
        match self {
            Self::UnsupportedCompressionAlgs(_) => zx::Status::NOT_SUPPORTED,
            Self::UnsupportedFeatureIncompat(_, _) => zx::Status::NOT_SUPPORTED,
            Self::Parse(_) => zx::Status::IO_DATA_INTEGRITY,
            Self::ReadError(_) => zx::Status::IO,
        }
    }
}

/// Errors that can occur during parsing of an EROFS image.
#[derive(Debug, Error, Clone, PartialEq)]
pub enum ParsingError {
    #[error("Invalid super block magic: 0x{:X}, should be 0x{:X}", _0, format::EROFS_MAGIC)]
    InvalidSuperBlockMagic(u32),
    #[error("Checksum mismatch: expected 0x{:X}, computed 0x{:X}", _0, _1)]
    ChecksumMismatch(u32, u32),
    #[error("Invalid block size bits: {}, must be between 9 and 12", _0)]
    InvalidBlockSizeBits(u8),

    #[error("Invalid inode data layout: 0x{:X}", _0)]
    InvalidInodeDataLayout(u16),
    #[error("Invalid directory entry")]
    InvalidDirectoryEntry,
    #[error("Invalid file type: {}", _0)]
    InvalidFileType(u8),
    #[error("Directory entry name was not valid utf8")]
    InvalidDirectoryEntryName(#[source] std::str::Utf8Error),
    #[error("Inline data layout missing inline data")]
    InlineDataLayoutMissingInlineData,

    #[error("Invalid root node")]
    InvalidRootNode,
    #[error("Node has an invalid U value for its data layout")]
    InvalidUValue,
    #[error("Invalid nid: {}", _0)]
    InvalidNid(u64),
    #[error("Integer overflow during calculation")]
    Overflow,
}

#[derive(Debug, Clone, Copy)]
enum InodeDataUnion {
    DataBlkAddrPlain(u32),
    DataBlkAddrInline(u32),
}

impl InodeDataUnion {
    fn parse(data: [u8; 4], format: InodeFormat) -> Self {
        match format.data_layout {
            InodeDataLayout::FlatPlain => {
                InodeDataUnion::DataBlkAddrPlain(u32::from_le_bytes(data))
            }
            // Technically this is only valid for inline data where the size is more than a block.
            InodeDataLayout::FlatInline => {
                InodeDataUnion::DataBlkAddrInline(u32::from_le_bytes(data))
            }
        }
    }
}

#[derive(Debug, Clone)]
struct NodeInner {
    inode_offset: u64,
    format: InodeFormat,
    mode: u16,
    size: u64,
    data_union: InodeDataUnion,
    ino: u32,
}

impl NodeInner {
    fn is_dir(&self) -> bool {
        self.mode & 0x4000 != 0
    }

    fn inode_offset(&self) -> u64 {
        self.inode_offset
    }

    /// Interpret the u field as a block address. This is only a valid interpretation on FlatPlain,
    /// or on FlatInline if the size is larger than a block. This debug_asserts that the size is
    /// larger than a block for the inline case to catch programming errors.
    fn blkaddr(&self, block_size: u64) -> u64 {
        match self.data_union {
            InodeDataUnion::DataBlkAddrPlain(addr) => addr.into(),
            InodeDataUnion::DataBlkAddrInline(addr) => {
                debug_assert!(self.size / block_size > 0);
                addr.into()
            }
        }
    }

    /// Safely calculate the on-disk offset for a read in this nodes data. This doesn't check out
    /// of bounds errors.
    fn blkaddr_offset(&self, block_size: u64, offset: u64) -> Result<u64, ParsingError> {
        self.blkaddr(block_size)
            .checked_mul(block_size)
            .ok_or(ParsingError::Overflow)?
            .checked_add(offset)
            .ok_or(ParsingError::Overflow)
    }

    fn metadata_size(&self) -> u64 {
        match self.format.version {
            InodeVersion::Compact => 32,
            InodeVersion::Extended => 64,
        }
    }
}

/// A directory node in the EROFS image.
#[derive(Debug, Clone)]
pub struct DirectoryNode(NodeInner);

impl DirectoryNode {
    pub fn size(&self) -> u64 {
        self.0.size
    }
    pub fn ino(&self) -> u32 {
        self.0.ino
    }
}

/// File type for a directory entry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FileType {
    #[default]
    Unknown = 0,
    RegFile = 1,
    Dir = 2,
    ChrDev = 3,
    BlkDev = 4,
    Fifo = 5,
    Sock = 6,
    Symlink = 7,
}

impl TryFrom<u8> for FileType {
    type Error = ParsingError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        match value {
            0 => Ok(FileType::Unknown),
            1 => Ok(FileType::RegFile),
            2 => Ok(FileType::Dir),
            3 => Ok(FileType::ChrDev),
            4 => Ok(FileType::BlkDev),
            5 => Ok(FileType::Fifo),
            6 => Ok(FileType::Sock),
            7 => Ok(FileType::Symlink),
            _ => Err(ParsingError::InvalidFileType(value)),
        }
    }
}

/// A directory entry in the EROFS image.
#[derive(Debug, Clone, Default)]
pub struct DirectoryEntry {
    pub nid: u64,
    pub file_type: FileType,
    pub name: String,
}

/// A file node in the EROFS image.
#[derive(Debug, Clone)]
pub struct FileNode(NodeInner);

impl FileNode {
    pub fn size(&self) -> u64 {
        self.0.size
    }
    pub fn ino(&self) -> u32 {
        self.0.ino
    }
}

/// A node in the EROFS image.
#[derive(Debug, Clone)]
pub enum Node {
    Directory(DirectoryNode),
    File(FileNode),
}

impl Node {
    fn new(inner: NodeInner) -> Self {
        if inner.is_dir() {
            Node::Directory(DirectoryNode(inner))
        } else {
            Node::File(FileNode(inner))
        }
    }

    fn parse_compact(
        inode_offset: u64,
        format: InodeFormat,
        inode: format::InodeCompact,
    ) -> Result<Self, ParsingError> {
        let data_union = InodeDataUnion::parse(inode.i_u, format);
        Ok(Self::new(NodeInner {
            inode_offset,
            format,
            mode: inode.mode.get(),
            size: inode.size.get().into(),
            data_union,
            ino: inode.ino.get(),
        }))
    }

    fn parse_extended(
        inode_offset: u64,
        format: InodeFormat,
        inode: format::InodeExtended,
    ) -> Result<Self, ParsingError> {
        let data_union = InodeDataUnion::parse(inode.i_u, format);
        Ok(Self::new(NodeInner {
            inode_offset,
            format,
            mode: inode.mode.get(),
            size: inode.size.get(),
            data_union,
            ino: inode.ino.get(),
        }))
    }

    fn from_nid(nid: u64, meta_addr: u64, reader: &dyn Reader) -> Result<Self, ErofsError> {
        let node_offset =
            nid.checked_mul(format::INODE_SLOT_SIZE).ok_or(ParsingError::InvalidNid(nid))?;
        let inode_offset =
            meta_addr.checked_add(node_offset).ok_or(ParsingError::InvalidNid(nid))?;
        // Read the first 2 bytes to determine the inode format.
        let mut head = [0u8; 2];
        reader.read(inode_offset, &mut head)?;
        let format = InodeFormat::parse(u16::from_le_bytes(head))?;
        let node = match format.version {
            InodeVersion::Compact => {
                Self::parse_compact(inode_offset, format, reader.read_object(inode_offset)?)?
            }
            InodeVersion::Extended => {
                Self::parse_extended(inode_offset, format, reader.read_object(inode_offset)?)?
            }
        };
        Ok(node)
    }

    pub fn size(&self) -> u64 {
        match self {
            Node::Directory(node) => node.size(),
            Node::File(node) => node.size(),
        }
    }

    pub fn ino(&self) -> u32 {
        match self {
            Node::Directory(node) => node.ino(),
            Node::File(node) => node.ino(),
        }
    }
}

/// The filesystem implementation for an EROFS image.
pub struct ErofsFilesystem {
    reader: Arc<dyn Reader>,
    block_size: u64,
    meta_addr: u64,
    root_node: DirectoryNode,
}

impl ErofsFilesystem {
    /// Creates a new filesystem instance for an EROFS image from a reader.
    pub fn new(reader: Arc<dyn Reader>) -> Result<Self, ErofsError> {
        let super_block = Self::parse_superblock(&reader)?;
        let block_size = 1u64 << super_block.block_size_bits;
        let meta_block_addr = super_block.meta_block_addr.get().into();
        let meta_addr = block_size.checked_mul(meta_block_addr).ok_or(ParsingError::Overflow)?;
        let root_nid = super_block.root_nid.get().into();
        let root_node = match Node::from_nid(root_nid, meta_addr, &reader)? {
            Node::Directory(node) => node,
            _ => return Err(ParsingError::InvalidRootNode.into()),
        };
        Ok(Self { reader, block_size, meta_addr, root_node })
    }

    fn parse_superblock(reader: &dyn Reader) -> Result<format::SuperBlock, ErofsError> {
        let sb: format::SuperBlock = reader.read_object(format::SUPERBLOCK_OFFSET)?;
        if sb.magic.get() != format::EROFS_MAGIC {
            return Err(ParsingError::InvalidSuperBlockMagic(sb.magic.get()).into());
        }
        // The max block size that can be made by tooling is 4096 right now, and the specified
        // minimum is 512, so make sure we are in that window.
        if sb.block_size_bits < 9 || sb.block_size_bits > 12 {
            return Err(ParsingError::InvalidBlockSizeBits(sb.block_size_bits).into());
        }
        // TODO(https://fxbug.dev/479841115): Handle more feature_compat flags.
        let feature_compat = FeatureCompat::from_bits_truncate(sb.feature_compat.get());
        if feature_compat.contains(FeatureCompat::SB_CHKSUM) {
            Self::check_superblock_checksum(reader, &sb)?;
        }
        // TODO(https://fxbug.dev/479841115): Handle feature_incompat flags.
        if sb.feature_incompat.get() != 0 {
            return Err(ErofsError::UnsupportedFeatureIncompat(sb.feature_incompat.get(), 0));
        }
        // TODO(https://fxbug.dev/479841115): Support compression. Validate we support all the
        // listed compression algorithms when we do.
        if sb.available_compr_algs.get() != 0 {
            return Err(ErofsError::UnsupportedCompressionAlgs(sb.available_compr_algs.get()));
        }
        Ok(sb)
    }

    fn check_superblock_checksum(
        reader: &dyn Reader,
        sb: &format::SuperBlock,
    ) -> Result<(), ErofsError> {
        let block_size = 1usize << sb.block_size_bits;
        let len = block_size - (format::SUPERBLOCK_OFFSET as usize) % block_size;
        let mut buf = vec![0u8; len];
        reader.read(format::SUPERBLOCK_OFFSET, &mut buf)?;

        // Zero out checksum field, which is at a well-known offset off the superblock offset.
        buf[4..8].copy_from_slice(&[0u8; 4]);

        let crc = Crc::<u32>::new(&CRC_32_ISCSI);
        let checksum = crc.checksum(&buf);
        // Undo final bitwise inversion applied by the crc crate, as suggested by the EROFS docs
        // (https://erofs.docs.kernel.org/en/latest/ondisk/core_ondisk.html#superblock-checksum)
        let checksum = !checksum;

        if checksum != sb.checksum.get() {
            Err(ParsingError::ChecksumMismatch(sb.checksum.get(), checksum).into())
        } else {
            Ok(())
        }
    }

    /// Returns the block size of the EROFS image.
    pub fn block_size(&self) -> u64 {
        self.block_size
    }

    /// Returns the node with the given nid.
    pub fn node(&self, nid: u64) -> Result<Node, ErofsError> {
        Node::from_nid(nid, self.meta_addr, &self.reader)
    }

    /// Returns the root node of the EROFS image.
    pub fn root_node(&self) -> DirectoryNode {
        self.root_node.clone()
    }

    /// Reads the data of the given file node into a buffer.
    pub fn read_file_range(
        &self,
        node: &FileNode,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, ErofsError> {
        self.read_node_range(&node.0, offset, buf)
    }

    /// Read bytes from the node's data at an offset. The length of the read is determined by the
    /// length of the provided output buf. The data is written into that buf. Returns the number of
    /// bytes read.
    ///
    /// TODO(https://fxbug.dev/479841115): This is a traditional unix-y way of handling reads -
    /// potentially reading less data than asked for - but we should determine whether that fits
    /// our apis and tweak it if needed.
    fn read_node_range(
        &self,
        node: &NodeInner,
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize, ErofsError> {
        if offset >= node.size {
            return Ok(0);
        }
        let read_len = std::cmp::min(buf.len() as u64, node.size - offset) as usize;
        let buf = &mut buf[..read_len];
        let block_size = self.block_size();

        match node.format.data_layout {
            InodeDataLayout::FlatPlain => {
                let read_offset = node.blkaddr_offset(block_size, offset)?;
                self.reader.read(read_offset, buf)?;
                Ok(read_len)
            }
            InodeDataLayout::FlatInline => {
                // A node will _only_ have the flat inline layout if it has a tail that that fits
                // inline after the inode, so we can assume any tail data is there.
                let full_blocks_len = (node.size / block_size) * block_size;
                let mut bytes_read = 0;

                if offset < full_blocks_len {
                    // If there are no full blocks and the full file is in the tail section, this
                    // check will never be true, so this is a valid use of the u value.
                    let current_read_len =
                        std::cmp::min(read_len as u64, full_blocks_len - offset) as usize;
                    let read_offset = node.blkaddr_offset(block_size, offset)?;
                    self.reader.read(read_offset, &mut buf[..current_read_len])?;
                    bytes_read += current_read_len;
                }

                if bytes_read < read_len {
                    let remaining_len = read_len - bytes_read;
                    let current_offset = offset + bytes_read as u64;
                    // TODO(https://fxbug.dev/479841115): figure out how xattrs fit into this.
                    let inline_data_offset = node
                        .inode_offset()
                        .checked_add(node.metadata_size())
                        .ok_or(ParsingError::Overflow)?;
                    let tail_offset = current_offset - full_blocks_len;
                    let tail_read_offset = inline_data_offset
                        .checked_add(tail_offset)
                        .ok_or(ParsingError::Overflow)?;
                    self.reader.read(tail_read_offset, &mut buf[bytes_read..])?;
                    bytes_read += remaining_len;
                }

                Ok(bytes_read)
            }
        }
    }

    /// Read a number of entries from a directory, starting at entry_offset. Will retrieve up to
    /// the number of entries in the directory or the size of the provided buffer, returning the
    /// number of entries filled in the buffer. If there are less filled entries then the number of
    /// entry slots provided in the buffer, there are no more entries in this directory. Entries
    /// are sorted lexicographically. Reads past the end of the number of entries will return zero
    /// entries filled.
    ///
    /// TODO(https://fxbug.dev/479841115): It is possible for directories to omit their "." entries
    /// in erofs, and in that case there is a flag marking it and we are expected to synthesize it.
    /// Parse that flag and implement it.
    /// TODO(https://fxbug.dev/479841115): This API is slightly awkward to hold. We should consider
    /// making it an iterator interface.
    pub fn read_directory(
        &self,
        node: &DirectoryNode,
        mut entry_offset: usize,
        entries: &mut [DirectoryEntry],
    ) -> Result<usize, ErofsError> {
        let block_size = self.block_size();
        let block_size_usize: usize = block_size as usize;
        let mut entries_filled = 0;
        let mut current_entry_index = 0;
        let mut block_data = vec![0u8; block_size_usize];

        for block in 0.. {
            let base_offset = block * block_size;
            let bytes_read = self.read_node_range(&node.0, base_offset, &mut block_data)?;
            if bytes_read < format::DIRENT_SIZE {
                // We must be done if there wasn't enough data left for another dirent.
                return Ok(entries_filled);
            }
            block_data[bytes_read..].fill(0);

            // Get the first dirent in the block to calculate the number of entries.
            let (dirent0, _) = zerocopy::Ref::<&[u8], format::Dirent>::from_prefix(&block_data)
                .map_err(|_| ParsingError::InvalidDirectoryEntry)?;
            let nameoff0 = dirent0.nameoff.get() as usize;
            if nameoff0 < format::DIRENT_SIZE || nameoff0 >= block_size_usize {
                return Err(ParsingError::InvalidDirectoryEntry.into());
            }
            let entry_count = nameoff0 / format::DIRENT_SIZE;

            // Check if the offset we want is even in this block.
            if current_entry_index + entry_count <= entry_offset {
                current_entry_index += entry_count;
                continue;
            }

            // Get all the dirents and make sure the nameoffs won't cause out of bounds errors.
            let dirents_raw = block_data
                .get(..entry_count * format::DIRENT_SIZE)
                .ok_or(ParsingError::InvalidDirectoryEntry)?;
            let dirents: &[format::Dirent] =
                &*zerocopy::Ref::<&[u8], [format::Dirent]>::from_bytes(dirents_raw)
                    .map_err(|_| ParsingError::InvalidDirectoryEntry)?;

            let block_entry_offset = entry_offset - current_entry_index;
            let space = entries.len() - entries_filled;
            let block_entry_end = std::cmp::min(
                entry_count,
                block_entry_offset.checked_add(space).ok_or(ParsingError::Overflow)?,
            );

            for i in block_entry_offset..block_entry_end {
                let last_entry = i + 1 == entry_count;
                let nameoff = dirents[i].nameoff.get() as usize;

                let name_bytes = if last_entry {
                    // For the last entry, it ends at the end of the block or is null-terminated.
                    // Since block_data is padded with nulls, we can just split by 0.
                    let name_data =
                        block_data.get(nameoff..).ok_or(ParsingError::InvalidDirectoryEntry)?;
                    name_data.split(|&x| x == 0).next().unwrap()
                } else {
                    let nameoff_next = dirents[i + 1].nameoff.get() as usize;
                    block_data
                        .get(nameoff..nameoff_next)
                        .ok_or(ParsingError::InvalidDirectoryEntry)?
                };

                let name = std::str::from_utf8(name_bytes)
                    .map_err(|e| ParsingError::InvalidDirectoryEntryName(e))?
                    .to_string();
                entries[entries_filled] = DirectoryEntry {
                    nid: dirents[i].nid.get(),
                    file_type: dirents[i].file_type.try_into()?,
                    name,
                };
                entries_filled += 1;
                if entries_filled == entries.len() {
                    return Ok(entries_filled);
                }
            }

            current_entry_index =
                current_entry_index.checked_add(entry_count).ok_or(ParsingError::Overflow)?;
            entry_offset = current_entry_index;
        }

        Ok(entries_filled)
    }

    /// Looks up a node by name in a directory.
    pub fn lookup(&self, dir: &DirectoryNode, name: &str) -> Result<Option<Node>, ErofsError> {
        let mut entry_offset = 0;
        let mut buffer = vec![DirectoryEntry::default(); 16];

        loop {
            let filled = self.read_directory(dir, entry_offset, &mut buffer)?;
            for i in 0..filled {
                if buffer[i].name == name {
                    let node = self.node(buffer[i].nid)?;
                    return Ok(Some(node));
                }
            }
            if filled < buffer.len() {
                break;
            }
            entry_offset += filled;
        }

        Ok(None)
    }
}

/// The version of the on-disk format of the inode. Can be either 32-byte compact or 64-byte
/// extended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeVersion {
    Compact,
    Extended,
}

/// The layout of the data portion of the inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeDataLayout {
    /// The data union is interpreted as a block address. The data for this inode is stored in
    /// consecutive blocks starting from that block address.
    FlatPlain,
    /// The data union is interpreted as a block address. The data for this inode is stored in
    /// consecutive blocks starting from that block address, except for the tail of the data which
    /// is stored immediately following this metadata. If the whole tail is inlined, the data union
    /// is unused and doesn't matter. For this to be used, the data _must_ have a tail section that
    /// fits within the current metadata block.
    FlatInline,
}

/// The format of the inode, containing the version and data layout.
#[derive(Debug, Clone, Copy)]
pub struct InodeFormat {
    pub version: InodeVersion,
    pub data_layout: InodeDataLayout,
}

impl InodeFormat {
    /// Parse the inode format from the given format value.
    pub fn parse(format: u16) -> Result<Self, ParsingError> {
        let version =
            if format & 0x1 == 0 { InodeVersion::Compact } else { InodeVersion::Extended };
        let data_layout_raw = (format >> 1) & 0x7;
        let data_layout = match data_layout_raw {
            0 => InodeDataLayout::FlatPlain,
            2 => InodeDataLayout::FlatInline,
            _ => return Err(ParsingError::InvalidInodeDataLayout(data_layout_raw)),
        };
        Ok(Self { version, data_layout })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::readers::VecReader;
    use std::fs;
    use test_case::test_case;

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_parse_superblock(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles.clone()));
        // The fs validates the superblock during construction.
        let _fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");

        // Now mutate a byte in the superblock. This ensures the checksumming is actually happening
        // and getting evaluated correctly.
        let mut mutated_runfiles = runfiles.clone();
        mutated_runfiles[1088] ^= 0xFF;

        let reader = Arc::new(VecReader::new(mutated_runfiles));
        let fs = ErofsFilesystem::new(reader);
        assert!(fs.is_err());
        match fs.err().unwrap() {
            ErofsError::Parse(ParsingError::ChecksumMismatch(_, _)) => {}
            e => panic!("Expected ChecksumMismatch error, got {:?}", e),
        }
    }

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_list_dir(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        let mut buf = vec![DirectoryEntry::default(); 16];
        let filled = fs.read_directory(&root_node, 0, &mut buf).expect("failed to read directory");

        let names: Vec<String> = buf[..filled].iter().map(|e| e.name.clone()).collect();
        assert_eq!(names, vec![".", "..", "file1", "large_dir", "photosynthesis", "quantum"]);
    }

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_overflow_nid(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let result = fs.node(u64::MAX);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err(), ErofsError::Parse(ParsingError::InvalidNid(u64::MAX)));
    }

    #[test_case("/pkg/data/simple.erofs", "file1" ; "4096 block size file1")]
    #[test_case("/pkg/data/simple_512.erofs", "file1" ; "512 block size file1")]
    #[test_case("/pkg/data/simple.erofs", "photosynthesis" ; "4096 block size photosynthesis")]
    #[test_case("/pkg/data/simple_512.erofs", "photosynthesis" ; "512 block size photosynthesis")]
    #[fuchsia::test]
    fn test_read_file_range(path: &str, name: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        let node = fs.lookup(&root_node, name).expect("failed to lookup").expect("file not found");
        let file_node = match node {
            Node::File(f) => f,
            _ => panic!("Expected file node"),
        };

        let size = file_node.size() as usize;
        let mut buf = vec![0u8; size];
        let bytes_read = fs.read_file_range(&file_node, 0, &mut buf).expect("failed to read");
        assert_eq!(bytes_read, size);
        if name == "file1" {
            assert_eq!(&buf[..14], b"this is a file");
        }

        // Test partial read within file
        let mut buf = vec![0u8; 5];
        let bytes_read = fs.read_file_range(&file_node, 5, &mut buf).expect("failed to read");
        assert_eq!(bytes_read, 5);
        if name == "file1" {
            assert_eq!(&buf, b"is a ");
        }

        // Test read spanning across EOF (buffer larger than remaining data)
        let mut buf = vec![0u8; 100];
        let bytes_read =
            fs.read_file_range(&file_node, (size - 5) as u64, &mut buf).expect("failed to read");
        assert_eq!(bytes_read, 5);
        if name == "file1" {
            assert_eq!(&buf[..5], b"file\n");
        }

        // Test read at EOF
        let mut buf = vec![0u8; 100];
        let bytes_read =
            fs.read_file_range(&file_node, size as u64, &mut buf).expect("failed to read");
        assert_eq!(bytes_read, 0);
    }

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_read_directory_pagination(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        let expected_names = vec![".", "..", "file1", "large_dir", "photosynthesis", "quantum"];

        // Test reading with buffer size 2 (pagination)
        let mut buf = vec![DirectoryEntry::default(); 2];

        // Page 1 (offset 0)
        let filled = fs.read_directory(&root_node, 0, &mut buf).expect("failed to read dir");
        assert_eq!(filled, 2);
        assert_eq!(buf[0].name, expected_names[0]);
        assert_eq!(buf[1].name, expected_names[1]);

        // Page 2 (offset 2)
        let filled = fs.read_directory(&root_node, 2, &mut buf).expect("failed to read dir");
        assert_eq!(filled, 2);
        assert_eq!(buf[0].name, expected_names[2]);
        assert_eq!(buf[1].name, expected_names[3]);

        // Page 4 (offset 5)
        let filled = fs.read_directory(&root_node, 5, &mut buf).expect("failed to read dir");
        assert_eq!(filled, 1);
        assert_eq!(buf[0].name, expected_names[5]);

        // Page 5 (offset 6 - EOF)
        let filled = fs.read_directory(&root_node, 6, &mut buf).expect("failed to read dir");
        assert_eq!(filled, 0);

        // Test reading with buffer size 1 (extreme pagination)
        let mut buf1 = vec![DirectoryEntry::default(); 1];
        for i in 0..expected_names.len() {
            let filled = fs.read_directory(&root_node, i, &mut buf1).expect("failed to read dir");
            assert_eq!(filled, 1);
            assert_eq!(buf1[0].name, expected_names[i]);
        }
        let filled = fs
            .read_directory(&root_node, expected_names.len(), &mut buf1)
            .expect("failed to read dir");
        assert_eq!(filled, 0);
    }

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_read_directory_large_dir(path: &str) {
        // Note: the large directory in the golden image is only large enough to split the entries
        // into multiple blocks on the 512 block size golden.
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        let large_dir_node = fs
            .lookup(&root_node, "large_dir")
            .expect("failed to look up large_dir")
            .expect("large_dir not found");

        let large_dir = match large_dir_node {
            Node::Directory(d) => d,
            _ => panic!("Expected directory node"),
        };

        // Skip the first two entries, . and ..
        let mut entry_offset = 2;
        let mut buffer = vec![DirectoryEntry::default(); 16];
        loop {
            let filled = fs.read_directory(&large_dir, entry_offset, &mut buffer).unwrap();
            for i in 0..filled {
                // check the prefix
                assert_eq!(buffer[i].name[..12], format!("file_number_"));
            }
            if filled < buffer.len() {
                break;
            }
            entry_offset += filled;
        }
    }
}
