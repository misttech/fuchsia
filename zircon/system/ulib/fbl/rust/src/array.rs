// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use core::ops::{Deref, DerefMut};
use kalloc::{AllocError, Allocator, Box, DefaultAllocator};

/// A fixed-size array that takes ownership of its elements.
/// This is a Rust analog to `fbl::Array` in C++.
pub struct Array<T, A: Allocator = DefaultAllocator> {
    buf: Box<[T], A>,
}

zr::static_assert!(core::mem::size_of::<Array<u32>>() == 16);
zr::static_assert!(core::mem::align_of::<Array<u32>>() == 8);

impl<T, A: Allocator> Array<T, A> {
    /// Creates an empty array with the given allocator.
    pub const fn new_in(allocator: A) -> Self {
        Self { buf: Box::empty_slice_in(allocator) }
    }

    /// Creates an array from a Box.
    pub fn from_box(buf: Box<[T], A>) -> Self {
        Self { buf }
    }

    /// Allocates a new array of the given length, default-constructing each element.
    pub fn try_new_in(len: usize, allocator: A) -> Result<Self, AllocError>
    where
        T: Default,
    {
        let mut b = Box::<[T], A>::try_new_uninit_slice_in(len, allocator)?;
        for i in 0..len {
            b[i].write(T::default());
        }
        // SAFETY: All elements have been initialized.
        Ok(Self { buf: unsafe { b.assume_init() } })
    }

    /// Returns the number of elements in the array.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Returns true if the array is empty.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Consumes the array and returns the inner Box.
    pub fn into_box(self) -> Box<[T], A> {
        self.buf
    }
}

impl<T> Array<T, DefaultAllocator> {
    /// Creates an empty array using the default allocator.
    pub const fn new() -> Self {
        Self { buf: Box::empty_slice() }
    }

    /// Allocates a new array of the given length, default-constructing each element.
    pub fn try_new(len: usize) -> Result<Self, AllocError>
    where
        T: Default,
    {
        Self::try_new_in(len, DefaultAllocator)
    }
}

impl<T, A: Allocator> Deref for Array<T, A> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        &self.buf
    }
}

impl<T, A: Allocator> DerefMut for Array<T, A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.buf
    }
}

impl<T> Default for Array<T, DefaultAllocator> {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use core::ptr::NonNull;

    #[derive(Debug, PartialEq, Eq)]
    struct TestState {
        live_obj_count: Cell<usize>,
        ctor_count: Cell<usize>,
        dtor_count: Cell<usize>,
        alloc_count: Cell<usize>,
        fail_threshold: Cell<usize>,
    }

    impl Default for TestState {
        fn default() -> Self {
            Self {
                live_obj_count: Cell::new(0),
                ctor_count: Cell::new(0),
                dtor_count: Cell::new(0),
                alloc_count: Cell::new(0),
                fail_threshold: Cell::new(usize::MAX),
            }
        }
    }

    #[derive(Clone)]
    struct TestAllocator<'a> {
        state: &'a TestState,
    }

    impl<'a> kalloc::Allocator for TestAllocator<'a> {
        fn allocate(&self, layout: core::alloc::Layout) -> Result<NonNull<[u8]>, AllocError> {
            let current = self.state.alloc_count.get();
            self.state.alloc_count.set(current + 1);
            if current >= self.state.fail_threshold.get() {
                return Err(AllocError);
            }
            DefaultAllocator::default().allocate(layout)
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: core::alloc::Layout) {
            unsafe { DefaultAllocator::default().deallocate(ptr, layout) }
        }

        unsafe fn grow(
            &self,
            ptr: NonNull<u8>,
            old_layout: core::alloc::Layout,
            new_layout: core::alloc::Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            let current = self.state.alloc_count.get();
            self.state.alloc_count.set(current + 1);
            if current >= self.state.fail_threshold.get() {
                return Err(AllocError);
            }
            unsafe { DefaultAllocator::default().grow(ptr, old_layout, new_layout) }
        }

        unsafe fn shrink(
            &self,
            ptr: NonNull<u8>,
            old_layout: core::alloc::Layout,
            new_layout: core::alloc::Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            let current = self.state.alloc_count.get();
            self.state.alloc_count.set(current + 1);
            if current >= self.state.fail_threshold.get() {
                return Err(AllocError);
            }
            unsafe { DefaultAllocator::default().shrink(ptr, old_layout, new_layout) }
        }

        fn allocate_zeroed(
            &self,
            layout: core::alloc::Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            let current = self.state.alloc_count.get();
            self.state.alloc_count.set(current + 1);
            if current >= self.state.fail_threshold.get() {
                return Err(AllocError);
            }
            DefaultAllocator::default().allocate_zeroed(layout)
        }
    }

    #[derive(Debug, Eq, PartialEq)]
    struct TestObject<'a> {
        val: usize,
        alive: bool,
        state: &'a TestState,
    }

    impl<'a> TestObject<'a> {
        fn new(val: usize, state: &'a TestState) -> Self {
            state.live_obj_count.set(state.live_obj_count.get() + 1);
            state.ctor_count.set(state.ctor_count.get() + 1);
            TestObject { val, alive: true, state }
        }
    }

    impl<'a> Drop for TestObject<'a> {
        fn drop(&mut self) {
            if self.alive {
                self.state.live_obj_count.set(self.state.live_obj_count.get() - 1);
                self.state.dtor_count.set(self.state.dtor_count.get() + 1);
            }
        }
    }

    #[test]
    fn test_empty_array() {
        let a: Array<u32> = Array::new();
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());
    }

    #[test]
    fn test_try_new() {
        let a = Array::<u32>::try_new(5).unwrap();
        assert_eq!(a.len(), 5);
        for i in 0..5 {
            assert_eq!(a[i], 0);
        }
    }

    #[test]
    fn test_deref() {
        let mut a = Array::<u32>::try_new(2).unwrap();
        a[0] = 10;
        a[1] = 20;

        let slice: &[u32] = &a;
        assert_eq!(slice, &[10, 20]);

        let slice_mut: &mut [u32] = &mut a;
        slice_mut[0] = 30;
        assert_eq!(a[0], 30);
    }

    #[test]
    fn test_drop_behavior() {
        let state = TestState::default();
        {
            let mut b = Box::<[TestObject<'_>], TestAllocator<'_>>::try_new_uninit_slice_in(
                2,
                TestAllocator { state: &state },
            )
            .unwrap();
            b[0].write(TestObject::new(1, &state));
            b[1].write(TestObject::new(2, &state));
            let _a = Array::from_box(unsafe { b.assume_init() });
            assert_eq!(state.live_obj_count.get(), 2);
        }
        assert_eq!(state.live_obj_count.get(), 0);
        assert_eq!(state.dtor_count.get(), 2);
    }

    #[test]
    fn test_allocation_failure() {
        let state = TestState::default();
        state.fail_threshold.set(0); // Fail immediately

        let res = Array::<u32, TestAllocator<'_>>::try_new_in(5, TestAllocator { state: &state });
        assert!(res.is_err());
    }

    #[test]
    fn test_try_new_zero_sized() {
        let state = TestState::default();
        let a = Array::<u32, TestAllocator<'_>>::try_new_in(0, TestAllocator { state: &state })
            .unwrap();
        assert_eq!(a.len(), 0);
        assert!(a.is_empty());
    }

    #[test]
    fn test_non_trivial_default() {
        #[derive(Debug, PartialEq, Eq)]
        struct MyInt {
            value: i32,
        }
        impl Default for MyInt {
            fn default() -> Self {
                Self { value: 42 }
            }
        }

        let a = Array::<MyInt>::try_new(5).unwrap();
        assert_eq!(a.len(), 5);
        for i in 0..5 {
            assert_eq!(a[i].value, 42);
        }
    }

    #[test]
    fn test_array_new_in() {
        let state = TestState::default();
        let alloc = TestAllocator { state: &state };
        let a = Array::<u32, TestAllocator<'_>>::new_in(alloc.clone());
        assert!(a.is_empty());
    }

    #[test]
    fn test_array_default() {
        let a_def: Array<u32> = Default::default();
        assert!(a_def.is_empty());
    }

    #[test]
    fn test_array_into_box() {
        let a_try = Array::<u32>::try_new(3).unwrap();
        let b = a_try.into_box();
        assert_eq!(b.len(), 3);
    }

    #[test]
    fn test_array_test_allocator_happy() {
        use kalloc::Allocator;
        let state = TestState::default();
        let alloc = TestAllocator { state: &state };
        let layout = core::alloc::Layout::new::<u32>();
        let ptr = alloc.allocate_zeroed(layout).unwrap();

        let ptr = unsafe {
            alloc.grow(ptr.cast(), layout, core::alloc::Layout::array::<u32>(2).unwrap()).unwrap()
        };

        let ptr = unsafe {
            alloc.shrink(ptr.cast(), core::alloc::Layout::array::<u32>(2).unwrap(), layout).unwrap()
        };

        unsafe {
            alloc.deallocate(ptr.cast(), layout);
        }
    }

    #[test]
    fn test_array_test_allocator_failure() {
        use kalloc::Allocator;
        let state = TestState::default();
        let alloc = TestAllocator { state: &state };
        let layout = core::alloc::Layout::new::<u32>();

        // Set fail threshold to fail immediately
        state.fail_threshold.set(0);

        assert!(alloc.allocate_zeroed(layout).is_err());

        let dummy_ptr = core::ptr::NonNull::<u8>::dangling();
        assert!(
            unsafe {
                alloc.grow(dummy_ptr.cast(), layout, core::alloc::Layout::array::<u32>(2).unwrap())
            }
            .is_err()
        );
        assert!(
            unsafe {
                alloc.shrink(
                    dummy_ptr.cast(),
                    core::alloc::Layout::array::<u32>(2).unwrap(),
                    layout,
                )
            }
            .is_err()
        );
    }
}
