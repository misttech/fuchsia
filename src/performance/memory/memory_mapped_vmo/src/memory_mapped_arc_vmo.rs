// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::marker::PhantomData;
use std::mem::{ManuallyDrop, size_of};
use std::ops::{Deref, DerefMut};
use std::sync::atomic::{AtomicUsize, Ordering};

use crate::ReadWriteMemoryMappedVmo;
use crate::memory_mapped_vmo::MemoryMappedVmo;

struct ControlBlock<M> {
    metadata: M,
    ref_count: AtomicUsize,
}

/// Reference-counted MemoryMappedVmo backed by an auxiliary control VMO to store the reference
/// count and, optionally, extra shared metadata.
pub struct MemoryMappedArcVmo<const WRITABLE: bool, M: Send + Sync = ()> {
    control: ManuallyDrop<ReadWriteMemoryMappedVmo>,
    storage: ManuallyDrop<MemoryMappedVmo<WRITABLE>>,
    _marker: PhantomData<M>,
}

impl<const WRITABLE: bool, M: Send + Sync> MemoryMappedArcVmo<WRITABLE, M> {
    /// Wraps a MemoryMappedVmo into a MemoryMappedArcVmo with the given extra metadata.
    ///
    /// # Safety
    /// The caller must either guarantee that the VMO is not modified by others while the returned
    /// instance is alive or that accesses are synchronized in application-specific ways.
    pub unsafe fn wrap(
        storage: MemoryMappedVmo<WRITABLE>,
        metadata: M,
    ) -> Result<Self, zx::Status> {
        unsafe { Self::wrap_internal(storage, metadata, None) }
    }

    /// Like wrap, but also sets the control VMO name.
    ///
    /// # Safety
    /// The caller must either guarantee that the VMO is not modified by others while the returned
    /// instance is alive or that accesses are synchronized in application-specific ways.
    pub unsafe fn wrap_with_name(
        storage: MemoryMappedVmo<WRITABLE>,
        metadata: M,
        control_vmo_name: &zx::Name,
    ) -> Result<Self, zx::Status> {
        unsafe { Self::wrap_internal(storage, metadata, Some(control_vmo_name)) }
    }

    unsafe fn wrap_internal(
        storage: MemoryMappedVmo<WRITABLE>,
        metadata: M,
        control_vmo_name: Option<&zx::Name>,
    ) -> Result<Self, zx::Status> {
        // Create the control VMO.
        let control_vmo = zx::Vmo::create(size_of::<ControlBlock<M>>() as u64)?;
        if let Some(name) = control_vmo_name {
            control_vmo.set_name(name)?;
        }

        // Fill it with initial data.
        let mut control = unsafe { ReadWriteMemoryMappedVmo::new(&control_vmo)? };
        let control_ptr = control.validate_and_get_mut_ptr::<ControlBlock<M>>(0, 1).unwrap();
        unsafe {
            control_ptr.write(ControlBlock { metadata, ref_count: AtomicUsize::new(1) });
        }

        Ok(MemoryMappedArcVmo {
            control: ManuallyDrop::new(control),
            storage: ManuallyDrop::new(storage),
            _marker: PhantomData,
        })
    }

    fn control_block(&self) -> &ControlBlock<M> {
        let ptr = self.control.validate_and_get_ptr::<ControlBlock<M>>(0, 1).unwrap();
        unsafe { &*ptr }
    }

    /// Returns a shared reference to the metadata stored in the control block.
    pub fn metadata(&self) -> &M {
        &self.control_block().metadata
    }
}

impl<M: Send + Sync> MemoryMappedArcVmo<true, M> {
    /// Downgrades a read-write mapping handle into a read-only mapping handle without unmapping.
    pub fn into_readonly(self) -> MemoryMappedArcVmo<false, M> {
        let mut this = ManuallyDrop::new(self);
        // SAFETY: self is now wrapped in ManuallyDrop. Therefore, control and storage are moved
        // once without running self's Drop implementation.
        unsafe {
            MemoryMappedArcVmo {
                control: ManuallyDrop::new(ManuallyDrop::take(&mut this.control)),
                storage: ManuallyDrop::new(ManuallyDrop::take(&mut this.storage).into_readonly()),
                _marker: PhantomData,
            }
        }
    }
}

impl<const WRITABLE: bool, M: Send + Sync> Deref for MemoryMappedArcVmo<WRITABLE, M> {
    type Target = MemoryMappedVmo<WRITABLE>;

    fn deref(&self) -> &Self::Target {
        &self.storage
    }
}

impl<M: Send + Sync> DerefMut for MemoryMappedArcVmo<true, M> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.storage
    }
}

impl<const WRITABLE: bool, M: Send + Sync> Clone for MemoryMappedArcVmo<WRITABLE, M> {
    fn clone(&self) -> Self {
        // Increment the reference count.
        self.control_block().ref_count.fetch_add(1, Ordering::Relaxed);

        // SAFETY: ref_count was just incremented, ensuring Drop will only unmap control and storage
        // when the last clone is dropped.
        unsafe {
            MemoryMappedArcVmo {
                control: self.control.create_alias_with_manually_drop(),
                storage: self.storage.create_alias_with_manually_drop(),
                _marker: PhantomData,
            }
        }
    }
}

impl<const WRITABLE: bool, M: Send + Sync> Drop for MemoryMappedArcVmo<WRITABLE, M> {
    fn drop(&mut self) {
        if self.control_block().ref_count.fetch_sub(1, Ordering::AcqRel) != 1 {
            return; // This was not the last reference.
        }

        let control_ptr = self.control.validate_and_get_mut_ptr::<ControlBlock<M>>(0, 1).unwrap();
        // SAFETY: We are the last reference to control and storage. No others exist anymore.
        unsafe {
            ManuallyDrop::drop(&mut self.storage);

            std::ptr::drop_in_place(control_ptr);
            ManuallyDrop::drop(&mut self.control);
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::{ReadWriteMemoryMappedArcVmo, ReadWriteMemoryMappedVmo};
    use std::mem::size_of;
    use std::sync::atomic::{AtomicBool, Ordering};

    const TEST_DATA: u64 = 42;
    const TEST_DATA_SIZE: usize = size_of::<u64>();

    struct DropOnceChecker<'a>(&'a AtomicBool);

    impl Drop for DropOnceChecker<'_> {
        fn drop(&mut self) {
            assert!(!self.0.swap(true, Ordering::SeqCst));
        }
    }

    #[test]
    fn test_clone_and_drop_metadata() {
        let vmo = zx::Vmo::create(TEST_DATA_SIZE as u64).unwrap();

        // Create an initial MemoryMappedVmo and write data into it.
        let mut rw = unsafe { ReadWriteMemoryMappedVmo::new(&vmo) }.unwrap();
        *rw.get_object_mut::<u64>(0).unwrap() = TEST_DATA;

        // Wrap with reference-counting and with sentinel metadata that reports when the mapping
        // is dropped.
        let dropped = AtomicBool::new(false);
        let m1 =
            unsafe { ReadWriteMemoryMappedArcVmo::wrap(rw, DropOnceChecker(&dropped)) }.unwrap();
        let m2 = m1.clone();

        // Drop one reference.
        assert_eq!(m2.control_block().ref_count.load(Ordering::SeqCst), 2);
        drop(m1);

        // Verify that the other reference is still valid.
        assert_eq!(m2.control_block().ref_count.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), false);
        assert_eq!(*m2.get_object::<u64>(0).unwrap(), TEST_DATA);

        // Verify downgrading into a read-only mapping.
        let m2 = m2.into_readonly();
        assert_eq!(m2.control_block().ref_count.load(Ordering::SeqCst), 1);
        assert_eq!(dropped.load(Ordering::SeqCst), false);
        assert_eq!(*m2.get_object::<u64>(0).unwrap(), TEST_DATA);

        drop(m2);
        assert_eq!(dropped.load(Ordering::SeqCst), true);
    }
}
