// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Safe wrappers around pinned and mapped DMA buffers for Fuchsia drivers.

#![deny(missing_docs)]

use fuchsia_runtime::vmar_root_self;
use mapped_vmo::Mapping;
use std::ops::{Deref, DerefMut};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zx::sys::zx_paddr_t;
use zx::{
    Bti, BtiOptions, CachePolicy, Pmt, Status, Vmar, VmarFlags, Vmo, VmoOp, system_get_page_size,
};

mod phys_iter;
pub use phys_iter::PhysIter;

/// Cache options for DMA buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum CacheOptions {
    /// Disable CPU caching for the DMA buffer (`ZX_CACHE_POLICY_UNCACHED_DEVICE`).
    Disabled,
    /// Enable CPU caching for the DMA buffer.
    #[default]
    Enabled,
}

/// Helper for writing to uncached DMA buffer memory without triggering alignment faults.
///
/// Using volatile pointer writes prevents the compiler from coalescing or vectorizing stores into
/// unaligned instructions or `memcpy` calls that fault on `ZX_CACHE_POLICY_UNCACHED_DEVICE`
/// mappings (e.g. on ARM64).
///
/// # Safety
///
/// `dest_ptr..dest_ptr.add(src.len())` must be valid for writes within the mapped DMA buffer.
unsafe fn uncached_write(mut dest_ptr: *mut u8, mut src: &[u8]) {
    while (dest_ptr as usize) & (size_of::<u64>() - 1) != 0 && !src.is_empty() {
        // SAFETY: `dest_ptr` is within the mapped buffer bounds.
        unsafe {
            dest_ptr.write_volatile(src[0]);
            dest_ptr = dest_ptr.add(1);
        }
        src = &src[1..];
    }
    let (chunks, remainder) = src.as_chunks::<{ size_of::<u64>() }>();
    for chunk in chunks {
        let val = u64::from_ne_bytes(*chunk);
        // SAFETY: `dest_ptr` is 8-byte aligned and at least 8 bytes remain in the mapped buffer.
        unsafe {
            dest_ptr.cast::<u64>().write_volatile(val);
            dest_ptr = dest_ptr.add(size_of::<u64>());
        }
    }
    for &byte in remainder {
        // SAFETY: `dest_ptr` is within the mapped buffer bounds.
        unsafe {
            dest_ptr.write_volatile(byte);
            dest_ptr = dest_ptr.add(1);
        }
    }
}

/// Helper for reading from uncached DMA buffer memory without triggering alignment faults.
///
/// # Safety
///
/// `src_ptr..src_ptr.add(dst.len())` must be valid for reads within the mapped DMA buffer.
unsafe fn uncached_read(mut src_ptr: *const u8, mut dst: &mut [u8]) {
    while (src_ptr as usize) & (size_of::<u64>() - 1) != 0 && !dst.is_empty() {
        // SAFETY: `src_ptr` is within the mapped buffer bounds.
        unsafe {
            dst[0] = src_ptr.read_volatile();
            src_ptr = src_ptr.add(1);
        }
        dst = &mut dst[1..];
    }
    let (chunks, remainder) = dst.as_chunks_mut::<{ size_of::<u64>() }>();
    for chunk in chunks {
        // SAFETY: `src_ptr` is 8-byte aligned and at least 8 bytes remain in the mapped buffer.
        let val = unsafe {
            let v = src_ptr.cast::<u64>().read_volatile();
            src_ptr = src_ptr.add(size_of::<u64>());
            v
        };
        *chunk = val.to_ne_bytes();
    }
    for byte in remainder {
        // SAFETY: `src_ptr` is within the mapped buffer bounds.
        unsafe {
            *byte = src_ptr.read_volatile();
            src_ptr = src_ptr.add(1);
        }
    }
}

/// Concrete type for DMA buffers providing cache management, pinned memory token lifecycle, and
/// safe memory access backed by a [`mapped_vmo::Mapping`].
#[derive(Debug)]
pub struct Buffer {
    mapping: Mapping,
    ptr: *mut u8,
    vmo: Vmo,
    pmt: Pmt,
    cache_options: CacheOptions,
}

// SAFETY: `Buffer` owns `mapping` (which is `Send` and `Sync`), and `ptr` points to the base of
// `mapping` and is only accessed via bounds-checked volatile reads and writes while `mapping` is
// alive.
unsafe impl Send for Buffer {}
unsafe impl Sync for Buffer {}

impl Deref for Buffer {
    type Target = Mapping;

    fn deref(&self) -> &Self::Target {
        &self.mapping
    }
}

impl DerefMut for Buffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.mapping
    }
}

impl Buffer {
    fn new(mut mapping: Mapping, vmo: Vmo, pmt: Pmt, cache_options: CacheOptions) -> Self {
        let (ptr, _) = mapping.as_ptr_len();
        Self { mapping, ptr, vmo, pmt, cache_options }
    }

    /// Returns the size of the mapped DMA buffer in bytes.
    pub fn size(&self) -> usize {
        self.mapping.len()
    }

    /// Returns a reference to the underlying VMO.
    pub fn vmo(&self) -> &Vmo {
        &self.vmo
    }

    /// Returns a reference to the underlying [`Mapping`].
    pub fn mapping(&self) -> &Mapping {
        &self.mapping
    }

    /// Returns a mutable reference to the underlying [`Mapping`].
    pub fn mapping_mut(&mut self) -> &mut Mapping {
        &mut self.mapping
    }

    /// Returns whether CPU caching is enabled for this DMA buffer.
    pub fn is_cache_enabled(&self) -> bool {
        self.cache_options == CacheOptions::Enabled
    }

    /// Returns the [`CacheOptions`] configured for this DMA buffer.
    pub fn cache_options(&self) -> CacheOptions {
        self.cache_options
    }

    /// Unpins the DMA buffer's physical memory token, consuming the buffer.
    ///
    /// Note that `Drop` does not automatically unpin the buffer (instead allowing the kernel to
    /// quarantine the pages on the BTI if dropped while still pinned). Call this method after
    /// ensuring hardware DMA has ceased.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.pmt.unpin() }
    }

    /// Flushes CPU data cache lines (clean to memory) for the given byte range.
    ///
    /// Use after CPU writes to the buffer so that DMA hardware can read from physical RAM
    /// (`DMA_TO_DEVICE`).
    pub fn cache_flush(&self, offset: usize, length: usize) -> Result<(), Status> {
        if length == 0 || !self.is_cache_enabled() {
            return Ok(());
        }
        if offset.checked_add(length).is_none_or(|end| end > self.size()) {
            return Err(Status::OUT_OF_RANGE);
        }
        self.vmo.op_range(VmoOp::CACHE_CLEAN, offset as u64, length as u64)
    }

    /// Flushes and invalidates CPU data cache lines for the given byte range.
    ///
    /// Use before CPU reads from the buffer after DMA hardware writes to physical RAM
    /// (`DMA_FROM_DEVICE`).
    pub fn cache_flush_invalidate(&self, offset: usize, length: usize) -> Result<(), Status> {
        if length == 0 || !self.is_cache_enabled() {
            return Ok(());
        }
        if offset.checked_add(length).is_none_or(|end| end > self.size()) {
            return Err(Status::OUT_OF_RANGE);
        }
        self.vmo.op_range(VmoOp::CACHE_CLEAN_INVALIDATE, offset as u64, length as u64)
    }

    /// Writes `src` into the DMA buffer at `offset` and flushes the CPU cache if caching is
    /// enabled.
    pub fn write(&self, offset: usize, src: &[u8]) -> Result<(), Status> {
        if src.is_empty() {
            return Ok(());
        }
        if offset.checked_add(src.len()).is_none_or(|end| end > self.size()) {
            return Err(Status::OUT_OF_RANGE);
        }
        if self.is_cache_enabled() {
            assert_eq!(self.mapping.write_at(offset, src), src.len());
            self.cache_flush(offset, src.len())
        } else {
            // SAFETY: `offset + src.len() <= self.size()` was verified above.
            unsafe { uncached_write(self.ptr.add(offset), src) };
            Ok(())
        }
    }

    /// Invalidates the CPU cache if caching is enabled and reads from the DMA buffer at `offset`
    /// into `dst`.
    pub fn read(&self, offset: usize, dst: &mut [u8]) -> Result<(), Status> {
        if dst.is_empty() {
            return Ok(());
        }
        if offset.checked_add(dst.len()).is_none_or(|end| end > self.size()) {
            return Err(Status::OUT_OF_RANGE);
        }
        if self.is_cache_enabled() {
            self.cache_flush_invalidate(offset, dst.len())?;
            assert_eq!(self.mapping.read_at(offset, dst), dst.len());
        } else {
            // SAFETY: `offset + dst.len() <= self.size()` was verified above.
            unsafe { uncached_read(self.ptr.add(offset), dst) };
        }
        Ok(())
    }

    /// Writes a value of type `T` at byte `offset` with automatic cache flush.
    pub fn write_struct<T: IntoBytes + Immutable>(
        &self,
        offset: usize,
        val: &T,
    ) -> Result<(), Status> {
        self.write(offset, val.as_bytes())
    }

    /// Reads a value of type `T` from byte `offset` with automatic cache invalidation.
    pub fn read_struct<T: FromBytes + IntoBytes + KnownLayout + Immutable>(
        &self,
        offset: usize,
    ) -> Result<T, Status> {
        let mut val = T::new_zeroed();
        self.read(offset, val.as_mut_bytes())?;
        Ok(val)
    }

    /// Reads the `index`-th element of type `T` (at byte offset `index * size_of::<T>()`) from the
    /// mapped buffer.
    ///
    /// # Panics
    ///
    /// Panics if `index * size_of::<T>() + size_of::<T>()` overflows or exceeds `self.size()`.
    pub fn read_element<T: FromBytes + IntoBytes + KnownLayout + Immutable>(
        &self,
        index: usize,
    ) -> T {
        let mut val = T::new_zeroed();
        let dst = val.as_mut_bytes();
        let offset = index.checked_mul(dst.len()).expect("DMA read index overflow");
        assert!(
            offset.checked_add(dst.len()).is_some_and(|end| end <= self.size()),
            "DMA read out of bounds"
        );
        assert_eq!(self.mapping.read_at(offset, dst), dst.len());
        val
    }

    /// Writes `value` as the `index`-th element of type `T` (at byte offset
    /// `index * size_of::<T>()`) in the mapped buffer.
    ///
    /// # Panics
    ///
    /// Panics if `index * size_of::<T>() + size_of::<T>()` overflows or exceeds `self.size()`.
    pub fn write_element<T: IntoBytes + Immutable>(&self, index: usize, value: &T) {
        let src = value.as_bytes();
        let offset = index.checked_mul(src.len()).expect("DMA write index overflow");
        assert!(
            offset.checked_add(src.len()).is_some_and(|end| end <= self.size()),
            "DMA write out of bounds"
        );
        assert_eq!(self.mapping.write_at(offset, src), src.len());
    }
}

/// A physically contiguous DMA buffer pinned to a [`Bti`] and mapped into a [`Vmar`].
#[derive(Debug)]
pub struct ContiguousBuffer {
    buffer: Buffer,
    phys: zx_paddr_t,
}

impl Deref for ContiguousBuffer {
    type Target = Buffer;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl DerefMut for ContiguousBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

impl ContiguousBuffer {
    /// Creates a new contiguous DMA buffer of `size` bytes (rounded up to the system page size)
    /// mapped in the root VMAR with default alignment and CPU caching enabled.
    pub fn new(bti: &Bti, size: usize) -> Result<Self, Status> {
        Self::new_with_vmar(bti, &vmar_root_self(), size)
    }

    /// Creates a new contiguous DMA buffer of `size` bytes (rounded up to the system page size)
    /// mapped in `vmar` with default alignment and CPU caching enabled.
    pub fn new_with_vmar(bti: &Bti, vmar: &Vmar, size: usize) -> Result<Self, Status> {
        Self::new_with_options(bti, vmar, size, 0, CacheOptions::Enabled)
    }

    /// Creates a new contiguous DMA buffer of `size` bytes (rounded up to the system page size)
    /// mapped in `vmar` with the specified `alignment_log2` and `cache_options`.
    pub fn new_with_options(
        bti: &Bti,
        vmar: &Vmar,
        size: usize,
        alignment_log2: u32,
        cache_options: CacheOptions,
    ) -> Result<Self, Status> {
        if size == 0 {
            return Err(Status::INVALID_ARGS);
        }
        let page_size = system_get_page_size() as usize;
        let mapped_size = size.checked_next_multiple_of(page_size).ok_or(Status::OUT_OF_RANGE)?;

        let vmo = Vmo::create_contiguous(bti, mapped_size, alignment_log2)?;
        if cache_options == CacheOptions::Disabled {
            vmo.set_cache_policy(CachePolicy::UnCachedDevice)?;
        }
        let mapping = Mapping::create_from_vmo_with_vmar(
            &vmo,
            vmar,
            mapped_size,
            VmarFlags::PERM_READ | VmarFlags::PERM_WRITE,
        )?;
        let mut phys = [0 as zx_paddr_t; 1];
        let pmt = bti.pin(
            BtiOptions::PERM_READ | BtiOptions::PERM_WRITE | BtiOptions::CONTIGUOUS,
            &vmo,
            0,
            mapped_size as u64,
            &mut phys,
        )?;

        Ok(Self { buffer: Buffer::new(mapping, vmo, pmt, cache_options), phys: phys[0] })
    }

    /// Returns a reference to the underlying [`Buffer`].
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Returns the base physical address of the contiguous DMA buffer.
    pub fn phys(&self) -> zx_paddr_t {
        self.phys
    }

    /// Unpins the DMA buffer's physical memory token, consuming the buffer so its physical
    /// address and mapping cannot be accessed after unpinning.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.buffer.unpin() }
    }
}

/// A paged DMA buffer consisting of one or more pinned pages that are contiguous in virtual memory
/// (`Mapping`), but may be discontiguous in physical memory.
#[derive(Debug)]
pub struct PagedBuffer {
    buffer: Buffer,
    phys: Vec<zx_paddr_t>,
}

impl Deref for PagedBuffer {
    type Target = Buffer;

    fn deref(&self) -> &Self::Target {
        &self.buffer
    }
}

impl DerefMut for PagedBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buffer
    }
}

impl PagedBuffer {
    /// Creates a new paged DMA buffer of `size` bytes (rounded up to the system page size) mapped
    /// in the root VMAR with CPU caching enabled.
    pub fn new(bti: &Bti, size: usize) -> Result<Self, Status> {
        Self::new_with_vmar(bti, &vmar_root_self(), size)
    }

    /// Creates a new paged DMA buffer of `size` bytes (rounded up to the system page size) mapped
    /// in `vmar` with CPU caching enabled.
    pub fn new_with_vmar(bti: &Bti, vmar: &Vmar, size: usize) -> Result<Self, Status> {
        Self::new_with_options(bti, vmar, size, CacheOptions::Enabled)
    }

    /// Creates a new paged DMA buffer of `size` bytes (rounded up to the system page size) mapped
    /// in `vmar` with the specified `cache_options`.
    pub fn new_with_options(
        bti: &Bti,
        vmar: &Vmar,
        size: usize,
        cache_options: CacheOptions,
    ) -> Result<Self, Status> {
        if size == 0 {
            return Err(Status::INVALID_ARGS);
        }
        let page_size = system_get_page_size() as usize;
        let mapped_size = size.checked_next_multiple_of(page_size).ok_or(Status::OUT_OF_RANGE)?;

        let vmo = Vmo::create(mapped_size as u64)?;
        if cache_options == CacheOptions::Disabled {
            vmo.set_cache_policy(CachePolicy::UnCachedDevice)?;
        }
        let mapping = Mapping::create_from_vmo_with_vmar(
            &vmo,
            vmar,
            mapped_size,
            VmarFlags::PERM_READ | VmarFlags::PERM_WRITE,
        )?;
        let mut phys = vec![0 as zx_paddr_t; mapped_size / page_size];
        let pmt = bti.pin(
            BtiOptions::PERM_READ | BtiOptions::PERM_WRITE,
            &vmo,
            0,
            mapped_size as u64,
            &mut phys,
        )?;

        Ok(Self { buffer: Buffer::new(mapping, vmo, pmt, cache_options), phys })
    }

    /// Returns a reference to the underlying [`Buffer`].
    pub fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Returns the slice of pinned page physical addresses backing the buffer.
    pub fn phys(&self) -> &[zx_paddr_t] {
        &self.phys
    }

    /// Unpins the DMA buffer's physical memory token, consuming the buffer so its physical
    /// addresses and mapping cannot be accessed after unpinning.
    ///
    /// # Safety
    ///
    /// The caller must ensure that hardware is no longer accessing the pinned memory (e.g. after
    /// resetting or quiescing the device).
    pub unsafe fn unpin(self) -> Result<(), Status> {
        // SAFETY: The caller guarantees hardware DMA to this buffer has ceased.
        unsafe { self.buffer.unpin() }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake_bti::FakeBti;

    #[fuchsia::test]
    fn test_contiguous_with_cache_enabled() {
        let bti = FakeBti::create().unwrap();
        let page_size = system_get_page_size() as usize;
        let size = page_size * 4;
        let buffer = ContiguousBuffer::new_with_options(
            &bti,
            &vmar_root_self(),
            size,
            12,
            CacheOptions::Enabled,
        )
        .unwrap();

        assert_eq!(buffer.size(), size);
        assert_eq!(buffer.phys(), FakeBti::phys_addr());
        assert!(buffer.is_cache_enabled());
        assert_eq!(buffer.cache_options(), CacheOptions::Enabled);
        assert_eq!(buffer.vmo().info().unwrap().cache_policy(), CachePolicy::Cached);
        assert_eq!(bti.info().unwrap().pmo_count, 1);

        unsafe { buffer.unpin().unwrap() };
        assert_eq!(bti.info().unwrap().pmo_count, 0);
    }

    #[fuchsia::test]
    fn test_contiguous_with_cache_disabled() {
        let bti = FakeBti::create().unwrap();
        let page_size = system_get_page_size() as usize;
        let size = page_size * 4;
        let buffer = ContiguousBuffer::new_with_options(
            &bti,
            &vmar_root_self(),
            size,
            12,
            CacheOptions::Disabled,
        )
        .unwrap();

        assert_eq!(buffer.size(), size);
        assert_eq!(buffer.phys(), FakeBti::phys_addr());
        assert!(!buffer.is_cache_enabled());
        assert_eq!(buffer.cache_options(), CacheOptions::Disabled);
        assert_eq!(buffer.vmo().info().unwrap().cache_policy(), CachePolicy::UnCachedDevice);

        unsafe { buffer.unpin().unwrap() };
        assert_eq!(bti.info().unwrap().pmo_count, 0);
    }

    #[fuchsia::test]
    fn test_paged_buffer() {
        let bti = FakeBti::create().unwrap();
        bti.set_paddrs(&[0x1000, 0x3000, 0x5000]);
        let page_size = system_get_page_size() as usize;
        // Request unaligned size (2 pages + 100 bytes) to verify rounding up to 3 pages.
        let buffer = PagedBuffer::new(&bti, page_size * 2 + 100).unwrap();

        assert_eq!(buffer.size(), page_size * 3);
        assert_eq!(buffer.phys(), &[0x1000, 0x3000, 0x5000]);
        assert!(buffer.is_cache_enabled());
        assert_eq!(bti.info().unwrap().pmo_count, 1);

        unsafe { buffer.unpin().unwrap() };
        assert_eq!(bti.info().unwrap().pmo_count, 0);
    }

    #[fuchsia::test]
    fn test_read_write_and_cache_operations() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;
        let buffer = ContiguousBuffer::new(&bti, size).unwrap();

        let write_data = [0xDE, 0xAD, 0xBE, 0xEF];
        let mut read_data = [0u8; 4];
        buffer.write(16, &write_data).unwrap();
        buffer.read(16, &mut read_data).unwrap();
        assert_eq!(write_data, read_data);

        #[repr(C)]
        #[derive(
            Debug, Clone, Copy, PartialEq, Eq, FromBytes, IntoBytes, KnownLayout, Immutable,
        )]
        struct TestPacket {
            magic: u32,
            length: u16,
            flags: u8,
            pad: u8,
        }

        let out_pkt = TestPacket { magic: 0x12345678, length: 64, flags: 0xFF, pad: 0 };
        buffer.write_struct(64, &out_pkt).unwrap();
        let in_pkt: TestPacket = buffer.read_struct(64).unwrap();
        assert_eq!(out_pkt, in_pkt);

        buffer.write_element(3, &out_pkt);
        assert_eq!(buffer.read_element::<TestPacket>(3), out_pkt);

        assert_eq!(buffer.write(size - 2, &write_data), Err(Status::OUT_OF_RANGE));
        assert_eq!(buffer.read(size - 2, &mut read_data), Err(Status::OUT_OF_RANGE));

        assert_eq!(buffer.cache_flush(0, 0), Ok(()));
        assert_eq!(buffer.cache_flush(0, 32), Ok(()));
        assert_eq!(buffer.cache_flush_invalidate(0, 0), Ok(()));
        assert_eq!(buffer.cache_flush_invalidate(0, 32), Ok(()));
        assert_eq!(buffer.cache_flush(size - 10, 20), Err(Status::OUT_OF_RANGE));
        assert_eq!(buffer.cache_flush_invalidate(size - 10, 20), Err(Status::OUT_OF_RANGE));

        unsafe { buffer.unpin().unwrap() };
    }

    #[fuchsia::test]
    fn test_uncached_memory_helper_all_cases() {
        let bti = FakeBti::create().unwrap();
        let size = system_get_page_size() as usize;
        let buffer = ContiguousBuffer::new_with_options(
            &bti,
            &vmar_root_self(),
            size,
            0,
            CacheOptions::Disabled,
        )
        .unwrap();
        assert!(!buffer.is_cache_enabled());

        // 1. Zero-length transfer.
        let mut empty = [];
        assert_eq!(buffer.write(0, &empty), Ok(()));
        assert_eq!(buffer.read(0, &mut empty), Ok(()));

        // 2. Background pattern verification helper.
        let bg_pattern = vec![0xAAu8; size];
        buffer.write(0, &bg_pattern).unwrap();

        let verify_slice = |offset: usize, length: usize, val: u8| {
            let mut read_back = vec![0u8; size];
            buffer.read(0, &mut read_back).unwrap();
            for (i, &b) in read_back.iter().enumerate() {
                if i >= offset && i < offset + length {
                    assert_eq!(b, val, "mismatch at modified index {i}");
                } else {
                    assert_eq!(b, 0xAA, "corruption at background index {i}");
                }
            }
        };

        // 3. Small unaligned transfer (< 8 bytes, at offset 1).
        buffer.write(1, &[0x11; 3]).unwrap();
        verify_slice(1, 3, 0x11);
        buffer.write(0, &bg_pattern).unwrap();

        // 4. Unaligned head alignment (offset 3, length 24).
        buffer.write(3, &[0x22; 24]).unwrap();
        verify_slice(3, 24, 0x22);
        buffer.write(0, &bg_pattern).unwrap();

        // 5. Unaligned tail alignment (offset 8, length 13).
        buffer.write(8, &[0x33; 13]).unwrap();
        verify_slice(8, 13, 0x33);
        buffer.write(0, &bg_pattern).unwrap();

        // 6. Both unaligned head and unaligned tail (offset 5, length 19).
        buffer.write(5, &[0x44; 19]).unwrap();
        verify_slice(5, 19, 0x44);
        buffer.write(0, &bg_pattern).unwrap();

        // 7. Exact word boundary transfer (offset 16, length 32).
        buffer.write(16, &[0x66; 32]).unwrap();
        verify_slice(16, 32, 0x66);

        unsafe { buffer.unpin().unwrap() };
    }
}
