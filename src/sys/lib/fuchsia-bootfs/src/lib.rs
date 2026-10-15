// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod bootfs;

use bootfs::{
    ZBI_BOOTFS_MAGIC, ZBI_BOOTFS_MAX_NAME_LEN, ZBI_BOOTFS_PAGE_SIZE, zbi_bootfs_dirent_t,
    zbi_bootfs_header_t,
};
use std::ffi::CStr;
use std::mem::size_of;
use std::str::Utf8Error;
use thiserror::Error;
use zerocopy::Ref;

const ZBI_BOOTFS_HEADER_SIZE: usize = size_of::<zbi_bootfs_header_t>();

pub fn zbi_bootfs_page_align(size: u32) -> u32 {
    size.wrapping_add(ZBI_BOOTFS_PAGE_SIZE - 1) & !(ZBI_BOOTFS_PAGE_SIZE - 1)
}

pub fn zbi_bootfs_is_aligned(size: u32) -> bool {
    size % ZBI_BOOTFS_PAGE_SIZE == 0
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BootfsParserError {
    #[error("Invalid magic for bootfs payload")]
    BadMagic,

    #[error("Directory entry {} exceeds available dirsize of {}", entry_index, dirsize)]
    DirEntryTooBig { entry_index: u32, dirsize: u32 },

    #[error("Failed to read payload: {}", status)]
    FailedToReadPayload { status: zx::Status },

    #[error("Failed to parse bootfs header")]
    FailedToParseHeader,

    #[error("Failed to parse directory entry")]
    FailedToParseDirEntry,

    #[error("Failed to read name as UTF-8: {}", cause)]
    InvalidNameFormat {
        #[source]
        cause: Utf8Error,
    },

    #[error("Failed to find null terminated string for name: {}", cause)]
    InvalidNameString {
        #[source]
        cause: std::ffi::FromBytesWithNulError,
    },

    #[error(
        "name_len must be between 1 and {}, found {} for directory entry {}",
        max_name_len,
        name_len,
        entry_index
    )]
    InvalidNameLength { name_len: u32, max_name_len: u32, entry_index: u32 },

    #[error("Data offset {} for directory entry {} is not page-aligned", offset, entry_index)]
    UnalignedDataOffset { offset: u32, entry_index: u32 },
}

fn parse_dirent_name(name_bytes: &[u8]) -> Result<&str, BootfsParserError> {
    // Name is stored as an array reference to a block of characters.
    // Characters must be UTF-8 encoded.
    // Valid names are terminated with NUL.
    // We should fail if either of the above conditions are not met.
    match CStr::from_bytes_with_nul(name_bytes) {
        Ok(bytes) => bytes.to_str().map_err(|cause| BootfsParserError::InvalidNameFormat { cause }),
        Err(cause) => Err(BootfsParserError::InvalidNameString { cause }),
    }
}

/// Parser for bootfs-formatted structures.
#[derive(Debug)]
pub struct BootfsParser {
    dir_bytes: Vec<u8>,
}

impl BootfsParser {
    /// Creates a BootfsParser from an existing VMO.
    ///
    /// If `vmo` contains invalid header data, BootfsParserError is returned.
    pub fn create_from_vmo(vmo: &zx::Vmo) -> Result<BootfsParser, BootfsParserError> {
        let header: zbi_bootfs_header_t = vmo
            .read_to_object(0)
            .map_err(|status| BootfsParserError::FailedToReadPayload { status })?;
        if header.magic.get() == ZBI_BOOTFS_MAGIC {
            let dirsize = header.dirsize.get() as u64;
            let vmo_size = vmo
                .get_size()
                .map_err(|status| BootfsParserError::FailedToReadPayload { status })?;
            if dirsize > vmo_size.saturating_sub(ZBI_BOOTFS_HEADER_SIZE as u64) {
                return Err(BootfsParserError::FailedToParseHeader);
            }

            // Read the entire bootfs directory region in a single VMO read so entry iteration can
            // parse directory entries from memory without per-entry syscalls or allocations.
            let dir_bytes = vmo
                .read_to_vec::<u8>(ZBI_BOOTFS_HEADER_SIZE as u64, dirsize)
                .map_err(|status| BootfsParserError::FailedToReadPayload { status })?;
            Ok(Self { dir_bytes })
        } else {
            Err(BootfsParserError::BadMagic)
        }
    }

    pub fn iter<'a>(
        &'a self,
        vmo: &'a zx::Vmo,
    ) -> impl Iterator<Item = Result<BootfsEntry, BootfsParserError>> + 'a {
        BootfsParserIterator::new(self, vmo)
    }

    pub fn zero_copy_iter(
        &self,
    ) -> impl Iterator<Item = Result<ZeroCopyBootfsEntry<'_>, BootfsParserError>> + '_ {
        ZeroCopyBootfsParserIterator::new(self)
    }
}

#[derive(Debug)]
pub struct ZeroCopyBootfsEntry<'a> {
    pub name: &'a str,
    pub offset: u64,
    pub size: u64,
}

#[derive(Debug)]
struct ZeroCopyBootfsParserIterator<'parser> {
    dir_bytes: &'parser [u8],
    entry_index: u32,
}

impl<'parser> ZeroCopyBootfsParserIterator<'parser> {
    pub fn new(parser: &'parser BootfsParser) -> Self {
        Self { dir_bytes: &parser.dir_bytes, entry_index: 0 }
    }

    fn next_entry(&mut self) -> Result<ZeroCopyBootfsEntry<'parser>, BootfsParserError> {
        let dirsize = self.dir_bytes.len() as u32;
        let (dirent, rest) = Ref::<_, zbi_bootfs_dirent_t>::from_prefix(self.dir_bytes)
            .map_err(|_| BootfsParserError::FailedToParseDirEntry)?;

        let name_len = dirent.name_len.get();
        if name_len < 1 || name_len > ZBI_BOOTFS_MAX_NAME_LEN {
            return Err(BootfsParserError::InvalidNameLength {
                entry_index: self.entry_index,
                max_name_len: ZBI_BOOTFS_MAX_NAME_LEN,
                name_len,
            });
        }

        // Each directory entry has a variable size of [16, 268] bytes that must be a multiple of 4
        // bytes. Since zbi_bootfs_dirent_t is 12 bytes (already 4-byte aligned), aligning the entry
        // to 4 bytes is equivalent to aligning name_len to a multiple of 4.
        static_assertions::const_assert_eq!(size_of::<zbi_bootfs_dirent_t>(), 12);
        let padded_name_len = name_len.next_multiple_of(4) as usize;
        let Some((name_and_padding, rest)) = rest.split_at_checked(padded_name_len) else {
            return Err(BootfsParserError::DirEntryTooBig {
                dirsize,
                entry_index: self.entry_index,
            });
        };

        let data_off = dirent.data_off.get();
        if !zbi_bootfs_is_aligned(data_off) {
            return Err(BootfsParserError::UnalignedDataOffset {
                offset: data_off,
                entry_index: self.entry_index,
            });
        }

        let name = parse_dirent_name(&name_and_padding[..(name_len as usize)])?;
        let offset: u64 = data_off.into();
        let size: u64 = dirent.data_len.get().into();

        self.dir_bytes = rest;
        self.entry_index += 1;

        Ok(ZeroCopyBootfsEntry { name, offset, size })
    }
}

impl<'parser> Iterator for ZeroCopyBootfsParserIterator<'parser> {
    type Item = Result<ZeroCopyBootfsEntry<'parser>, BootfsParserError>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.dir_bytes.is_empty() {
            return None;
        }

        let result = self.next_entry();
        if result.is_err() {
            self.dir_bytes = &[];
        }
        Some(result)
    }
}

#[derive(Debug)]
pub struct BootfsEntry {
    pub name: String,
    pub offset: u64,
    pub size: u64,
    pub payload: Vec<u8>,
}

#[derive(Debug)]
struct BootfsParserIterator<'a> {
    vmo: &'a zx::Vmo,
    iter: ZeroCopyBootfsParserIterator<'a>,
}

impl<'a> BootfsParserIterator<'a> {
    pub fn new(parser: &'a BootfsParser, vmo: &'a zx::Vmo) -> Self {
        Self { vmo, iter: ZeroCopyBootfsParserIterator::new(parser) }
    }
}

impl Iterator for BootfsParserIterator<'_> {
    type Item = Result<BootfsEntry, BootfsParserError>;

    fn next(&mut self) -> Option<Self::Item> {
        let entry = self.iter.next()?;
        let result = entry.and_then(|zero_copy_entry| {
            let payload = self
                .vmo
                .read_to_vec::<u8>(zero_copy_entry.offset, zero_copy_entry.size)
                .map_err(|status| BootfsParserError::FailedToReadPayload { status })?;
            Ok(BootfsEntry {
                name: zero_copy_entry.name.to_string(),
                offset: zero_copy_entry.offset,
                size: zero_copy_entry.size,
                payload,
            })
        });
        if result.is_err() {
            self.iter.dir_bytes = &[];
        }
        Some(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Error;
    use std::collections::HashMap;
    use std::fs::File;
    use std::io::prelude::*;
    use std::sync::LazyLock;

    const ZBI_BOOTFS_DIRENT_SIZE: usize = size_of::<zbi_bootfs_dirent_t>();
    static GOLDEN_DIR: &str = "/pkg/data/golden/";
    static BASIC_BOOTFS_UNCOMPRESSED_FILE: &str = "/pkg/data/basic.bootfs.uncompressed";

    fn read_file_into_hashmap(dir: &str, filename: &str, map: &mut HashMap<String, Vec<u8>>) {
        let mut file_buffer = Vec::new();
        let path = format!("{}{}", dir, filename);

        File::open(&path)
            .unwrap_or_else(|e| panic!("Failed to open file {}: {:?}", &path, e))
            .read_to_end(&mut file_buffer)
            .unwrap_or_else(|e| panic!("Failed to read file {}: {:?}", &path, e));
        map.insert(filename.to_string(), file_buffer);
    }

    static GOLDEN_FILES: LazyLock<HashMap<String, Vec<u8>>> = LazyLock::new(|| {
        let mut m = HashMap::new();
        read_file_into_hashmap(GOLDEN_DIR, "dir/empty", &mut m);
        read_file_into_hashmap(GOLDEN_DIR, "dir/lorem.txt", &mut m);
        read_file_into_hashmap(GOLDEN_DIR, "dir/simple-copy.txt", &mut m);
        read_file_into_hashmap(GOLDEN_DIR, "empty", &mut m);
        read_file_into_hashmap(GOLDEN_DIR, "random.dat", &mut m);
        read_file_into_hashmap(GOLDEN_DIR, "simple.txt", &mut m);
        m
    });

    fn read_file_to_vmo(path: &str) -> Result<zx::Vmo, Error> {
        let mut file_buffer = Vec::new();
        File::open(path)?.read_to_end(&mut file_buffer)?;

        let vmo = zx::Vmo::create(file_buffer.len() as u64)?;
        vmo.write(&file_buffer, 0)?;
        Ok(vmo)
    }

    #[test]
    fn dirent_from_raw_fails_on_bad_cstring() {
        // The last byte should be NUL...but it's not for this test.
        let name_bytes = b"oka";
        match parse_dirent_name(name_bytes).unwrap_err() {
            BootfsParserError::InvalidNameString { cause: _cause } => (),
            _ => panic!("parse_dirent_name did not fail with correct error"),
        }
    }

    #[test]
    fn dirent_from_raw_fails_on_non_utf8_string() {
        // This is an invalid UTF-8 sequence.
        let name_bytes = [0xC3, 0x28, b'\0'];

        // Assert that it actually IS an invalid UTF-8 string.
        assert_eq!(true, String::from_utf8(name_bytes.to_vec()).is_err());

        match parse_dirent_name(&name_bytes).unwrap_err() {
            BootfsParserError::InvalidNameFormat { cause: _cause } => (),
            _ => panic!("parse_dirent_name did not fail with correct error"),
        }
    }

    #[test]
    fn create_bootfs_parser() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();
        assert_eq!(true, BootfsParser::create_from_vmo(&vmo).is_ok());
    }

    #[test]
    fn process_basic_bootfs() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();
        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to read bootfs file");

        let mut files = Box::new(HashMap::new());

        parser.iter(&vmo).for_each(|result| {
            let result = result.expect("Failed to process bootfs payload");
            let BootfsEntry { name, payload, .. } = result;
            files.insert(name, payload);
        });

        assert_eq!(*GOLDEN_FILES, *files);
    }

    #[test]
    fn process_bootfs_zero_copy() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();

        let mut files = Box::new(HashMap::new());

        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to read bootfs file");
        parser.zero_copy_iter().for_each(|result| {
            let result = result.expect("Failed to process bootfs payload");
            let ZeroCopyBootfsEntry { name, offset, size } = result;

            let buffer_size = usize::try_from(size).unwrap();
            let mut bytes = vec![0; buffer_size];
            vmo.read(&mut bytes, offset).expect("Failed to read data from the vmo");

            files.insert(name.to_string(), bytes);
        });

        assert_eq!(*GOLDEN_FILES, *files);
    }

    #[test]
    fn process_bootfs_with_invalid_header() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();
        let new_header = [0; ZBI_BOOTFS_HEADER_SIZE];

        // Wipe the header of a known good bootfs payload.
        vmo.write(&new_header, 0).expect("Failed to wipe bootfs header");

        match BootfsParser::create_from_vmo(&vmo).unwrap_err() {
            BootfsParserError::BadMagic => (),
            _ => panic!("BootfsParser::create_from_vmo did not fail with correct error"),
        }
    }

    #[test]
    fn process_bootfs_with_invalid_direntry() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();
        let new_header = [0; ZBI_BOOTFS_DIRENT_SIZE];

        // Wipe the first direntry of a known good bootfs payload.
        // The first direntry starts immediately after the header.
        vmo.write(&new_header, ZBI_BOOTFS_HEADER_SIZE as u64).expect("Failed to wipe direntry");

        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to create BootfsParser");
        parser.iter(&vmo).for_each(|result| match result.unwrap_err() {
            BootfsParserError::InvalidNameLength { entry_index, max_name_len, name_len } => {
                assert_eq!(0, entry_index);
                assert_eq!(ZBI_BOOTFS_MAX_NAME_LEN, max_name_len);
                assert_eq!(0, name_len);
            }
            _ => panic!("parser did not fail with correct error"),
        });
    }

    #[test]
    fn process_bootfs_undersized_dirsize() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();
        let new_header = [(ZBI_BOOTFS_DIRENT_SIZE + 1) as u8, 0, 0, 0];

        // Change dirsize to ZBI_BOOTFS_DIRENT_SIZE+1.
        // It is the second u32 value in the zbi_bootfs_header_t struct.
        vmo.write(&new_header, size_of::<u32>() as u64).expect("Failed to change dirsize");

        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to create BootfsParser");
        parser.iter(&vmo).for_each(|result| match result.unwrap_err() {
            BootfsParserError::DirEntryTooBig { entry_index, dirsize } => {
                assert_eq!(0, entry_index);
                assert_eq!(ZBI_BOOTFS_DIRENT_SIZE + 1, dirsize as usize);
            }
            _ => panic!("parser did not fail with correct error"),
        });
    }

    #[test]
    fn process_bootfs_oversized_dirsize_fails() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();
        let vmo_size = vmo.get_size().unwrap();

        // Change dirsize to exceed the VMO capacity for the directory table (VMO size minus
        // header).
        let oversized_dirsize = (vmo_size - ZBI_BOOTFS_HEADER_SIZE as u64 + 1) as u32;
        vmo.write(&oversized_dirsize.to_le_bytes(), size_of::<u32>() as u64)
            .expect("Failed to change dirsize");

        match BootfsParser::create_from_vmo(&vmo).unwrap_err() {
            BootfsParserError::FailedToParseHeader => (),
            err => panic!("Expected FailedToParseHeader, got: {:?}", err),
        }
    }

    #[test]
    fn process_bootfs_truncated_direntry_fails() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();

        // Change dirsize to fewer than ZBI_BOOTFS_DIRENT_SIZE bytes remaining.
        let truncated_dirsize = (ZBI_BOOTFS_DIRENT_SIZE - 1) as u32;
        vmo.write(&truncated_dirsize.to_le_bytes(), size_of::<u32>() as u64)
            .expect("Failed to change dirsize");

        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to create BootfsParser");
        let mut iter = parser.iter(&vmo);
        match iter.next() {
            Some(Err(BootfsParserError::FailedToParseDirEntry)) => (),
            err => panic!("Expected FailedToParseDirEntry, got: {:?}", err),
        }
        assert!(iter.next().is_none());

        let mut zero_copy_iter = parser.zero_copy_iter();
        match zero_copy_iter.next() {
            Some(Err(BootfsParserError::FailedToParseDirEntry)) => (),
            err => panic!("Expected FailedToParseDirEntry, got: {:?}", err),
        }
        assert!(zero_copy_iter.next().is_none());
    }

    #[test]
    fn process_bootfs_truncated_entry_with_unaligned_offset_fails_with_dir_entry_too_big() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();

        // Change dirsize to ZBI_BOOTFS_DIRENT_SIZE + 1 (entry header plus 1 byte, truncated name).
        let undersized_dirsize = (ZBI_BOOTFS_DIRENT_SIZE + 1) as u32;
        vmo.write(&undersized_dirsize.to_le_bytes(), size_of::<u32>() as u64)
            .expect("Failed to change dirsize");

        // Also change data_off of the first direntry to an unaligned offset.
        let unaligned_offset = ZBI_BOOTFS_PAGE_SIZE + 1;
        let data_off_pos = (ZBI_BOOTFS_HEADER_SIZE + 2 * size_of::<u32>()) as u64;
        vmo.write(&unaligned_offset.to_le_bytes(), data_off_pos)
            .expect("Failed to change data_off");

        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to create BootfsParser");
        let mut iter = parser.iter(&vmo);
        match iter.next() {
            Some(Err(BootfsParserError::DirEntryTooBig { entry_index, dirsize })) => {
                assert_eq!(0, entry_index);
                assert_eq!(undersized_dirsize as usize, dirsize as usize);
            }
            err => panic!("Expected DirEntryTooBig, got: {:?}", err),
        }
        assert!(iter.next().is_none());

        let mut zero_copy_iter = parser.zero_copy_iter();
        match zero_copy_iter.next() {
            Some(Err(BootfsParserError::DirEntryTooBig { entry_index, dirsize })) => {
                assert_eq!(0, entry_index);
                assert_eq!(undersized_dirsize as usize, dirsize as usize);
            }
            err => panic!("Expected DirEntryTooBig, got: {:?}", err),
        }
        assert!(zero_copy_iter.next().is_none());
    }

    #[test]
    fn process_bootfs_unaligned_data_off_fails() {
        let vmo = read_file_to_vmo(BASIC_BOOTFS_UNCOMPRESSED_FILE).unwrap();

        // Change data_off of the first direntry (the third u32 in zbi_bootfs_dirent_t) to an
        // unaligned offset.
        let unaligned_offset = ZBI_BOOTFS_PAGE_SIZE + 1;
        let data_off_pos = (ZBI_BOOTFS_HEADER_SIZE + 2 * size_of::<u32>()) as u64;
        vmo.write(&unaligned_offset.to_le_bytes(), data_off_pos)
            .expect("Failed to change data_off");

        let parser = BootfsParser::create_from_vmo(&vmo).expect("Failed to create BootfsParser");
        parser.iter(&vmo).for_each(|result| match result.unwrap_err() {
            BootfsParserError::UnalignedDataOffset { offset, entry_index } => {
                assert_eq!(unaligned_offset, offset);
                assert_eq!(0, entry_index);
            }
            err => panic!("Expected UnalignedDataOffset, got: {:?}", err),
        });
    }
}
