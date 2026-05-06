// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::alloc::Layout;
use core::ptr::NonNull;

/// Error type for allocation failures.
#[derive(Debug, Eq, PartialEq)]
pub struct AllocError;

impl core::fmt::Display for AllocError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("allocation failure")
    }
}

impl core::error::Error for AllocError {}

/// Trait for allocators used by Box and other collections.
///
/// This trait mirrors the `core::alloc::Allocator` trait,
/// returning `Result<NonNull<[u8]>, AllocError>` to provide the actual
/// size allocated.
///
/// Implementations of this trait must tolerate being passed zero-sized layouts.
/// For zero-sized allocations, implementations should return a dangling pointer
/// with the appropriate alignment, and `deallocate` should be a no-op for such pointers.
pub trait Allocator: Clone {
    /// Allocates memory as described by the given `layout`.
    ///
    /// If the layout has a size of zero, this method returns a dangling pointer.
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError>;

    /// # Safety
    ///
    /// - The pointer must have been allocated by `allocate` with the same layout.
    ///
    /// If the layout has a size of zero, this method is a no-op.
    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout);

    /// # Safety
    ///
    /// - `ptr` must denote a block of memory currently allocated via this allocator.
    /// - `old_layout` must fit that block of memory (The `new_layout` argument need not fit it.).
    /// - `new_layout.size()` must be greater than or equal to `old_layout.size()`.
    ///
    /// Note that `new_layout.align()` need not be the same as `old_layout.align()`.
    ///
    /// If the new layout has a size of zero, this method returns a dangling pointer.
    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError>;

    /// # Safety
    ///
    /// - `ptr` must denote a block of memory currently allocated via this allocator.
    /// - `old_layout` must fit that block of memory (The `new_layout` argument need not fit it.).
    /// - `new_layout.size()` must be less than or equal to `old_layout.size()`.
    ///
    /// Note that `new_layout.align()` need not be the same as `old_layout.align()`.
    ///
    /// If the new layout has a size of zero, this method deallocates the memory and returns a dangling pointer.
    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError>;

    /// Allocates zeroed memory as described by the given `layout`.
    ///
    /// If the layout has a size of zero, this method returns a dangling pointer.
    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError>;
}

/// Default allocator that uses the global allocator (userspace) or kernel allocator.
#[derive(Clone, Default)]
pub struct DefaultAllocator;

impl Allocator for DefaultAllocator {
    fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::from_ref(&[]));
        }
        let ptr = unsafe { crate::alloc(layout).ok_or(AllocError)? };
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }

    unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
        if layout.size() == 0 {
            return;
        }
        unsafe { crate::dealloc(ptr.as_ptr(), layout) }
    }

    unsafe fn grow(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        assert!(new_layout.size() >= old_layout.size());
        if new_layout.size() == 0 {
            return Ok(NonNull::from_ref(&[]));
        }
        if old_layout.size() == 0 {
            return self.allocate(new_layout);
        }
        let ptr = unsafe {
            crate::realloc(ptr.as_ptr(), old_layout, new_layout.size()).ok_or(AllocError)?
        };
        Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()))
    }

    unsafe fn shrink(
        &self,
        ptr: NonNull<u8>,
        old_layout: Layout,
        new_layout: Layout,
    ) -> Result<NonNull<[u8]>, AllocError> {
        assert!(new_layout.size() <= old_layout.size());
        if new_layout.size() == 0 {
            unsafe { self.deallocate(ptr, old_layout) };
            return Ok(NonNull::from_ref(&[]));
        }
        if old_layout.size() == 0 {
            return Ok(NonNull::from_ref(&[]));
        }
        let ptr = unsafe {
            crate::realloc(ptr.as_ptr(), old_layout, new_layout.size()).ok_or(AllocError)?
        };
        Ok(NonNull::slice_from_raw_parts(ptr, new_layout.size()))
    }

    fn allocate_zeroed(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
        if layout.size() == 0 {
            return Ok(NonNull::from_ref(&[]));
        }
        let ptr = unsafe { crate::alloc_zeroed(layout).ok_or(AllocError)? };
        Ok(NonNull::slice_from_raw_parts(ptr, layout.size()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_allocator() {
        let layout = Layout::from_size_align(10, 1).unwrap();
        let ptr = DefaultAllocator::default().allocate(layout).unwrap();
        unsafe {
            DefaultAllocator::default().deallocate(ptr.cast::<u8>(), layout);
        }
    }

    #[test]
    fn test_default_allocator_grow_shrink() {
        let layout = Layout::from_size_align(10, 1).unwrap();
        let ptr = DefaultAllocator::default().allocate(layout).unwrap();

        let new_layout = Layout::from_size_align(20, 1).unwrap();
        let grown_ptr = unsafe {
            DefaultAllocator::default().grow(ptr.cast::<u8>(), layout, new_layout).unwrap()
        };
        assert!(grown_ptr.len() >= 20);

        let shrunk_layout = Layout::from_size_align(5, 1).unwrap();
        let shrunk_ptr = unsafe {
            DefaultAllocator::default()
                .shrink(grown_ptr.cast::<u8>(), new_layout, shrunk_layout)
                .unwrap()
        };
        assert!(shrunk_ptr.len() >= 5);

        unsafe {
            DefaultAllocator::default().deallocate(shrunk_ptr.cast::<u8>(), shrunk_layout);
        }
    }

    #[test]
    fn test_default_allocator_zeroed() {
        let layout = Layout::from_size_align(10, 1).unwrap();
        let ptr = DefaultAllocator::default().allocate_zeroed(layout).unwrap();

        // Verify it is zeroed!
        let slice = unsafe { ptr.as_ref() };
        for &b in slice {
            assert_eq!(b, 0);
        }

        unsafe {
            DefaultAllocator::default().deallocate(ptr.cast::<u8>(), layout);
        }
    }

    #[test]
    fn test_default_allocator_zero_sized() {
        let allocator = DefaultAllocator::default();

        let layout0 = Layout::from_size_align(0, 1).unwrap();
        let ptr0 = allocator.allocate(layout0).unwrap();
        assert_eq!(ptr0.len(), 0);

        let ptr0_zeroed = allocator.allocate_zeroed(layout0).unwrap();
        assert_eq!(ptr0_zeroed.len(), 0);

        unsafe {
            allocator.deallocate(ptr0.cast::<u8>(), layout0);
            allocator.deallocate(ptr0_zeroed.cast::<u8>(), layout0);
        }

        let grown0 = unsafe { allocator.grow(ptr0.cast::<u8>(), layout0, layout0).unwrap() };
        assert_eq!(grown0.len(), 0);

        let layout10 = Layout::from_size_align(10, 1).unwrap();
        let grown10 = unsafe { allocator.grow(ptr0.cast::<u8>(), layout0, layout10).unwrap() };
        assert!(grown10.len() >= 10);

        let shrunk0 = unsafe { allocator.shrink(grown10.cast::<u8>(), layout10, layout0).unwrap() };
        assert_eq!(shrunk0.len(), 0);

        let shrunk0_again =
            unsafe { allocator.shrink(ptr0.cast::<u8>(), layout0, layout0).unwrap() };
        assert_eq!(shrunk0_again.len(), 0);
    }
}
