// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use smallvec::{SmallVec, smallvec};
use starnix_core::mm::{MemoryAccessor, MemoryAccessorExt};
use starnix_uapi::errors::Errno;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{errno, error};
use zerocopy::FromBytes;

const INLINE_CAPACITY: usize = 256;

/// Allows for sequential reading of a task's userspace memory.
pub struct UserMemoryCursor {
    buffer: SmallVec<[u8; INLINE_CAPACITY]>,
    bytes_read: usize,
}

impl UserMemoryCursor {
    /// Create a new [`UserMemoryCursor`] starting at userspace address `addr` of length `len`.
    /// Upon creation, the cursor reads the entire user buffer then caches it.
    /// Any reads past `addr + len` will fail with `EINVAL`.
    pub fn new(ma: &dyn MemoryAccessor, addr: UserAddress, len: u64) -> Result<Self, Errno> {
        let mut buffer = smallvec![0u8; len as usize];
        ma.read_memory_to_slice(addr, &mut buffer)?;
        Ok(Self { buffer, bytes_read: 0 })
    }

    /// Increment the read position.
    #[inline]
    pub fn advance(&mut self, length: u64) -> Result<(), Errno> {
        let length = length as usize;
        let remaining = self.buffer.len() - self.bytes_read;
        if length > remaining {
            return error!(EINVAL);
        }
        self.bytes_read += length;
        Ok(())
    }

    /// Read an object from userspace memory and increment the read position.
    #[inline]
    pub fn read_object<T: FromBytes>(&mut self) -> Result<T, Errno> {
        let slice = &self.buffer[self.bytes_read..];
        let (obj, _) = T::read_from_prefix(slice).map_err(|_| errno!(EINVAL))?;
        self.bytes_read += std::mem::size_of::<T>();
        Ok(obj)
    }

    /// The total number of bytes read.
    #[inline]
    pub fn bytes_read(&self) -> usize {
        self.bytes_read
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::mm::PAGE_SIZE;
    use starnix_core::testing::{map_memory_anywhere, spawn_kernel_and_run};
    use zerocopy::IntoBytes;

    #[fuchsia::test]
    async fn user_memory_cursor_inline_and_bounds() {
        spawn_kernel_and_run(async |current_task| {
            let addr = map_memory_anywhere(current_task, *PAGE_SIZE);
            let payload: [u32; 4] = [0x11223344, 0x55667788, 0x99aabbcc, 0xddeeff00];
            current_task.write_memory(addr, payload.as_bytes()).expect("write payload");

            let mut cursor =
                UserMemoryCursor::new(current_task, addr, std::mem::size_of_val(&payload) as u64)
                    .expect("create cursor");
            assert!(!cursor.buffer.spilled());
            assert_eq!(cursor.bytes_read(), 0);

            assert_eq!(cursor.read_object::<u32>().expect("read first u32"), 0x11223344);
            assert_eq!(cursor.bytes_read(), 4);

            cursor.advance(8).expect("advance 8 bytes");
            assert_eq!(cursor.bytes_read(), 12);

            // Reading an 8-byte u64 when only 4 bytes remain must fail without advancing.
            assert_eq!(cursor.read_object::<u64>(), error!(EINVAL));
            assert_eq!(cursor.bytes_read(), 12);

            // Advancing past the remaining 4 bytes must fail without advancing.
            assert_eq!(cursor.advance(5), error!(EINVAL));
            assert_eq!(cursor.advance(u64::MAX), error!(EINVAL));
            assert_eq!(cursor.bytes_read(), 12);

            // Reading the final u32 succeeds and reaches the end of the cursor.
            assert_eq!(cursor.read_object::<u32>().expect("read last u32"), 0xddeeff00);
            assert_eq!(cursor.bytes_read(), 16);

            // At the end of the cursor, zero-length advance succeeds while further reads/advances fail.
            cursor.advance(0).expect("advance 0 bytes at end");
            assert_eq!(cursor.advance(1), error!(EINVAL));
            assert_eq!(cursor.read_object::<u8>(), error!(EINVAL));
        })
        .await;
    }

    #[fuchsia::test]
    async fn user_memory_cursor_spilled_heap_fallback() {
        spawn_kernel_and_run(async |current_task| {
            let addr = map_memory_anywhere(current_task, *PAGE_SIZE);
            let count = (INLINE_CAPACITY / std::mem::size_of::<u64>()) * 2;
            let payload: Vec<u64> = (0..count as u64).map(|i| 0xdead_beef_0000_0000 + i).collect();
            let total_bytes = payload.as_bytes().len();
            assert!(total_bytes > INLINE_CAPACITY);
            current_task.write_memory(addr, payload.as_bytes()).expect("write large payload");

            let mut cursor = UserMemoryCursor::new(current_task, addr, total_bytes as u64)
                .expect("create spilled cursor");
            assert!(cursor.buffer.spilled());
            assert_eq!(cursor.bytes_read(), 0);

            assert_eq!(cursor.read_object::<u64>().expect("read element 0"), payload[0]);
            assert_eq!(cursor.bytes_read(), 8);

            // Advance past INLINE_CAPACITY into the second half of the heap buffer.
            cursor.advance(INLINE_CAPACITY as u64).expect("advance across inline capacity");
            let mid_index = 1 + (INLINE_CAPACITY / std::mem::size_of::<u64>());
            assert_eq!(cursor.read_object::<u64>().expect("read mid element"), payload[mid_index]);

            let remaining = total_bytes - cursor.bytes_read();
            assert_eq!(cursor.advance(remaining as u64 + 1), error!(EINVAL));
            cursor
                .advance((remaining - std::mem::size_of::<u64>()) as u64)
                .expect("advance to last");
            assert_eq!(
                cursor.read_object::<u64>().expect("read last element"),
                *payload.last().unwrap()
            );
            assert_eq!(cursor.bytes_read(), total_bytes);
            assert_eq!(cursor.read_object::<u8>(), error!(EINVAL));
            assert_eq!(cursor.advance(1), error!(EINVAL));
        })
        .await;
    }
}
