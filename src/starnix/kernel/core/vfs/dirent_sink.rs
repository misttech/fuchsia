// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use smallvec::SmallVec;
use std::mem;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

use crate::mm::MemoryAccessor;
use crate::task::CurrentTask;
use crate::vfs::FsStr;
use starnix_uapi::errors::{ENOSPC, Errno};
use starnix_uapi::file_mode::FileMode;
use starnix_uapi::math::round_up_to_increment;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{NAME_MAX, error, ino_t, off_t};

#[derive(Debug, Default, Copy, Clone, PartialOrd, Ord, PartialEq, Eq)]
pub struct DirectoryEntryType(u8);

// These values are defined in libc.
impl DirectoryEntryType {
    pub const UNKNOWN: DirectoryEntryType = DirectoryEntryType(0);
    pub const FIFO: DirectoryEntryType = DirectoryEntryType(1);
    pub const CHR: DirectoryEntryType = DirectoryEntryType(2);
    pub const DIR: DirectoryEntryType = DirectoryEntryType(4);
    pub const BLK: DirectoryEntryType = DirectoryEntryType(6);
    pub const REG: DirectoryEntryType = DirectoryEntryType(8);
    pub const LNK: DirectoryEntryType = DirectoryEntryType(10);
    pub const SOCK: DirectoryEntryType = DirectoryEntryType(12);

    pub fn from_bits(bits: u8) -> DirectoryEntryType {
        Self(bits)
    }

    pub fn from_mode(mode: FileMode) -> DirectoryEntryType {
        match mode.fmt() {
            FileMode::IFLNK => DirectoryEntryType::LNK,
            FileMode::IFREG => DirectoryEntryType::REG,
            FileMode::IFDIR => DirectoryEntryType::DIR,
            FileMode::IFCHR => DirectoryEntryType::CHR,
            FileMode::IFBLK => DirectoryEntryType::BLK,
            FileMode::IFIFO => DirectoryEntryType::FIFO,
            FileMode::IFSOCK => DirectoryEntryType::SOCK,
            _ => DirectoryEntryType::UNKNOWN,
        }
    }

    pub fn bits(&self) -> u8 {
        self.0
    }
}

const DIRENT64_PADDING_SIZE: usize = 5;

#[repr(C)]
#[derive(Debug, Default, Copy, Clone, IntoBytes, KnownLayout, FromBytes, Immutable)]
struct DirentHeader64 {
    d_ino: u64,
    d_off: i64,
    d_reclen: u16,
    d_type: u8,
    padding: [u8; DIRENT64_PADDING_SIZE],
}

const DIRENT64_HEADER_SIZE: usize = mem::size_of::<DirentHeader64>() - DIRENT64_PADDING_SIZE;

// Large enough for a 32-bit or 64-bit dirent whose name is at most NAME_MAX bytes.
const DIRENT_BUFFER_SIZE: usize =
    (DIRENT64_HEADER_SIZE + NAME_MAX as usize + 1).next_multiple_of(8);
type DirentBuffer = SmallVec<[u8; DIRENT_BUFFER_SIZE]>;

pub trait DirentSink {
    /// Add the given directory entry to this buffer.
    ///
    /// In case of success, this will update the offset from the FileObject. Any other bookkeeping
    /// must be done by the caller after this method returns successfully.
    ///
    /// Returns error!(ENOSPC) if the entry does not fit.
    fn add(
        &mut self,
        inode_num: ino_t,
        offset: off_t,
        entry_type: DirectoryEntryType,
        name: &FsStr,
    ) -> Result<(), Errno>;

    /// The current offset to return.
    fn offset(&self) -> off_t;

    /// Optional information about the max number of bytes to send to the user.
    fn user_capacity(&self) -> Option<usize> {
        None
    }
}

struct BaseDirentSink<'a> {
    current_task: &'a CurrentTask,
    offset: &'a mut off_t,
    user_buffer: UserAddress,
    user_capacity: usize,
    actual: usize,
}

impl<'a> BaseDirentSink<'a> {
    fn add(&mut self, offset: off_t, buffer: &[u8]) -> Result<(), Errno> {
        if self.user_capacity().is_none_or(|cap| self.actual + buffer.len() > cap) {
            return error!(ENOSPC);
        }
        self.current_task.write_memory((self.user_buffer + self.actual)?, buffer)?;
        self.actual += buffer.len();
        *self.offset = offset;
        Ok(())
    }

    fn offset(&self) -> off_t {
        *self.offset
    }

    // Validate `i32::MAX` lazily here rather than upfront in `sys_getdents`/`sys_getdents64` so
    // `EBADF`, `ENOENT`, `EACCES`, and `ENOTDIR` checks in `FileObject::readdir` run first.
    fn user_capacity(&self) -> Option<usize> {
        if self.user_capacity > i32::MAX as usize { None } else { Some(self.user_capacity) }
    }

    // Converts result value received from `add()` to `getdents()` result.
    //
    // `add()` may return `ENOSPC` if there was not enough space in the buffer for the new entry.
    // This error should be returned to the caller only if we didn't have space for the first
    // directory entry.
    //
    // We use `ENOSPC` rather than `EINVAL` to signal this condition because `EINVAL` is a very
    // generic error. We only want to perform this transformation in exactly the case where there
    // was not sufficient space in the DirentSink.
    fn map_result_with_actual(&self, result: Result<(), Errno>) -> Result<usize, Errno> {
        match result {
            Ok(()) => Ok(self.actual),
            Err(_) if self.actual > 0 => Ok(self.actual),
            Err(errno) if errno == ENOSPC => error!(EINVAL),
            Err(e) => Err(e),
        }
    }
}

pub struct DirentSink64<'a> {
    base: BaseDirentSink<'a>,
}

impl<'a> DirentSink64<'a> {
    pub fn new(
        current_task: &'a CurrentTask,
        offset: &'a mut off_t,
        user_buffer: UserAddress,
        user_capacity: usize,
    ) -> Self {
        Self {
            base: BaseDirentSink { current_task, offset, user_buffer, user_capacity, actual: 0 },
        }
    }

    pub fn map_result_with_actual(&self, result: Result<(), Errno>) -> Result<usize, Errno> {
        self.base.map_result_with_actual(result)
    }
}

impl DirentSink for DirentSink64<'_> {
    fn add(
        &mut self,
        inode_num: ino_t,
        offset: off_t,
        entry_type: DirectoryEntryType,
        name: &FsStr,
    ) -> Result<(), Errno> {
        let content_size = DIRENT64_HEADER_SIZE + name.len();
        let entry_size = round_up_to_increment(content_size + 1, 8)?; // +1 for the null terminator.
        let mut buffer = DirentBuffer::with_capacity(entry_size);
        let header = DirentHeader64 {
            d_ino: inode_num,
            d_off: offset,
            d_reclen: entry_size as u16,
            d_type: entry_type.bits(),
            ..DirentHeader64::default()
        };
        let header_bytes = header.as_bytes();
        buffer.extend_from_slice(&header_bytes[..DIRENT64_HEADER_SIZE]);
        buffer.extend_from_slice(name);
        buffer.resize(buffer.len() + (entry_size - content_size), b'\0');
        assert_eq!(buffer.len(), entry_size);
        self.base.add(offset, &buffer)
    }

    fn offset(&self) -> off_t {
        self.base.offset()
    }

    fn user_capacity(&self) -> Option<usize> {
        self.base.user_capacity()
    }
}

#[cfg(target_arch = "x86_64")]
pub use x86_64::*;

#[cfg(target_arch = "x86_64")]
mod x86_64 {
    use super::{BaseDirentSink, DirectoryEntryType, DirentBuffer, DirentSink};
    use crate::task::CurrentTask;
    use crate::vfs::FsStr;
    use starnix_uapi::errors::Errno;
    use starnix_uapi::math::round_up_to_increment;
    use starnix_uapi::user_address::UserAddress;
    use starnix_uapi::{NAME_MAX, ino_t, off_t};
    use std::mem;
    use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

    const DIRENT32_PADDING_SIZE: usize = 6;
    const DIRENT32_HEADER_SIZE: usize = mem::size_of::<DirentHeader32>() - DIRENT32_PADDING_SIZE;

    const _: () = assert!(
        (DIRENT32_HEADER_SIZE + NAME_MAX as usize + 2).next_multiple_of(8)
            <= super::DIRENT_BUFFER_SIZE
    );

    #[repr(C)]
    #[derive(Debug, Default, Copy, Clone, IntoBytes, KnownLayout, FromBytes, Immutable)]
    struct DirentHeader32 {
        d_ino: u64,
        d_off: i64,
        d_reclen: u16,
        padding: [u8; DIRENT32_PADDING_SIZE],
        // pad: u8, // Zero padding byte
        // d_type: u8, // File type
    }

    pub struct DirentSink32<'a> {
        base: BaseDirentSink<'a>,
    }

    impl<'a> DirentSink32<'a> {
        pub fn new(
            current_task: &'a CurrentTask,
            offset: &'a mut off_t,
            user_buffer: UserAddress,
            user_capacity: usize,
        ) -> Self {
            Self {
                base: BaseDirentSink {
                    current_task,
                    offset,
                    user_buffer,
                    user_capacity,
                    actual: 0,
                },
            }
        }

        pub fn map_result_with_actual(&self, result: Result<(), Errno>) -> Result<usize, Errno> {
            self.base.map_result_with_actual(result)
        }
    }

    impl DirentSink for DirentSink32<'_> {
        fn add(
            &mut self,
            inode_num: ino_t,
            offset: off_t,
            entry_type: DirectoryEntryType,
            name: &FsStr,
        ) -> Result<(), Errno> {
            let content_size = DIRENT32_HEADER_SIZE + name.len();
            // +1 for the null terminator, +1 for the type.
            let entry_size = round_up_to_increment(content_size + 2, 8)?;
            let mut buffer = DirentBuffer::with_capacity(entry_size);
            let header = DirentHeader32 {
                d_ino: inode_num,
                d_off: offset,
                d_reclen: entry_size as u16,
                ..DirentHeader32::default()
            };
            let header_bytes = header.as_bytes();
            buffer.extend_from_slice(&header_bytes[..DIRENT32_HEADER_SIZE]);
            buffer.extend_from_slice(name);
            buffer.resize(buffer.len() + (entry_size - content_size - 1), b'\0');
            buffer.push(entry_type.bits()); // Include the type.
            assert_eq!(buffer.len(), entry_size);
            self.base.add(offset, &buffer)
        }

        fn offset(&self) -> off_t {
            self.base.offset()
        }

        fn user_capacity(&self) -> Option<usize> {
            self.base.user_capacity()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mm::{MemoryAccessorExt, PAGE_SIZE};
    use crate::testing::{map_memory, spawn_kernel_and_run};

    const SHORT_RECLEN: usize = 24;
    const LONG_RECLEN: usize = 288;

    #[::fuchsia::test]
    fn dirent_buffer_inline_capacity() {
        let max_entry =
            round_up_to_increment(DIRENT64_HEADER_SIZE + NAME_MAX as usize + 1, 8).unwrap();
        assert!(!DirentBuffer::with_capacity(max_entry).spilled());
        assert!(DirentBuffer::with_capacity(max_entry + 8).spilled());
    }

    #[::fuchsia::test]
    async fn test_dirent_sink64_add() {
        spawn_kernel_and_run(async |current_task| {
            let user_buffer = map_memory(current_task, UserAddress::default(), *PAGE_SIZE);
            let mut offset = 0;
            let mut sink =
                DirentSink64::new(current_task, &mut offset, user_buffer, *PAGE_SIZE as usize);

            // Short entry (fits in stack buffer, padded with zeros to 8-byte alignment).
            sink.add(42, 1, DirectoryEntryType::REG, "a".into()).expect("add short entry");
            assert_eq!(sink.offset(), 1);

            // Max standard filename (NAME_MAX = 255 -> 280-byte entry, exact stack buffer size).
            let max_name = vec![b'x'; NAME_MAX as usize];
            sink.add(43, 2, DirectoryEntryType::DIR, max_name.as_slice().into())
                .expect("add NAME_MAX entry");
            assert_eq!(sink.offset(), 2);

            // Oversized filename (> NAME_MAX -> exercises heap fallback).
            let long_name = vec![b'y'; NAME_MAX as usize + 10];
            sink.add(44, 3, DirectoryEntryType::LNK, long_name.as_slice().into())
                .expect("add oversized entry");
            assert_eq!(sink.offset(), 3);

            let total_written = sink.map_result_with_actual(Ok(())).expect("map_result");
            assert_eq!(total_written, SHORT_RECLEN + DIRENT_BUFFER_SIZE + LONG_RECLEN);

            let bytes = current_task.read_memory_to_vec(user_buffer, total_written).expect("read");

            let (hdr1, _) = DirentHeader64::read_from_prefix(&bytes[..SHORT_RECLEN]).unwrap();
            assert_eq!(hdr1.d_ino, 42);
            assert_eq!(hdr1.d_off, 1);
            assert_eq!(hdr1.d_reclen, SHORT_RECLEN as u16);
            assert_eq!(hdr1.d_type, DirectoryEntryType::REG.bits());
            assert_eq!(&bytes[DIRENT64_HEADER_SIZE..SHORT_RECLEN], b"a\0\0\0\0");

            let entry2 = &bytes[SHORT_RECLEN..SHORT_RECLEN + DIRENT_BUFFER_SIZE];
            let (hdr2, _) = DirentHeader64::read_from_prefix(entry2).unwrap();
            assert_eq!(hdr2.d_ino, 43);
            assert_eq!(hdr2.d_off, 2);
            assert_eq!(hdr2.d_reclen, DIRENT_BUFFER_SIZE as u16);
            assert_eq!(hdr2.d_type, DirectoryEntryType::DIR.bits());
            assert_eq!(
                &entry2[DIRENT64_HEADER_SIZE..DIRENT64_HEADER_SIZE + max_name.len()],
                &max_name[..]
            );
            assert!(entry2[DIRENT64_HEADER_SIZE + max_name.len()..].iter().all(|&b| b == 0));

            let entry3 = &bytes[SHORT_RECLEN + DIRENT_BUFFER_SIZE..];
            let (hdr3, _) = DirentHeader64::read_from_prefix(entry3).unwrap();
            assert_eq!(hdr3.d_ino, 44);
            assert_eq!(hdr3.d_off, 3);
            assert_eq!(hdr3.d_reclen, LONG_RECLEN as u16);
            assert_eq!(hdr3.d_type, DirectoryEntryType::LNK.bits());
            assert_eq!(
                &entry3[DIRENT64_HEADER_SIZE..DIRENT64_HEADER_SIZE + long_name.len()],
                &long_name[..]
            );
            assert!(entry3[DIRENT64_HEADER_SIZE + long_name.len()..].iter().all(|&b| b == 0));
        })
        .await;
    }
}
