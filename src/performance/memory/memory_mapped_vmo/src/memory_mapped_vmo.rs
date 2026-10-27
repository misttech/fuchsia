// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::mem::{ManuallyDrop, align_of, size_of};
use std::ops::Deref;
use zerocopy::FromBytes;

use crate::Error;

/// Memory-maps a VMO and mediates access to its memory.
#[repr(C)]
pub struct MemoryMappedVmo<const WRITABLE: bool> {
    map_addr: usize,
    vmo_size: usize,
}

impl<const WRITABLE: bool> MemoryMappedVmo<WRITABLE> {
    /// Maps a VMO into memory (read-only if WRITABLE is false, read-write if WRITABLE is true).
    ///
    /// # Safety
    /// The caller must either guarantee that the VMO is not modified by others while the returned
    /// instance is alive or that accesses are synchronized in application-specific ways.
    pub unsafe fn new(vmo: &zx::Vmo) -> Result<Self, zx::Status> {
        let vmo_size = vmo.get_content_size()? as usize;

        let mut flags = zx::VmarFlags::PERM_READ
            | zx::VmarFlags::ALLOW_FAULTS
            | zx::VmarFlags::REQUIRE_NON_RESIZABLE;
        if WRITABLE {
            flags |= zx::VmarFlags::PERM_WRITE;
        }

        let map_addr = fuchsia_runtime::vmar_root_self().map(0, vmo, 0, vmo_size, flags)?;
        Ok(MemoryMappedVmo { map_addr, vmo_size })
    }

    /// Creates a MemoryMappedVmo pointing to the *same* VMAR mapping.
    ///
    /// # Safety
    /// The same requirements as new() still apply. On top of that, the caller now becomes
    /// responsible for ensuring that Drop is only executed once and after all the MemoryMappedVmo
    /// referencing the same mapping have gone out of scope.
    pub(crate) unsafe fn create_alias_with_manually_drop(&self) -> ManuallyDrop<Self> {
        ManuallyDrop::new(MemoryMappedVmo { map_addr: self.map_addr, vmo_size: self.vmo_size })
    }

    /// Returns the number of usable bytes in the VMO (i.e. its ZX_PROP_VMO_CONTENT_SIZE property,
    /// which is not rounded to the page size).
    pub fn vmo_size(&self) -> usize {
        self.vmo_size
    }

    /// Given an element type, a base offset within the VMO and a number of elements, verifies that
    /// the offset is suitably aligned and that the range of the elements fits in the VMO bounds. If
    /// both conditions are satisfied, return a const pointer to its first element.
    pub(crate) fn validate_and_get_ptr<T>(
        &self,
        byte_offset: usize,
        num_elements: usize,
    ) -> Result<*const T, Error> {
        if byte_offset % align_of::<T>() == 0 {
            if let Some(num_bytes) = size_of::<T>().checked_mul(num_elements) {
                if let Some(end) = byte_offset.checked_add(num_bytes) {
                    if end <= self.vmo_size {
                        return Ok((self.map_addr + byte_offset) as *const T);
                    }
                }
            }
        }

        Err(Error::InvalidInput)
    }

    /// Returns a reference to a slice of elements in the VMO.
    ///
    /// This method validates the alignment and the bounds against the VMO size.
    pub fn get_slice<'a, T: FromBytes>(
        &'a self,
        byte_offset: usize,
        num_elements: usize,
    ) -> Result<&'a [T], Error> {
        let ptr = self.validate_and_get_ptr(byte_offset, num_elements)?;
        unsafe { Ok(std::slice::from_raw_parts(ptr, num_elements)) }
    }

    /// Returns a reference to an element in the VMO.
    ///
    /// This method validates the alignment and the bounds against the VMO size.
    pub fn get_object<'a, T: FromBytes>(&'a self, byte_offset: usize) -> Result<&'a T, Error> {
        let ptr = self.validate_and_get_ptr(byte_offset, 1)?;
        unsafe { Ok(&*ptr) }
    }
}

impl MemoryMappedVmo<true> {
    /// Like validate_and_get_ptr, but returns a mut pointer.
    pub(crate) fn validate_and_get_mut_ptr<T>(
        &mut self,
        byte_offset: usize,
        num_elements: usize,
    ) -> Result<*mut T, Error> {
        Ok(self.validate_and_get_ptr::<T>(byte_offset, num_elements)? as *mut T)
    }

    /// Returns a mutable reference to a slice of elements in the VMO.
    ///
    /// This method validates the alignment and the bounds against the VMO size.
    pub fn get_slice_mut<'a, T: FromBytes>(
        &'a mut self,
        byte_offset: usize,
        num_elements: usize,
    ) -> Result<&'a mut [T], Error> {
        let ptr = self.validate_and_get_mut_ptr(byte_offset, num_elements)?;
        unsafe { Ok(std::slice::from_raw_parts_mut(ptr, num_elements)) }
    }

    /// Returns a mutable reference to an element in the VMO.
    ///
    /// This method validates the alignment and the bounds against the VMO size.
    pub fn get_object_mut<'a, T: FromBytes>(
        &'a mut self,
        byte_offset: usize,
    ) -> Result<&'a mut T, Error> {
        let ptr = self.validate_and_get_mut_ptr(byte_offset, 1)?;
        unsafe { Ok(&mut *ptr) }
    }

    /// Downgrades a read-write mapping handle into a read-only mapping handle without unmapping.
    pub fn into_readonly(self) -> MemoryMappedVmo<false> {
        let this = ManuallyDrop::new(self);
        MemoryMappedVmo { map_addr: this.map_addr, vmo_size: this.vmo_size }
    }
}

impl Deref for MemoryMappedVmo<true> {
    type Target = MemoryMappedVmo<false>;

    fn deref(&self) -> &Self::Target {
        // SAFETY: MemoryMappedVmo<true> and MemoryMappedVmo<false> have identical #[repr(C)]
        // layout, and a read-write mapping is always valid for read-only operations.
        unsafe { &*(self as *const MemoryMappedVmo<true> as *const MemoryMappedVmo<false>) }
    }
}

impl<const WRITABLE: bool> Drop for MemoryMappedVmo<WRITABLE> {
    fn drop(&mut self) {
        // SAFETY: We owned the mapping.
        unsafe {
            fuchsia_runtime::vmar_root_self()
                .unmap(self.map_addr, self.vmo_size)
                .expect("failed to unmap MemoryMappedVmo");
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{Error, ReadOnlyMemoryMappedVmo, ReadWriteMemoryMappedVmo};
    use assert_matches::assert_matches;
    use std::mem::size_of;

    // Test data used by some of the following tests.
    const TEST_DATA: [u64; 4] = [11, 22, 33, 44];
    const TEST_DATA_SIZE: usize = size_of::<u64>() * TEST_DATA.len();

    #[test]
    fn test_vmo_size() {
        let vmo = zx::Vmo::create(TEST_DATA_SIZE as u64).unwrap();
        let m = unsafe { ReadWriteMemoryMappedVmo::new(&vmo) }.unwrap();

        assert_eq!(m.vmo_size(), TEST_DATA_SIZE);
    }

    #[test]
    fn test_write_objects_read_slice() {
        let vmo = zx::Vmo::create(TEST_DATA_SIZE as u64).unwrap();

        // Fill VMO with test data as individual objects.
        {
            let mut m = unsafe { ReadWriteMemoryMappedVmo::new(&vmo) }.unwrap();
            for (i, val) in TEST_DATA.iter().enumerate() {
                *m.get_object_mut(size_of::<u64>() * i).unwrap() = *val;
            }
        }

        // Verify that we can read them back correctly as a slice.
        {
            let m = unsafe { ReadOnlyMemoryMappedVmo::new(&vmo) }.unwrap();
            assert_eq!(*m.get_slice::<u64>(0, 4).unwrap(), TEST_DATA);
        }
    }

    #[test]
    fn test_write_slice_read_objects() {
        let vmo = zx::Vmo::create(TEST_DATA_SIZE as u64).unwrap();

        // Fill VMO with test data as a slice.
        {
            let mut m = unsafe { ReadWriteMemoryMappedVmo::new(&vmo) }.unwrap();
            m.get_slice_mut(0, 4).unwrap().copy_from_slice(&TEST_DATA);
        }

        // Verify that we can read it back correctly as individual objects.
        {
            let m = unsafe { ReadOnlyMemoryMappedVmo::new(&vmo) }.unwrap();
            for (i, expected_val) in TEST_DATA.iter().enumerate() {
                let actual_val: &u64 = m.get_object(size_of::<u64>() * i).unwrap();
                assert_eq!(*actual_val, *expected_val, "value mismatch at i={}", i);
            }
        }
    }

    #[test]
    fn test_write_slice_read_subslices() {
        const COUNT: usize = 4;
        let vmo = zx::Vmo::create((size_of::<u64>() * COUNT) as u64).unwrap();

        // Fill VMO with test data.
        let mut m = unsafe { ReadWriteMemoryMappedVmo::new(&vmo) }.unwrap();
        m.get_slice_mut::<u64>(0, COUNT).unwrap().copy_from_slice(&[11, 22, 33, 44]);

        // Verify that we can read subslices correctly.
        const SECOND_ELEM_BYTE_OFFSET: usize = size_of::<u64>();
        assert_eq!(*m.get_slice::<u64>(SECOND_ELEM_BYTE_OFFSET, 0).unwrap(), []);
        assert_eq!(*m.get_slice::<u64>(SECOND_ELEM_BYTE_OFFSET, 1).unwrap(), [22]);
        assert_eq!(*m.get_slice::<u64>(SECOND_ELEM_BYTE_OFFSET, 2).unwrap(), [22, 33]);
        assert_eq!(*m.get_slice::<u64>(SECOND_ELEM_BYTE_OFFSET, 3).unwrap(), [22, 33, 44]);
    }

    #[test]
    fn test_uninitialized_is_zero() {
        const COUNT: usize = 4;
        let vmo = zx::Vmo::create((size_of::<u64>() * COUNT) as u64).unwrap();
        let m = unsafe { ReadOnlyMemoryMappedVmo::new(&vmo) }.unwrap();

        // Verify that the value of uninitialized data is zero.
        assert_eq!(*m.get_slice::<u64>(0, COUNT).unwrap(), [0; COUNT]);
    }

    #[test]
    fn test_range_errors() {
        const COUNT: usize = 4;
        let vmo = zx::Vmo::create((size_of::<u64>() * COUNT) as u64).unwrap();
        let m = unsafe { ReadOnlyMemoryMappedVmo::new(&vmo) }.unwrap();

        // Reading at a misaligned offset should fail.
        const MISALIGNED_OFFSET: usize = size_of::<u64>() - 1;
        assert_matches!(m.get_object::<u64>(MISALIGNED_OFFSET), Err(Error::InvalidInput));

        // Reading an out-of-bounds range should fail.
        const SECOND_ELEM_BYTE_OFFSET: usize = size_of::<u64>();
        assert_matches!(
            m.get_slice::<u64>(SECOND_ELEM_BYTE_OFFSET, COUNT),
            Err(Error::InvalidInput)
        );
    }

    #[test]
    fn test_into_readonly() {
        let vmo = zx::Vmo::create(TEST_DATA_SIZE as u64).unwrap();
        let mut m = unsafe { ReadWriteMemoryMappedVmo::new(&vmo) }.unwrap();
        m.get_slice_mut(0, 4).unwrap().copy_from_slice(&TEST_DATA);

        let m: ReadOnlyMemoryMappedVmo = m.into_readonly();
        assert_eq!(*m.get_slice::<u64>(0, 4).unwrap(), TEST_DATA);
    }
}
