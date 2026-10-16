// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Memory mapping utilities for cached VMOs in MMIO tests.

use mmio::region::{MmioRegion, UnsafeMmio};
use mmio::vmo::{VmoMapping, VmoMemory};

use crate::atomic::{AtomicMmioPtr, AtomicOperand};

/// A wrapper around [`VmoMemory`] guaranteed to be mapped with cached memory
/// policy.
pub struct CachedVmoMemory(VmoMemory);

impl CachedVmoMemory {
    /// Creates a new cached vmo memory from mapped memory.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the mapped VMO is mapped with
    /// `zx::CachePolicy::Cached`.
    pub unsafe fn new_unchecked(inner: VmoMemory) -> Self {
        Self(inner)
    }

    /// Returns the contained mapped VMO.
    pub fn get(&self) -> &VmoMemory {
        &self.0
    }

    /// Maps `vmo` at `offset` with `size` ensuring the correct cache policy.
    pub fn map(offset: usize, size: usize, vmo: zx::Vmo) -> Result<MmioRegion<Self>, zx::Status> {
        let region = VmoMapping::map_with_cache_policy(offset, size, vmo, zx::CachePolicy::Cached)?;
        Ok(region.map(Self))
    }

    /// Creates a new VMO of `size` bytes and maps it into memory with cached
    /// cache policy.
    pub fn new_mapped(size: usize) -> Result<MmioRegion<Self>, zx::Status> {
        let size_u64 = u64::try_from(size).map_err(|_| zx::Status::OUT_OF_RANGE)?;
        let vmo = zx::Vmo::create(size_u64)?;
        Self::map(0, size, vmo)
    }
}

impl UnsafeMmio for CachedVmoMemory {
    fn len(&self) -> usize {
        self.0.len()
    }

    fn align_offset(&self, align: usize) -> usize {
        self.0.align_offset(align)
    }

    unsafe fn load8_unchecked(&self, offset: usize) -> u8 {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.load8_unchecked(offset) }
    }

    unsafe fn load16_unchecked(&self, offset: usize) -> u16 {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.load16_unchecked(offset) }
    }

    unsafe fn load32_unchecked(&self, offset: usize) -> u32 {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.load32_unchecked(offset) }
    }

    unsafe fn load64_unchecked(&self, offset: usize) -> u64 {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.load64_unchecked(offset) }
    }

    unsafe fn store8_unchecked(&self, offset: usize, v: u8) {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.store8_unchecked(offset, v) }
    }

    unsafe fn store16_unchecked(&self, offset: usize, v: u16) {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.store16_unchecked(offset, v) }
    }

    unsafe fn store32_unchecked(&self, offset: usize, v: u32) {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.store32_unchecked(offset, v) }
    }

    unsafe fn store64_unchecked(&self, offset: usize, v: u64) {
        // SAFETY: Passthrough from caller.
        unsafe { self.0.store64_unchecked(offset, v) }
    }

    fn write_barrier(&self) {
        self.0.write_barrier()
    }

    fn read_barrier(&self) {
        self.0.read_barrier()
    }
}

impl AtomicMmioPtr for CachedVmoMemory {
    fn ptr<T: AtomicOperand>(&self, offset: usize) -> std::ptr::NonNull<T> {
        self.0.ptr(offset)
    }
}
