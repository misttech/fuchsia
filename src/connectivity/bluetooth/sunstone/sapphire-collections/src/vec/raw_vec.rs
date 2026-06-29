// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::alloc::Layout;
use core::marker::PhantomData;
use core::mem::MaybeUninit;
use core::ptr::NonNull;

use crate::AllocError;
use crate::storage::{Storage, StorageFamily};

/// Manages the underlying allocation behind a buffer of elements
///
/// A RawVec is the building block to many data structures. It takes care of
/// growing/shrinking the underlying allocation and providing higher-level
/// data structures the underlying buffer to actually store elements.
pub struct RawVec<T, A: StorageFamily> {
    buffer: MaybeUninit<<A::Storage<T> as Storage>::Handle>,
    capacity: usize,
    allocator: A::Storage<T>,
    _elements: PhantomData<[T]>,
}

type StorageHandle<T, A> = <<A as StorageFamily>::Storage<T> as Storage>::Handle;

impl<A: StorageFamily, T> RawVec<T, A> {
    /// Creates the `RawVec` in the given allocator.
    pub fn new_in(allocator: A::Storage<T>) -> Self {
        Self { buffer: MaybeUninit::uninit(), capacity: 0, allocator, _elements: PhantomData }
    }

    /// Default-constructs an empty `RawVec`.
    pub fn new() -> Self
    where
        Self: Default,
    {
        Default::default()
    }

    /// Returns the total capacity of the buffer.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Attempts to grow the buffer
    ///
    /// If successful, all of the data in the original buffer will be copied over
    /// to the beginning of the new buffer.
    ///
    /// Returns `Err(AllocError)` if the allocation fails.
    pub fn grow(&mut self) -> Result<(), AllocError> {
        match self.get_handle() {
            Some(handle) => {
                let new_capacity = self.capacity.checked_mul(2).ok_or(AllocError)?;
                let (new_handle, new_size) = unsafe {
                    self.allocator.grow(
                        Layout::array::<T>(self.capacity).map_err(|_| AllocError)?,
                        Layout::array::<T>(new_capacity).map_err(|_| AllocError)?,
                        handle,
                    )?
                };
                let element_size = size_of::<T>();
                assert!(new_size >= element_size * new_capacity);
                self.capacity =
                    if element_size == 0 { usize::MAX } else { new_size / element_size };
                self.set_handle(new_handle);
                Ok(())
            }
            None => {
                let (new_handle, new_size) =
                    self.allocator.allocate(Layout::array::<T>(1).map_err(|_| AllocError)?)?;
                let element_size = size_of::<T>();
                assert!(new_size >= element_size);
                self.capacity =
                    if element_size == 0 { usize::MAX } else { new_size / element_size };
                self.set_handle(new_handle);
                Ok(())
            }
        }
    }

    fn set_handle(&mut self, handle: StorageHandle<T, A>) {
        // NOTE: No need to drop the old handle since A::Handle: Copy
        self.buffer.write(handle);
    }

    fn get_handle(&self) -> Option<StorageHandle<T, A>> {
        if self.capacity == 0 {
            None
        } else {
            // SAFETY: We have a handle because our capacity is non-zero
            Some(*unsafe { self.buffer.assume_init_ref() })
        }
    }

    pub fn as_ptr(&self) -> NonNull<[MaybeUninit<T>]> {
        match self.get_handle() {
            Some(handle) => {
                let ptr = unsafe { self.allocator.resolve(handle).cast() };
                NonNull::slice_from_raw_parts(ptr, self.capacity)
            }
            None => NonNull::slice_from_raw_parts(NonNull::dangling(), 0),
        }
    }

    pub fn as_ptr_mut(&mut self) -> NonNull<[MaybeUninit<T>]> {
        match self.get_handle() {
            Some(handle) => {
                let ptr = unsafe { self.allocator.resolve(handle).cast() };
                NonNull::slice_from_raw_parts(ptr, self.capacity)
            }
            None => NonNull::slice_from_raw_parts(NonNull::dangling(), 0),
        }
    }

    /// Returns a shared reference to the underlying buffer.
    pub fn buffer(&self) -> &[MaybeUninit<T>] {
        unsafe { self.as_ptr().as_ref() }
    }

    /// Returns a mutable reference to the underlying buffer.
    pub fn buffer_mut(&mut self) -> &mut [MaybeUninit<T>] {
        unsafe { self.as_ptr_mut().as_mut() }
    }
}

impl<T, A: StorageFamily> Default for RawVec<T, A>
where
    A::Storage<T>: Default,
{
    fn default() -> Self {
        Self::new_in(Default::default())
    }
}

impl<T, A: StorageFamily> Drop for RawVec<T, A> {
    fn drop(&mut self) {
        if let Some(handle) = self.get_handle() {
            unsafe {
                self.allocator.deallocate(
                    Layout::array::<T>(self.capacity).expect(
                        "Layout must have been constructed before in order to have this allocation",
                    ),
                    handle,
                )
            }
        }
    }
}
