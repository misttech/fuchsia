// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! EROFS filesystem.

use bitflags::bitflags;
use crc::{CRC_32_ISCSI, Crc};
use std::sync::Arc;
use thiserror::Error;
use zerocopy::IntoBytes;
use zerocopy::byteorder::little_endian::U32 as LEU32;

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
    #[error("Missing shared xattr area but inode has shared xattrs")]
    MissingSharedXattrArea,
    #[error("Xattr entry extends past the end of the inline xattr region")]
    XattrEntryOutOfBounds,
    #[error("Invalid xattr namespace index: {}", _0)]
    InvalidXattrNamespace(u8),
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
pub struct NodeInner {
    inode_offset: u64,
    format: InodeFormat,
    mode: u16,
    size: u64,
    data_union: InodeDataUnion,
    ino: u32,
    nid: u64,
    link_count: u32,
    uid: u32,
    gid: u32,
    mtime_ns: u64,
    xattr_icount: u16,
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

    fn inline_xattr_size(&self) -> u64 {
        if self.xattr_icount == 0 { 0 } else { ((self.xattr_icount as u64 - 1) * 4) + 12 }
    }

    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn ino(&self) -> u32 {
        self.ino
    }
    pub fn nid(&self) -> u64 {
        self.nid
    }
    pub fn link_count(&self) -> u32 {
        self.link_count
    }
    pub fn uid(&self) -> u32 {
        self.uid
    }
    pub fn gid(&self) -> u32 {
        self.gid
    }
    pub fn mtime_ns(&self) -> u64 {
        self.mtime_ns
    }
    pub fn mode(&self) -> u16 {
        self.mode
    }
}

/// A directory node in the EROFS image.
#[derive(Debug, Clone)]
pub struct DirectoryNode(NodeInner);

impl std::ops::Deref for DirectoryNode {
    type Target = NodeInner;
    fn deref(&self) -> &Self::Target {
        &self.0
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

impl std::ops::Deref for FileNode {
    type Target = NodeInner;
    fn deref(&self) -> &Self::Target {
        &self.0
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
        nid: u64,
        inode_offset: u64,
        format: InodeFormat,
        inode: format::InodeCompact,
        build_time_ns: u64,
    ) -> Result<Self, ParsingError> {
        let data_union = InodeDataUnion::parse(inode.i_u, format);
        Ok(Self::new(NodeInner {
            inode_offset,
            format,
            mode: inode.mode.get(),
            size: inode.size.get().into(),
            data_union,
            ino: inode.ino.get(),
            nid,
            link_count: inode.link_count.get().into(),
            uid: inode.uid.get().into(),
            gid: inode.gid.get().into(),
            mtime_ns: build_time_ns,
            xattr_icount: inode.xattr_icount.get(),
        }))
    }

    fn parse_extended(
        nid: u64,
        inode_offset: u64,
        format: InodeFormat,
        inode: format::InodeExtended,
    ) -> Result<Self, ParsingError> {
        let data_union = InodeDataUnion::parse(inode.i_u, format);
        let mtime_ns = inode
            .mtime
            .get()
            .checked_mul(1_000_000_000)
            .and_then(|t| t.checked_add(inode.mtime_ns.get().into()))
            .ok_or(ParsingError::Overflow)?;
        Ok(Self::new(NodeInner {
            inode_offset,
            format,
            mode: inode.mode.get(),
            size: inode.size.get(),
            data_union,
            ino: inode.ino.get(),
            nid,
            link_count: inode.link_count.get(),
            uid: inode.uid.get(),
            gid: inode.gid.get(),
            mtime_ns,
            xattr_icount: inode.xattr_icount.get(),
        }))
    }

    fn from_nid(
        nid: u64,
        meta_addr: u64,
        build_time_ns: u64,
        reader: &dyn Reader,
    ) -> Result<Self, ErofsError> {
        let node_offset =
            nid.checked_mul(format::INODE_SLOT_SIZE).ok_or(ParsingError::InvalidNid(nid))?;
        let inode_offset =
            meta_addr.checked_add(node_offset).ok_or(ParsingError::InvalidNid(nid))?;
        // Read the first 2 bytes to determine the inode format.
        let mut head = [0u8; 2];
        reader.read(inode_offset, &mut head)?;
        let format = InodeFormat::parse(u16::from_le_bytes(head))?;
        let node = match format.version {
            InodeVersion::Compact => Self::parse_compact(
                nid,
                inode_offset,
                format,
                reader.read_object(inode_offset)?,
                build_time_ns,
            )?,
            InodeVersion::Extended => {
                Self::parse_extended(nid, inode_offset, format, reader.read_object(inode_offset)?)?
            }
        };
        Ok(node)
    }
}

impl std::ops::Deref for Node {
    type Target = NodeInner;
    fn deref(&self) -> &Self::Target {
        match self {
            Node::Directory(d) => d,
            Node::File(f) => f,
        }
    }
}

/// The filesystem implementation for an EROFS image.
pub struct ErofsFilesystem {
    reader: Arc<dyn Reader>,
    block_size: u64,
    meta_addr: u64,
    xattr_addr: u64,
    root_node: DirectoryNode,
    total_bytes: u64,
    total_inodes: u64,
    build_time_ns: u64,
}

impl ErofsFilesystem {
    /// Creates a new filesystem instance for an EROFS image from a reader.
    pub fn new(reader: Arc<dyn Reader>) -> Result<Self, ErofsError> {
        let super_block = Self::parse_superblock(&reader)?;
        let block_size = 1u64 << super_block.block_size_bits;
        let meta_block_addr = super_block.meta_block_addr.get().into();
        let meta_addr = block_size.checked_mul(meta_block_addr).ok_or(ParsingError::Overflow)?;
        let total_inodes = super_block.inode_count.get();
        let build_time_ns = super_block
            .epoch
            .get()
            .checked_mul(1_000_000_000)
            .and_then(|t| t.checked_add(super_block.fixed_nsec.get().into()))
            .ok_or(ParsingError::Overflow)?;
        let total_bytes = (super_block.blocks.get() as u64) * block_size;
        let xattr_block_addr = super_block.xattr_block_addr.get().into();
        let xattr_addr = block_size.checked_mul(xattr_block_addr).ok_or(ParsingError::Overflow)?;
        let root_nid = super_block.root_nid.get().into();
        let root_node = match Node::from_nid(root_nid, meta_addr, build_time_ns, &reader)? {
            Node::Directory(node) => node,
            _ => return Err(ParsingError::InvalidRootNode.into()),
        };
        Ok(Self {
            reader,
            block_size,
            meta_addr,
            xattr_addr,
            root_node,
            total_bytes,
            total_inodes,
            build_time_ns,
        })
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
        Node::from_nid(nid, self.meta_addr, self.build_time_ns, &self.reader)
    }

    /// Returns the root node of the EROFS image.
    pub fn root_node(&self) -> DirectoryNode {
        self.root_node.clone()
    }

    pub fn total_bytes(&self) -> u64 {
        self.total_bytes
    }

    pub fn total_inodes(&self) -> u64 {
        self.total_inodes
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
                    let inline_xattr_size = node.inline_xattr_size();
                    let inline_data_offset = node
                        .inode_offset()
                        .checked_add(node.metadata_size())
                        .ok_or(ParsingError::Overflow)?
                        .checked_add(inline_xattr_size)
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

    /// Returns an iterator over the xattr entry headers for a node.
    pub fn iter_xattrs<'a>(&'a self, node: &NodeInner) -> Result<XattrIterator<'a>, ErofsError> {
        if node.xattr_icount == 0 {
            return Ok(XattrIterator {
                reader: self.reader.as_ref(),
                xattr_addr: self.xattr_addr,
                shared_ids: Vec::new(),
                inline_offset: 0,
                inline_end: 0,
            });
        }
        let xattr_metadata_size = node.inline_xattr_size();
        let xattr_metadata_start =
            node.inode_offset().checked_add(node.metadata_size()).ok_or(ParsingError::Overflow)?;

        // Read the inline xattr header to get the details on the extended attributes for this node
        let header: format::XattrInlineBodyHeader =
            self.reader.read_object(xattr_metadata_start)?;
        let shared_count = header.shared_count as usize;

        let shared_ids_size = shared_count as u64 * 4;
        let inline_entries_start = xattr_metadata_start + 12 + shared_ids_size;
        let inline_end = xattr_metadata_start + xattr_metadata_size;

        if inline_entries_start > inline_end {
            return Err(ParsingError::XattrEntryOutOfBounds.into());
        }

        let shared_ids = if shared_count > 0 {
            let mut ids = vec![LEU32::ZERO; shared_count];
            self.reader.read(xattr_metadata_start + 12, ids.as_mut_bytes())?;
            ids
        } else {
            Vec::new()
        };

        Ok(XattrIterator {
            reader: self.reader.as_ref(),
            xattr_addr: self.xattr_addr,
            shared_ids,
            inline_offset: inline_entries_start,
            inline_end,
        })
    }

    /// List all xattr names for a given node.
    pub fn list_xattrs(&self, node: &NodeInner) -> Result<Vec<Vec<u8>>, ErofsError> {
        let mut names = Vec::new();
        for entry in self.iter_xattrs(node)? {
            let entry = entry?;
            names.push(entry.read_name(self.reader.as_ref())?);
        }
        Ok(names)
    }

    /// Get the value of a specific xattr for a given node.
    pub fn get_xattr(&self, node: &NodeInner, name: &[u8]) -> Result<Option<Vec<u8>>, ErofsError> {
        for entry in self.iter_xattrs(node)? {
            let entry = entry?;
            if entry.matches_name(self.reader.as_ref(), name)? {
                return Ok(Some(entry.read_value(self.reader.as_ref())?));
            }
        }
        Ok(None)
    }
}

/// An iterator over xattr entry headers for an inode.
pub struct XattrIterator<'a> {
    reader: &'a dyn Reader,
    xattr_addr: u64,
    shared_ids: Vec<LEU32>,
    inline_offset: u64,
    inline_end: u64,
}

impl XattrIterator<'_> {
    fn next_inner(&mut self) -> Result<Option<XattrEntryHeader>, ErofsError> {
        if let Some(shared_id) = self.shared_ids.pop() {
            let shared_entry_offset = self.xattr_addr + (shared_id.get() as u64 * 4);
            if self.shared_ids.is_empty() {
                self.shared_ids = Vec::new();
            }
            return Ok(Some(XattrEntryHeader::parse(self.reader, shared_entry_offset)?));
        }

        if self.inline_offset < self.inline_end {
            if self.inline_offset + 4 > self.inline_end {
                return Err(ParsingError::XattrEntryOutOfBounds.into());
            }

            let header = XattrEntryHeader::parse(self.reader, self.inline_offset)?;
            let next_offset = self
                .inline_offset
                .checked_add(header.entry_aligned_size)
                .ok_or(ParsingError::Overflow)?;
            if next_offset > self.inline_end {
                return Err(ParsingError::XattrEntryOutOfBounds.into());
            }
            self.inline_offset = next_offset;
            Ok(Some(header))
        } else {
            Ok(None)
        }
    }
}

impl Iterator for XattrIterator<'_> {
    type Item = Result<XattrEntryHeader, ErofsError>;

    fn next(&mut self) -> Option<Self::Item> {
        match self.next_inner() {
            // Throw out the rest of the values if we encounter an error parsing the extended
            // attributes. Since most of the errors are related to overflows and math issues, there
            // is no safe way to recover for future attributes as the locations on disk are all
            // relative to each other.
            Err(e) => {
                self.shared_ids = Vec::new();
                self.inline_offset = self.inline_end;
                Some(Err(e))
            }
            Ok(None) => None,
            Ok(Some(x)) => Some(Ok(x)),
        }
    }
}

/// A parsed representation of an EROFS xattr entry record header.
#[derive(Debug, Clone, Copy)]
pub struct XattrEntryHeader {
    pub offset: u64,
    pub prefix: &'static [u8],
    pub name_index: u8,
    pub name_len: usize,
    pub value_size: usize,
    pub entry_aligned_size: u64,
}

impl XattrEntryHeader {
    /// Read and validate an xattr entry record header from the reader.
    pub fn parse(reader: &dyn Reader, offset: u64) -> Result<Self, ErofsError> {
        let entry: format::XattrEntry = reader.read_object(offset)?;
        let prefix = Self::get_xattr_prefix(entry.name_index)?;
        let name_len = entry.name_len as usize;
        let value_size = entry.value_size.get() as usize;

        let entry_aligned_size = 4usize
            .checked_add(name_len)
            .and_then(|s| s.checked_add(value_size))
            .and_then(|s| s.checked_next_multiple_of(4))
            .ok_or(ParsingError::Overflow)? as u64;

        Ok(Self {
            offset,
            prefix,
            name_index: entry.name_index,
            name_len,
            value_size,
            entry_aligned_size,
        })
    }

    /// Check if this xattr entry matches the given full attribute name (prefix + suffix).
    pub fn matches_name(&self, reader: &dyn Reader, name: &[u8]) -> Result<bool, ReaderError> {
        let Some(suffix) = name.strip_prefix(self.prefix) else {
            return Ok(false);
        };
        if suffix.len() != self.name_len {
            return Ok(false);
        }
        if self.name_len == 0 {
            // Implies suffix.len() is also zero because of the previous check.
            return Ok(true);
        }
        let mut buf = vec![0u8; self.name_len];
        reader.read(self.offset + 4, &mut buf)?;
        Ok(buf == suffix)
    }

    /// Read the name of this xattr entry (prefix + suffix).
    pub fn read_name(&self, reader: &dyn Reader) -> Result<Vec<u8>, ReaderError> {
        let mut name_bytes = Vec::with_capacity(self.prefix.len() + self.name_len);
        name_bytes.extend_from_slice(self.prefix);
        if self.name_len > 0 {
            name_bytes.resize(self.prefix.len() + self.name_len, 0);
            reader.read(self.offset + 4, &mut name_bytes[self.prefix.len()..])?;
        }
        Ok(name_bytes)
    }

    /// Read the value payload for this entry.
    pub fn read_value(&self, reader: &dyn Reader) -> Result<Vec<u8>, ReaderError> {
        let mut value_bytes = vec![0u8; self.value_size];
        reader.read(self.offset + 4 + self.name_len as u64, &mut value_bytes)?;
        Ok(value_bytes)
    }

    /// Read both key name and value payload for this entry.
    pub fn read_payload(&self, reader: &dyn Reader) -> Result<(Vec<u8>, Vec<u8>), ReaderError> {
        let name = self.read_name(reader)?;
        let value = self.read_value(reader)?;
        Ok((name, value))
    }

    fn get_xattr_prefix(index: u8) -> Result<&'static [u8], ParsingError> {
        match index {
            1 => Ok(b"user."),
            2 => Ok(b"system.posix_acl_access"),
            3 => Ok(b"system.posix_acl_default"),
            4 => Ok(b"trusted."),
            6 => Ok(b"security."),
            _ => Err(ParsingError::InvalidXattrNamespace(index)),
        }
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
    use zerocopy::byteorder::little_endian::{U16 as LEU16, U32 as LEU32, U64 as LEU64};

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

        assert_eq!(fs.get_xattr(&root_node, b"security.selinux").unwrap(), None);
        let selinux_val = fs.get_xattr(&large_dir, b"security.selinux").unwrap().unwrap();
        assert_eq!(
            selinux_val,
            b"u:object_r:very_long_selinux_context_exceeding_the_inline_limit_of_two_hundred_and_fifty_six_bytes_and_requiring_the_use_of_extended_attributes_instead_of_returning_the_context_inline_in_the_node_attributes_table_representation_as_dictated_by_the_fuchsia_io_node_fidl_specification:s0"
        );
        let file1_node = fs.lookup(&large_dir, "file_number_1").unwrap().unwrap();
        let file1_selinux = fs.get_xattr(&file1_node, b"security.selinux").unwrap().unwrap();
        assert_eq!(
            file1_selinux,
            b"u:object_r:very_long_selinux_context_exceeding_the_inline_limit_of_two_hundred_and_fifty_six_bytes_and_requiring_the_use_of_extended_attributes_instead_of_returning_the_context_inline_in_the_node_attributes_table_representation_as_dictated_by_the_fuchsia_io_node_fidl_specification:s0"
        );

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

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_filesystem_metadata(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");

        assert!(fs.total_bytes() > 0);
        assert!(fs.total_inodes() > 0);
    }

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_node_metadata(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        assert!(root_node.link_count() >= 2);
        assert!(root_node.mtime_ns() > 0);

        let file1_node = fs.lookup(&root_node, "file1").unwrap().unwrap();
        assert_eq!(file1_node.link_count(), 1);
        assert!(file1_node.mtime_ns() > 0);
    }

    #[test_case("/pkg/data/simple.erofs" ; "4096 block size")]
    #[test_case("/pkg/data/simple_512.erofs" ; "512 block size")]
    #[fuchsia::test]
    fn test_xattrs(path: &str) {
        let runfiles = fs::read(path).expect("failed to read test file");
        let reader = Arc::new(VecReader::new(runfiles));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        // Check file1 (has both inline and shared xattrs)
        let file1_node = fs.lookup(&root_node, "file1").unwrap().unwrap();

        let xattr_names = fs.list_xattrs(&file1_node).unwrap();
        // Should contain user.flavor, user.security, user.shared, security.selinux
        assert!(xattr_names.contains(&b"user.flavor".to_vec()));
        assert!(xattr_names.contains(&b"user.security".to_vec()));
        assert!(xattr_names.contains(&b"user.shared".to_vec()));
        assert!(xattr_names.contains(&b"security.selinux".to_vec()));
        assert_eq!(xattr_names.len(), 4);

        let flavor_val = fs.get_xattr(&file1_node, b"user.flavor").unwrap().unwrap();
        assert_eq!(flavor_val, b"vanilla");

        let security_val = fs.get_xattr(&file1_node, b"user.security").unwrap().unwrap();
        assert_eq!(security_val, b"high");

        let shared_val = fs.get_xattr(&file1_node, b"user.shared").unwrap().unwrap();
        assert_eq!(shared_val, b"same_value");

        let selinux_val = fs.get_xattr(&file1_node, b"security.selinux").unwrap().unwrap();
        assert_eq!(selinux_val, b"u:object_r:file1_t:s0");

        // Check photosynthesis (has only shared xattr)
        let photo_node = fs.lookup(&root_node, "photosynthesis").unwrap().unwrap();

        let photo_xattrs = fs.list_xattrs(&photo_node).unwrap();
        assert_eq!(photo_xattrs, vec![b"user.shared".to_vec()]);

        let photo_shared_val = fs.get_xattr(&photo_node, b"user.shared").unwrap().unwrap();
        assert_eq!(photo_shared_val, b"same_value");
        assert_eq!(fs.get_xattr(&photo_node, b"security.selinux").unwrap(), None);

        // Check quantum (has no xattrs)
        let quantum_node = fs.lookup(&root_node, "quantum").unwrap().unwrap();
        assert_eq!(fs.get_xattr(&quantum_node, b"security.selinux").unwrap(), None);

        // Verify that we can still read the file content of photosynthesis
        let file_node = match photo_node {
            Node::File(f) => f,
            _ => panic!("Expected file node"),
        };
        let size = file_node.size() as usize;
        let mut buf = vec![0u8; size];
        let bytes_read = fs.read_file_range(&file_node, 0, &mut buf).expect("failed to read");
        assert_eq!(bytes_read, size);
        assert!(size > 0);

        // Check non-existent xattr
        let val = fs.get_xattr(&file1_node, b"user.non_existent").unwrap();
        assert_eq!(val, None);
    }

    // EROFS doesn't seem to make shared xattr groups with simple xattrs like the one above (I
    // assume it has some heuristics for judging when it is worth the cost) so this test manually
    // constructs a shared xattr area to test that part of the parsing logic.
    #[fuchsia::test]
    fn test_shared_xattr_parsing() {
        let block_size = 4096usize;
        let mut buf = vec![0u8; 3 * block_size];

        // Superblock at offset 1024
        let sb = format::SuperBlock {
            magic: LEU32::new(format::EROFS_MAGIC),
            checksum: LEU32::new(0),
            feature_compat: LEU32::new(0),
            block_size_bits: 12,
            sb_ext_slots: 0,
            root_nid: LEU16::new(0),
            inode_count: LEU64::new(1),
            epoch: LEU64::new(0),
            fixed_nsec: LEU32::new(0),
            blocks: LEU32::new(3),
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
        buf[1024..1024 + 128].copy_from_slice(sb.as_bytes());

        // Compact Inode at meta_block_addr (block 1, offset 4096)
        let inode = format::InodeCompact {
            format: LEU16::new(0),
            xattr_icount: LEU16::new(2),
            mode: LEU16::new(0o040755),
            link_count: LEU16::new(1),
            size: LEU32::new(0),
            reserved_1: [0; 4],
            i_u: [0; 4],
            ino: LEU32::new(0),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        buf[4096..4096 + 32].copy_from_slice(inode.as_bytes());

        // XattrInlineBodyHeader at offset 4128
        let header = format::XattrInlineBodyHeader {
            name_filter: LEU32::new(0),
            shared_count: 1,
            reserved: [0; 7],
        };
        buf[4128..4128 + 12].copy_from_slice(header.as_bytes());
        // shared_id index 512 (512 * 4 = offset 2048 in block 0) at offset 4140
        buf[4140..4144].copy_from_slice(&512u32.to_le_bytes());

        // Shared Xattr Entry at xattr_block_addr (block 0, offset 2048)
        let xentry = format::XattrEntry {
            name_len: 6,
            name_index: 1, // "user."
            value_size: LEU16::new(10),
        };
        buf[2048..2052].copy_from_slice(xentry.as_bytes());
        buf[2052..2058].copy_from_slice(b"shared");
        buf[2058..2068].copy_from_slice(b"same_value");

        let reader = Arc::new(VecReader::new(buf));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        let xattr_names = fs.list_xattrs(&root_node).expect("failed to list xattrs");
        assert_eq!(xattr_names, vec![b"user.shared".to_vec()]);

        let shared_val =
            fs.get_xattr(&root_node, b"user.shared").expect("failed to get xattr").unwrap();
        assert_eq!(shared_val, b"same_value");
    }

    #[fuchsia::test]
    fn test_xattr_iterator_fusing_on_error() {
        let block_size = 4096usize;
        let mut buf = vec![0u8; 3 * block_size];

        let sb = format::SuperBlock {
            magic: LEU32::new(format::EROFS_MAGIC),
            checksum: LEU32::new(0),
            feature_compat: LEU32::new(0),
            block_size_bits: 12,
            sb_ext_slots: 0,
            root_nid: LEU16::new(0),
            inode_count: LEU64::new(1),
            epoch: LEU64::new(0),
            fixed_nsec: LEU32::new(0),
            blocks: LEU32::new(3),
            meta_block_addr: LEU32::new(1),
            xattr_block_addr: LEU32::new(2),
            uuid: [0; 16],
            volume_name: [0; 16],
            feature_incompat: LEU32::new(0),
            available_compr_algs: LEU16::new(0),
            extra_devices: LEU32::new(0),
            dirblkbits: 0,
            reserved: [0; 37],
        };
        buf[1024..1024 + 128].copy_from_slice(sb.as_bytes());

        let inode = format::InodeCompact {
            format: LEU16::new(0),
            xattr_icount: LEU16::new(3),
            mode: LEU16::new(0o040755),
            link_count: LEU16::new(1),
            size: LEU32::new(0),
            reserved_1: [0; 4],
            i_u: [0; 4],
            ino: LEU32::new(0),
            uid: LEU16::new(0),
            gid: LEU16::new(0),
            reserved_2: [0; 4],
        };
        buf[4096..4096 + 32].copy_from_slice(inode.as_bytes());

        let header = format::XattrInlineBodyHeader {
            name_filter: LEU32::new(0),
            shared_count: 2,
            reserved: [0; 7],
        };
        buf[4128..4128 + 12].copy_from_slice(header.as_bytes());
        buf[4140..4144].copy_from_slice(&0u32.to_le_bytes());
        buf[4144..4148].copy_from_slice(&1u32.to_le_bytes());

        // Invalid shared xattr entry (invalid name_index 99) at xattr_block_addr for shared_id 0
        // (offset 8192)
        let invalid_xentry =
            format::XattrEntry { name_len: 6, name_index: 99, value_size: LEU16::new(10) };
        buf[8192..8196].copy_from_slice(invalid_xentry.as_bytes());

        let reader = Arc::new(VecReader::new(buf));
        let fs = ErofsFilesystem::new(reader).expect("failed to parse superblock");
        let root_node = fs.root_node();

        let mut iter = fs.iter_xattrs(&root_node).expect("failed to create iterator");
        assert!(iter.next().unwrap().is_err());
        // Iterator MUST be fused now: subsequent next() calls MUST return None
        assert!(iter.next().is_none());
    }
}
