// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Physically contiguous, BTI-pinned, uncached DMA buffers for UFS descriptor lists and command
//! descriptors.

use zx::{self, Bti, BtiOptions, CachePolicy, Pmt, VmarFlags, Vmo};

/// A physically contiguous buffer pinned for DMA and mapped uncached into the root VMAR.
///
/// Dropping the buffer unmaps it but does not unpin it: if the controller might still access the
/// memory, closing the PMT handle makes the kernel quarantine the pages instead of freeing them.
/// Call [`ContiguousDmaBuffer::unpin`] once the controller is known to be quiesced.
// TODO(https://fxbug.dev/567641541): Move to a library in //src/storage/lib and share with cqhci.
pub struct ContiguousDmaBuffer {
    vmo: Vmo,
    pmt: Option<Pmt>,
    phys_address: usize,
    virt_address: usize,
    size: usize,
}

impl std::fmt::Debug for ContiguousDmaBuffer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ContiguousDmaBuffer")
            .field("phys_address", &format_args!("{:#x}", self.phys_address))
            .field("size", &self.size)
            .field("pinned", &self.pmt.is_some())
            .finish()
    }
}

/// Splits a `len`-byte access starting at virtual address `base` into naturally aligned pieces:
/// single bytes up to the first 8-byte boundary, 8-byte words, then any trailing bytes. Yields
/// `(index, width)` pairs relative to `base`.
fn aligned_chunks(base: usize, len: usize) -> impl Iterator<Item = (usize, usize)> {
    let head = len.min(base.wrapping_neg() % 8);
    let body_end = head + (len - head) / 8 * 8;
    (0..head)
        .map(|index| (index, 1))
        .chain((head..body_end).step_by(8).map(|index| (index, 8)))
        .chain((body_end..len).map(|index| (index, 1)))
}

impl ContiguousDmaBuffer {
    #[cfg(test)]
    pub fn is_pinned(&self) -> bool {
        self.pmt.is_some()
    }

    /// Allocates and pins a physically contiguous buffer of at least `size` bytes (rounded up to
    /// a page) and maps it into the root VMAR.
    ///
    /// # Errors
    ///
    /// Returns `INVALID_ARGS` for `size == 0`, or the error from VMO creation, pinning, or
    /// mapping.
    pub fn new(bti: &Bti, size: usize) -> Result<Self, zx::Status> {
        if size == 0 {
            return Err(zx::Status::INVALID_ARGS);
        }
        let page_size = zx::system_get_page_size() as usize;
        let rounded_size =
            size.checked_next_multiple_of(page_size).ok_or(zx::Status::OUT_OF_RANGE)?;
        let vmo = Vmo::create_contiguous(bti, rounded_size, 0)?;

        // Descriptors are shared with the controller, so map them uncached. On AArch64 this is a
        // strongly ordered (device) mapping, hence the naturally aligned accesses below.
        vmo.set_cache_policy(CachePolicy::UnCached)?;

        let mut phys_address = 0usize;
        let pmt = bti.pin(
            BtiOptions::PERM_READ | BtiOptions::PERM_WRITE | BtiOptions::CONTIGUOUS,
            &vmo,
            0,
            rounded_size as u64,
            std::slice::from_mut(&mut phys_address),
        )?;

        let root_vmar = fuchsia_runtime::vmar_root_self();
        let virt_address = match root_vmar.map(
            0,
            &vmo,
            0,
            rounded_size,
            VmarFlags::PERM_READ | VmarFlags::PERM_WRITE | VmarFlags::MAP_RANGE,
        ) {
            Ok(addr) => addr,
            Err(status) => {
                // SAFETY: The buffer was just pinned and its physical address was never handed to
                // the controller, so no DMA can target it.
                unsafe {
                    let _ = pmt.unpin();
                }
                return Err(status);
            }
        };

        Ok(Self { vmo, pmt: Some(pmt), phys_address, virt_address, size: rounded_size })
    }

    /// Returns a reference to the underlying VMO.
    pub fn vmo(&self) -> &Vmo {
        &self.vmo
    }

    /// Returns the base physical address of the contiguous memory buffer.
    pub fn phys_address(&self) -> u64 {
        self.phys_address as u64
    }

    /// Returns the allocated size of the memory buffer.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Returns the virtual address of `offset`, or `OUT_OF_RANGE` if `[offset, offset + len)`
    /// does not fit in the buffer.
    fn checked_address(&self, offset: usize, len: usize) -> Result<usize, zx::Status> {
        match offset.checked_add(len) {
            Some(end) if end <= self.size => Ok(self.virt_address + offset),
            _ => Err(zx::Status::OUT_OF_RANGE),
        }
    }

    /// Reads `data.len()` bytes at `offset` using naturally aligned volatile loads.
    ///
    /// # Errors
    ///
    /// Returns `OUT_OF_RANGE` if the range does not fit in the buffer.
    pub fn read(&self, offset: usize, data: &mut [u8]) -> Result<(), zx::Status> {
        let base = self.checked_address(offset, data.len())?;
        for (index, width) in aligned_chunks(base, data.len()) {
            let address = base + index;
            // SAFETY: `checked_address` guarantees `[base, base + data.len())` lies inside the
            // mapping `[virt_address, virt_address + size)`, which stays mapped for the lifetime
            // of `&self` (only `Drop` unmaps it). `aligned_chunks` yields naturally aligned
            // accesses, as required for the strongly ordered (`UnCached`) mapping on AArch64.
            unsafe {
                if width == 8 {
                    let value = core::ptr::read_volatile(address as *const u64);
                    data[index..index + 8].copy_from_slice(&value.to_ne_bytes());
                } else {
                    data[index] = core::ptr::read_volatile(address as *const u8);
                }
            }
        }
        Ok(())
    }

    /// Writes `data` at `offset` using naturally aligned volatile stores.
    ///
    /// # Errors
    ///
    /// Returns `OUT_OF_RANGE` if the range does not fit in the buffer.
    pub fn write(&mut self, offset: usize, data: &[u8]) -> Result<(), zx::Status> {
        let base = self.checked_address(offset, data.len())?;
        for (index, width) in aligned_chunks(base, data.len()) {
            let address = base + index;
            // SAFETY: `checked_address` guarantees `[base, base + data.len())` lies inside the
            // mapping, which stays mapped for the lifetime of `&mut self`. `&mut self` rules out
            // concurrent CPU writers. `aligned_chunks` yields naturally aligned accesses.
            unsafe {
                if width == 8 {
                    let mut chunk = [0u8; 8];
                    chunk.copy_from_slice(&data[index..index + 8]);
                    core::ptr::write_volatile(address as *mut u64, u64::from_ne_bytes(chunk));
                } else {
                    core::ptr::write_volatile(address as *mut u8, data[index]);
                }
            }
        }
        Ok(())
    }

    /// Zeroes `len` bytes at `offset`.
    ///
    /// # Errors
    ///
    /// Returns `OUT_OF_RANGE` if the range does not fit in the buffer.
    pub fn zero(&mut self, offset: usize, len: usize) -> Result<(), zx::Status> {
        const ZEROES: [u8; 512] = [0; 512];
        let mut done = 0;
        self.checked_address(offset, len)?;
        while done < len {
            let chunk = (len - done).min(ZEROES.len());
            self.write(offset + done, &ZEROES[..chunk])?;
            done += chunk;
        }
        Ok(())
    }

    /// Reads a `T` from the buffer at `offset`.
    ///
    /// # Errors
    ///
    /// Returns `OUT_OF_RANGE` if the range does not fit in the buffer.
    pub fn read_struct<T: zerocopy::FromBytes + zerocopy::IntoBytes>(
        &self,
        offset: usize,
    ) -> Result<T, zx::Status> {
        let mut val = T::new_zeroed();
        self.read(offset, val.as_mut_bytes())?;
        Ok(val)
    }

    /// Writes `val` into the buffer at `offset`.
    ///
    /// # Errors
    ///
    /// Returns `OUT_OF_RANGE` if the range does not fit in the buffer.
    pub fn write_struct<T: zerocopy::IntoBytes + zerocopy::Immutable>(
        &mut self,
        offset: usize,
        val: &T,
    ) -> Result<(), zx::Status> {
        self.write(offset, val.as_bytes())
    }

    /// Unpins the buffer. Later calls are no-ops.
    ///
    /// # Safety
    ///
    /// The controller must no longer be able to access this buffer, e.g. `HCE` reads 0 or the
    /// buffer's physical address was never programmed into the controller.
    pub unsafe fn unpin(&mut self) -> Result<(), zx::Status> {
        match self.pmt.take() {
            // SAFETY: The caller guarantees the controller can no longer access the buffer.
            Some(pmt) => unsafe { pmt.unpin() },
            None => Ok(()),
        }
    }
}

impl Drop for ContiguousDmaBuffer {
    fn drop(&mut self) {
        // The PMT (if still present) is closed without unpinning so the kernel quarantines the
        // pages in case the controller is still running.
        let root_vmar = fuchsia_runtime::vmar_root_self();
        // SAFETY: `[virt_address, virt_address + size)` was mapped in `new()` and is only
        // unmapped here. No references into the mapping outlive `self`.
        unsafe {
            let _ = root_vmar.unmap(self.virt_address, self.size);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fake_bti::FakeBti;

    #[fuchsia::test]
    fn test_aligned_chunks() {
        let chunks = |base, len| aligned_chunks(base, len).collect::<Vec<_>>();
        assert_eq!(chunks(0x1000, 0), []);
        assert_eq!(chunks(0x1000, 3), [(0, 1), (1, 1), (2, 1)]);
        assert_eq!(chunks(0x1000, 16), [(0, 8), (8, 8)]);
        assert_eq!(chunks(0x1003, 4), [(0, 1), (1, 1), (2, 1), (3, 1)]);
        assert_eq!(chunks(0x1006, 12), [(0, 1), (1, 1), (2, 8), (10, 1), (11, 1)]);
        // Every byte is covered exactly once.
        for base in 0x1000..0x1008 {
            for len in 0..40 {
                let total: usize = aligned_chunks(base, len).map(|(_, width)| width).sum();
                assert_eq!(total, len, "base={base:#x} len={len}");
            }
        }
    }

    #[fuchsia::test]
    fn test_contiguous_dma_buffer() {
        let bti = FakeBti::create().expect("Failed to create fake bti");
        let mut buffer = ContiguousDmaBuffer::new(&bti, 8192).expect("Failed to create DMA buffer");
        assert!(buffer.phys_address() > 0);
        assert_eq!(buffer.size(), 8192);

        #[derive(
            Debug,
            Clone,
            Copy,
            PartialEq,
            Eq,
            zerocopy::FromBytes,
            zerocopy::IntoBytes,
            zerocopy::Immutable,
        )]
        #[repr(C)]
        pub struct TestData {
            pub value1: u64,
            pub value2: u64,
        }

        let write_val = TestData { value1: 0x0000000011223344, value2: 0x5566778899AABBCC };
        buffer.write_struct(3, &write_val).unwrap();
        let read_val: TestData = buffer.read_struct(3).unwrap();
        assert_eq!(read_val, write_val);

        buffer.zero(3, 16).unwrap();
        assert_eq!(buffer.read_struct::<TestData>(3).unwrap(), TestData { value1: 0, value2: 0 });

        assert_eq!(buffer.write(8190, &[0; 4]), Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(buffer.read(usize::MAX, &mut [0; 4]), Err(zx::Status::OUT_OF_RANGE));
        assert_eq!(buffer.zero(8192, 1), Err(zx::Status::OUT_OF_RANGE));

        // SAFETY: The buffer's physical address was never given to a controller.
        unsafe {
            buffer.unpin().unwrap();
        }
    }
}
