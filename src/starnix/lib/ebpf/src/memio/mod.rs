// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use derivative::Derivative;
use std::marker::PhantomData;
use std::mem::MaybeUninit;
use std::ops::{Range, RangeBounds};
use zerocopy::{FromBytes, IntoBytes};

#[cfg(target_arch = "aarch64")]
mod arm64;

#[cfg(target_arch = "aarch64")]
use arm64 as arch;

#[cfg(target_arch = "x86_64")]
mod x64;

#[cfg(target_arch = "x86_64")]
use x64 as arch;

#[cfg(target_arch = "riscv64")]
mod riscv64;

#[cfg(target_arch = "riscv64")]
use riscv64 as arch;

/// Pointer to a buffer that may be shared between eBPF programs. It allows to
/// safely pass around pointers to the data stored in eBPF maps and access the
/// data referenced by the pointer.
#[derive(Derivative)]
#[derivative(Copy(bound = ""), Clone(bound = ""))]
pub struct EbpfPtr<'a, T> {
    ptr: *mut T,
    phantom: PhantomData<&'a T>,
}

#[allow(clippy::undocumented_unsafe_blocks, reason = "Force documented unsafe blocks in Starnix")]
unsafe impl<'a, T> Send for EbpfPtr<'a, T> {}
#[allow(clippy::undocumented_unsafe_blocks, reason = "Force documented unsafe blocks in Starnix")]
unsafe impl<'a, T> Sync for EbpfPtr<'a, T> {}

impl<'a, T> EbpfPtr<'a, T>
where
    T: Sized,
{
    /// Creates a new `EbpfPtr` from the specified pointer.
    ///
    /// # Safety
    /// Caller must ensure that the buffer referenced by `ptr` is valid for
    /// lifetime `'a` and there are no other mutable references to the same memory.
    pub unsafe fn new(ptr: *mut T) -> Self {
        Self { ptr, phantom: PhantomData }
    }

    /// # Safety
    /// Caller must ensure that the value cannot be updated by other threads
    /// while the returned reference is live.
    pub unsafe fn deref(&self) -> &'a T {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        unsafe {
            &*self.ptr
        }
    }

    /// # Safety
    /// Caller must ensure that the value is not being used by other threads.
    pub unsafe fn deref_mut(&self) -> &'a mut T {
        #[allow(clippy::undocumented_unsafe_blocks, reason = "2024 edition migration")]
        unsafe {
            &mut *self.ptr
        }
    }

    pub fn get_field<F, const OFFSET: usize>(&self) -> EbpfPtr<'a, F> {
        assert!(OFFSET + std::mem::size_of::<F>() <= std::mem::size_of::<T>());
        // SAFETY: offset is guaranteed to be within the bounds of the struct,
        // see the assert above.
        let field_ptr = unsafe { self.ptr.byte_offset(OFFSET as isize) } as *mut F;
        EbpfPtr::<'a, F> { ptr: field_ptr, phantom: PhantomData }
    }

    pub fn ptr(&self) -> *mut T {
        self.ptr
    }
}

impl<'a, T> From<&'a mut T> for EbpfPtr<'a, T>
where
    T: IntoBytes + FromBytes + Sized,
{
    fn from(value: &'a mut T) -> Self {
        let ptr = value.as_mut_bytes().as_mut_ptr() as *mut T;
        // SAFETY: We borrow a mutable reference to T for the lifetime 'a.
        // This guarantees that the returned pointer is valid for the lifetime
        // 'a and there are no other mutable references.
        unsafe { Self::new(ptr) }
    }
}

impl EbpfPtr<'_, u64> {
    /// Loads the value referenced by the pointer. Atomicity is guaranteed
    /// if and only if the pointer is 8-byte aligned.
    pub fn load_relaxed(&self) -> u64 {
        // SAFETY: Atomic load of the value referenced by the pointer.
        unsafe { arch::load_u64(self.ptr) }
    }

    /// Stores the `value` at the memory referenced by the pointer. Atomicity
    /// is guaranteed if and only if the pointer is 8-byte aligned.
    pub fn store_relaxed(&self, value: u64) {
        // SAFETY: Atomic store of the value referenced by the pointer.
        unsafe { arch::store_u64(self.ptr, value) }
    }
}

impl EbpfPtr<'_, u32> {
    /// Loads the value referenced by the pointer. Atomicity is guaranteed
    /// if and only if the pointer is 4-byte aligned.
    pub fn load_relaxed(&self) -> u32 {
        // SAFETY: Atomic load of the value referenced by the pointer.
        unsafe { arch::load_u32(self.ptr) }
    }

    /// Stores the `value` at the memory referenced by the pointer. Atomicity
    /// is guaranteed if and only if the pointer is 4-byte aligned.
    pub fn store_relaxed(&self, value: u32) {
        // SAFETY: Atomic store of the value referenced by the pointer.
        unsafe { arch::store_u32(self.ptr, value) }
    }
}

impl EbpfPtr<'_, i32> {
    /// Loads the value referenced by the pointer. Atomicity is guaranteed
    /// if and only if the pointer is 4-byte aligned.
    pub fn load_relaxed(&self) -> i32 {
        // SAFETY: Atomic load of the value referenced by the pointer.
        unsafe { arch::load_u32(self.ptr as *mut u32) as i32 }
    }

    /// Stores the `value` at the memory referenced by the pointer. Atomicity
    /// is guaranteed if and only if the pointer is 4-byte aligned.
    pub fn store_relaxed(&self, value: i32) {
        // SAFETY: Atomic store of the value referenced by the pointer.
        unsafe { arch::store_u32(self.ptr as *mut u32, value as u32) }
    }
}

/// Wraps a pointer to buffer used in eBPF runtime, such as an eBPF maps
/// entry. The referenced data may be access from multiple threads in parallel,
/// which makes it unsafe to access it using standard Rust types.
/// `EbpfBufferPtr` allows to access these buffers safely. It may be used to
/// reference either a whole VMO allocated for and eBPF map or individual
/// elements of that VMO (see `slice()`). The address and the size of the
/// buffer are always 8-byte aligned.
#[derive(Copy, Clone)]
pub struct EbpfBufferPtr<'a> {
    ptr: *mut u8,
    size: usize,
    phantom: PhantomData<&'a u8>,
}

impl<'a> EbpfBufferPtr<'a> {
    pub const ALIGNMENT: usize = size_of::<u64>();

    /// Creates a new `EbpfBufferPtr` from the specified pointer.
    ///
    /// # Safety
    /// Caller must ensure that the buffer referenced by `ptr` is valid for
    /// lifetime `'a`.
    pub unsafe fn new(ptr: *mut u8, size: usize) -> Self {
        Self { ptr, size, phantom: PhantomData }
    }

    /// Size of the buffer in bytes.
    pub fn len(&self) -> usize {
        self.size
    }

    /// Raw pointer to the start of the buffer.
    pub fn raw_ptr(&self) -> *mut u8 {
        self.ptr
    }

    // SAFETY: caller must ensure that the value at the specified offset fits
    // the buffer.
    unsafe fn get_ptr_internal<T>(&self, offset: usize) -> EbpfPtr<'a, T> {
        // SAFETY: The caller is expected to ensure that the pointer is valid
        // for the lifetime 'a.
        unsafe { EbpfPtr::new(self.ptr.byte_offset(offset as isize) as *mut T) }
    }

    /// Returns a pointer to a value of type `T` at the specified `offset`.
    pub fn get_ptr<T>(&self, offset: usize) -> Option<EbpfPtr<'a, T>> {
        if offset + std::mem::size_of::<T>() <= self.size {
            // SAFETY: Buffer bounds are verified above.
            Some(unsafe { self.get_ptr_internal(offset) })
        } else {
            None
        }
    }

    /// Returns pointer to the specified range in the buffer.
    pub fn slice(&self, range: impl RangeBounds<usize>) -> Option<Self> {
        let start = match range.start_bound() {
            std::ops::Bound::Included(&start) => start,
            std::ops::Bound::Excluded(&start) => start + 1,
            std::ops::Bound::Unbounded => 0,
        };
        let end = match range.end_bound() {
            std::ops::Bound::Included(&end) => end + 1,
            std::ops::Bound::Excluded(&end) => end,
            std::ops::Bound::Unbounded => self.size,
        };

        assert!(start <= end);
        (end <= self.size).then(|| {
            // SAFETY: Returned buffer has the same lifetime as `self`, which
            // ensures that the `ptr` stays valid for the lifetime of the
            // result.
            unsafe {
                Self {
                    ptr: self.ptr.byte_offset(start as isize),
                    size: end - start,
                    phantom: PhantomData,
                }
            }
        })
    }

    /// Loads contents of the buffer into the specified slice, `dst` must be
    /// of the same size as `self`.
    pub fn load_to_slice(&self, dst: &mut [MaybeUninit<u8>]) {
        assert_eq!(dst.len(), self.size);

        let mut src_ptr = self.ptr;
        // SAFETY: `len` is validated above to be within bounds.
        let src_end = unsafe { src_ptr.add(self.size) };

        let Range { start: dst_ptr, end: dst_end } = dst.as_mut_ptr_range();
        let mut dst_ptr = dst_ptr as *mut u8;
        let dst_end = dst_end as *mut u8;

        if src_ptr as usize % 8 > 0 {
            if src_ptr < src_end && src_ptr as usize % 2 > 0 {
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    let value: u8 = arch::load_u8(src_ptr as *const u8);
                    std::ptr::write_unaligned(dst_ptr, value);
                    src_ptr = src_ptr.add(1);
                    dst_ptr = dst_ptr.add(1);
                };
            }

            if src_ptr as usize + 2 <= src_end as usize && src_ptr as usize % 4 > 0 {
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    let value: u16 = arch::load_u16(src_ptr as *const u16);
                    std::ptr::write_unaligned(dst_ptr as *mut u16, value);
                    src_ptr = src_ptr.add(2);
                    dst_ptr = dst_ptr.add(2);
                }
            }

            if src_ptr as usize + 4 <= src_end as usize && src_ptr as usize % 8 > 0 {
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    let value: u32 = arch::load_u32(src_ptr as *const u32);
                    std::ptr::write_unaligned(dst_ptr as *mut u32, value);
                    src_ptr = src_ptr.add(4);
                    dst_ptr = dst_ptr.add(4);
                }
            }
        }

        while src_ptr as usize + 8 <= src_end as usize {
            // SAFETY: Pointers are verified to be within the buffer bounds.
            unsafe {
                let value: u64 = arch::load_u64(src_ptr as *const u64);
                std::ptr::write_unaligned(dst_ptr as *mut u64, value);
                src_ptr = src_ptr.add(8);
                dst_ptr = dst_ptr.add(8);
            }
        }

        if src_ptr < src_end {
            if src_ptr as usize + 4 <= src_end as usize {
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    let value: u32 = arch::load_u32(src_ptr as *const u32);
                    std::ptr::write_unaligned(dst_ptr as *mut u32, value);
                    src_ptr = src_ptr.add(4);
                    dst_ptr = dst_ptr.add(4);
                }
            }

            if src_ptr as usize + 2 <= src_end as usize {
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    let value: u16 = arch::load_u16(src_ptr as *const u16);
                    std::ptr::write_unaligned(dst_ptr as *mut u16, value);
                    src_ptr = src_ptr.add(2);
                    dst_ptr = dst_ptr.add(2);
                }
            }

            if src_ptr < src_end {
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    let value: u8 = arch::load_u8(src_ptr as *const u8);
                    std::ptr::write_unaligned(dst_ptr, value);
                    src_ptr = src_ptr.add(1);
                    dst_ptr = dst_ptr.add(1);
                }
            }
        }

        debug_assert_eq!(src_ptr, src_end);
        debug_assert_eq!(dst_ptr, dst_end);
    }

    /// Loads all buffer contents into a `SmallVec`.
    pub fn load(&self) -> Vec<u8> {
        let mut vec = Vec::<u8>::with_capacity(self.size);
        self.load_to_slice(vec.spare_capacity_mut());
        // SAFETY: load() fills the buffer.
        unsafe { vec.set_len(self.size) };
        vec
    }

    /// Stores `data` in the buffer. `data` must not be larger than the buffer.
    pub fn store(&self, data: &[u8]) {
        assert!(data.len() <= self.size);

        let mut ptr = self.ptr;
        // SAFETY: `len` is validated above to be within bounds.
        let end = unsafe { ptr.add(data.len()) };
        let mut data_offset = 0;

        // Write the head of the buffer with u8, u16, u32 stores.
        if ptr as usize % 8 > 0 {
            if ptr < end && ptr as usize % 2 > 0 {
                let value = data[data_offset];
                data_offset += 1;
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    arch::store_u8(ptr, value);
                    ptr = ptr.add(1)
                };
            }

            if (ptr as usize) + 2 <= end as usize && ptr as usize % 4 > 0 {
                let value = u16::read_from_bytes(&data[data_offset..(data_offset + 2)]).unwrap();
                data_offset += 2;
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    arch::store_u16(ptr as *mut u16, value);
                    ptr = ptr.add(2)
                };
            }

            if (ptr as usize) + 4 <= end as usize && ptr as usize % 8 > 0 {
                let value = u32::read_from_bytes(&data[data_offset..(data_offset + 4)]).unwrap();
                data_offset += 4;
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    arch::store_u32(ptr as *mut u32, value);
                    ptr = ptr.add(4)
                };
            }
        }

        // Write the body of the buffer with u64 stores.
        while (ptr as usize) + 8 <= end as usize {
            let value = u64::read_from_bytes(&data[data_offset..(data_offset + 8)]).unwrap();
            data_offset += 8;
            // SAFETY: Pointers are verified to be within the buffer bounds.
            unsafe {
                arch::store_u64(ptr as *mut u64, value);
                ptr = ptr.add(8)
            };
        }

        // Write the tail of the buffer with u32, u16, u8 stores.
        if ptr < end {
            if (ptr as usize) + 4 <= end as usize {
                let value = u32::read_from_bytes(&data[data_offset..(data_offset + 4)]).unwrap();
                data_offset += 4;
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    arch::store_u32(ptr as *mut u32, value);
                    ptr = ptr.add(4)
                };
            }

            if (ptr as usize) + 2 <= end as usize {
                let value = u16::read_from_bytes(&data[data_offset..(data_offset + 2)]).unwrap();
                data_offset += 2;
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    arch::store_u16(ptr as *mut u16, value);
                    ptr = ptr.add(2)
                };
            }

            if ptr < end {
                let value = data[data_offset];
                data_offset += 1;
                // SAFETY: Pointers are verified to be within the buffer bounds.
                unsafe {
                    arch::store_u8(ptr, value);
                    ptr = ptr.add(1)
                };
            }
        }

        debug_assert_eq!(ptr, end);
        debug_assert_eq!(data_offset, data.len());
    }

    /// Copies the data from another `EbpfBufferPtr`. `src` may be smaller than
    /// `self`. In this case it's copied to the beginning of `self`.
    pub fn copy(&self, src: &EbpfBufferPtr<'_>) {
        assert!(src.len() <= self.size);

        let mut dst_ptr = self.ptr;
        // SAFETY: src.len() <= self.size is asserted above.
        let dst_end = unsafe { dst_ptr.add(src.len()) };

        let mut src_ptr = src.ptr;
        // SAFETY: Calculate ptr to the end of the source buffer.
        let src_end = unsafe { src_ptr.add(src.len()) };

        // Fast path when both buffers are 8-byte aligned.
        if (src_ptr as usize) % 8 == 0 && (dst_ptr as usize) % 8 == 0 {
            while (src_ptr as usize) + 8 <= src_end as usize {
                // SAFETY: Pointers are verified to be within the bounds of valid buffers.
                unsafe {
                    let value: u64 = arch::load_u64(src_ptr as *const u64);
                    arch::store_u64(dst_ptr as *mut u64, value);
                    src_ptr = src_ptr.add(8);
                    dst_ptr = dst_ptr.add(8);
                }
            }

            if src_ptr < src_end {
                if (src_ptr as usize) + 4 <= src_end as usize {
                    // SAFETY: Pointers are verified to be within the bounds of valid buffers.
                    unsafe {
                        let value: u32 = arch::load_u32(src_ptr as *const u32);
                        arch::store_u32(dst_ptr as *mut u32, value);
                        src_ptr = src_ptr.add(4);
                        dst_ptr = dst_ptr.add(4);
                    }
                }

                if (src_ptr as usize) + 2 <= src_end as usize {
                    // SAFETY: Pointers are verified to be within the bounds of valid buffers.
                    unsafe {
                        let value: u16 = arch::load_u16(src_ptr as *const u16);
                        arch::store_u16(dst_ptr as *mut u16, value);
                        src_ptr = src_ptr.add(2);
                        dst_ptr = dst_ptr.add(2);
                    }
                }

                if src_ptr < src_end {
                    // SAFETY: Pointers are verified to be within the bounds of valid buffers.
                    unsafe {
                        let value: u8 = arch::load_u8(src_ptr as *const u8);
                        arch::store_u8(dst_ptr as *mut u8, value);
                        src_ptr = src_ptr.add(1);
                        dst_ptr = dst_ptr.add(1);
                    }
                }
            }

            debug_assert_eq!(src_ptr, src_end);
            debug_assert_eq!(dst_ptr, dst_end);
        } else {
            // Slow path fallback for unaligned buffers: Load the source values
            // into a temporary buffer and then store them into the destination
            // buffer.
            self.store(&src.load());
        }
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use fuchsia_runtime::vmar_root_self;
    use std::sync::Barrier;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::thread;

    #[test]
    fn test_u64_atomicity() {
        let vmo_size = zx::system_get_page_size() as usize;
        let vmo = zx::Vmo::create(vmo_size as u64).unwrap();
        let addr = vmar_root_self()
            .map(0, &vmo, 0, vmo_size, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
            .unwrap();
        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        let shared_ptr = unsafe { EbpfPtr::new(addr as *mut u64) };

        const NUM_THREADS: usize = 10;

        // Barrier used to synchronize start of the threads.
        let barrier = Barrier::new(NUM_THREADS * 2);

        let finished_writers = AtomicU32::new(0);

        thread::scope(|scope| {
            let mut threads = Vec::new();

            for _ in 0..10 {
                threads.push(scope.spawn(|| {
                    barrier.wait();
                    for _ in 0..1000 {
                        for i in 0..255 {
                            // Store a value with the same value repeated in every byte.
                            let v = i << 8 | i;
                            let v = v << 16 | v;
                            let v = v << 32 | v;
                            shared_ptr.store_relaxed(v);
                        }
                    }
                    finished_writers.fetch_add(1, Ordering::Relaxed);
                }));

                threads.push(scope.spawn(|| {
                    barrier.wait();
                    loop {
                        for _ in 0..1000 {
                            let v = shared_ptr.load_relaxed();
                            // Verify that all bytes in `v` are set to the same.
                            assert!(v >> 32 == v & 0xffff_ffff);
                            assert!((v >> 16) & 0xffff == v & 0xffff);
                            assert!((v >> 8) & 0xff == v & 0xff);
                        }
                        if finished_writers.load(Ordering::Relaxed) == NUM_THREADS as u32 {
                            break;
                        }
                    }
                }));
            }

            for t in threads.into_iter() {
                t.join().expect("failed to join a test thread");
            }
        });

        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        unsafe {
            vmar_root_self().unmap(addr, vmo_size).unwrap()
        };
    }

    #[test]
    fn test_buffer_slice() {
        const SIZE: usize = 32;

        let mut buf = [0; SIZE];
        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        let buf_ptr = unsafe { EbpfBufferPtr::new(buf.as_mut_ptr(), SIZE) };

        buf_ptr.slice(8..16).unwrap().store(&[1, 2, 3, 4, 5, 6, 7, 8]);
        let value = buf_ptr.slice(0..24).unwrap().load();
        assert_eq!(
            &value[..],
            &[0, 0, 0, 0, 0, 0, 0, 0, 1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0]
        );

        assert!(buf_ptr.slice(8..40).is_none());
    }

    #[test]
    fn test_buffer_load() {
        const FULL_SIZE: usize = 40;
        let mut buf = (0..(FULL_SIZE as u8)).map(|v| v as u8).collect::<Vec<_>>();
        // SAFETY: Creating EbpfBufferPtr for the buffer allocated above.
        let buf_ptr = unsafe { EbpfBufferPtr::new(buf.as_mut_ptr(), FULL_SIZE) };

        for start in 0..FULL_SIZE {
            for end in start..=FULL_SIZE {
                let slice = buf_ptr.slice(start..end).unwrap();
                let loaded = slice.load();

                let expected = (start..end).map(|v| v as u8).collect::<Vec<_>>();
                assert_eq!(&loaded[..], &expected[..], "failed for range {}..{}", start, end);
            }
        }
    }

    #[test]
    fn test_buffer_store() {
        const FULL_SIZE: usize = 40;
        let mut buf = [0u8; FULL_SIZE];
        // SAFETY: Creating EbpfBufferPtr for the buffer allocated above.
        let buf_ptr = unsafe { EbpfBufferPtr::new(buf.as_mut_ptr(), FULL_SIZE) };

        for start in 0..FULL_SIZE {
            for end in start..=FULL_SIZE {
                let slice = buf_ptr.slice(start..end).unwrap();
                let data_to_store = (start..end).map(|v| v as u8).collect::<Vec<_>>();
                slice.store(&data_to_store);

                let loaded = slice.load();
                assert_eq!(&loaded[..], &data_to_store[..], "failed for range {}..{}", start, end);
            }
        }
    }

    #[test]
    fn test_buffer_copy() {
        const BASE_SIZE: usize = 48;
        let mut src_buf = (0..(BASE_SIZE as u8)).map(|v| v as u8).collect::<Vec<_>>();
        let mut dst_buf = [0u8; BASE_SIZE];

        // SAFETY: Creating EbpfBufferPtr for the buffer allocated above.
        let src_base = unsafe { EbpfBufferPtr::new(src_buf.as_mut_ptr(), BASE_SIZE) };

        // SAFETY: Creating EbpfBufferPtr for the buffer allocated above.
        let dst_base = unsafe { EbpfBufferPtr::new(dst_buf.as_mut_ptr(), BASE_SIZE) };

        for src_align in 0..8 {
            for dst_align in 0..8 {
                for len in 0..=32 {
                    dst_buf.fill(0);

                    let src_slice = src_base.slice(src_align..(src_align + len)).unwrap();
                    let dst_slice = dst_base.slice(dst_align..(dst_align + len)).unwrap();

                    dst_slice.copy(&src_slice);

                    let loaded = dst_slice.load();
                    let expected =
                        (src_align..(src_align + len)).map(|v| v as u8).collect::<Vec<_>>();
                    assert_eq!(
                        &loaded[..],
                        &expected[..],
                        "copy failed for length {} with src align {} and dst align {}",
                        len,
                        src_align,
                        dst_align
                    );

                    for i in 0..BASE_SIZE {
                        if i < dst_align || i >= dst_align + len {
                            assert_eq!(
                                dst_buf[i], 0,
                                "out-of-bounds memory modified at index {} (dst_align={}, len={})",
                                i, dst_align, len
                            );
                        }
                    }
                }
            }
        }
    }
}
