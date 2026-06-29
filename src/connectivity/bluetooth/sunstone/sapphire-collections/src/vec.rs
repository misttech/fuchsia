// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::ops::{Deref, DerefMut};

pub mod raw_vec;

use crate::storage::StorageFamily;
use crate::vec::raw_vec::RawVec;

/// A contiguous, growable vector collection backed by a `RawVecLike` container.
///
/// # Examples
///
/// ```
/// use sapphire_collections::vec::StackVec;
///
/// let mut vec = StackVec::<i32, 4>::new();
/// vec.try_push(10).unwrap();
/// vec.try_push(20).unwrap();
/// assert_eq!(vec.len(), 2);
/// assert_eq!(vec[0], 10);
/// assert_eq!(vec.pop(), Some(20));
/// ```
pub struct Vec<T, A: StorageFamily> {
    inner: RawVec<T, A>,
    len: usize,
}

impl<T, A: StorageFamily> Default for Vec<T, A>
where
    RawVec<T, A>: Default,
{
    fn default() -> Self {
        Self { inner: Default::default(), len: 0 }
    }
}

impl<T, A: StorageFamily> Vec<T, A> {
    /// Creates a new, empty vector.
    pub fn new() -> Self
    where
        Self: Default,
    {
        Self::default()
    }

    /// Creates a new, empty vector with the given allocator.
    pub fn new_in(allocator: A::Storage<T>) -> Self {
        Self { inner: RawVec::new_in(allocator), len: 0 }
    }

    /// Returns the total capacity of the underlying buffer.
    pub fn capacity(&self) -> usize {
        self.inner.capacity()
    }

    /// Attempts to push a value to the back of the vector.
    ///
    /// Returns `Err(value)` if the buffer is full and cannot be grown.
    pub fn try_push(&mut self, value: T) -> Result<(), T> {
        if self.len() == self.capacity() {
            if self.inner.grow().is_err() {
                return Err(value);
            }
        }
        debug_assert!(self.capacity() > self.len());
        // SAFETY: `len < capacity()`, so the index `len` is within bounds.
        unsafe {
            self.inner.buffer_mut().get_unchecked_mut(self.len).write(value);
        }
        self.len += 1;
        Ok(())
    }

    /// Removes and returns the last element of the vector, if any.
    pub fn pop(&mut self) -> Option<T> {
        if self.len() == 0 {
            None
        } else {
            self.len -= 1;
            // SAFETY: `len` was previously initialized before incrementing, so it is valid to read.
            let inner =
                unsafe { self.inner.buffer_mut().get_unchecked_mut(self.len).assume_init_read() };
            Some(inner)
        }
    }

    /// Removes and returns the element at position `index` within the vector,
    /// shifting all elements after it to the left.
    ///
    /// # Panics
    /// Panics if `index` is out of bounds.
    pub fn remove(&mut self, index: usize) -> T {
        let len = self.len();
        assert!(index < len, "Index out of bounds");
        // SAFETY: We asserted that `index < len`. Because `inner` is initialized up to `len`,
        // the index `index` represents a valid initialized element. Shifting elements to the left
        // with `ptr::copy` is safe as long as the segments are within [0..len], which they are.
        unsafe {
            let ptr = self.inner.buffer_mut().as_mut_ptr().add(index);
            let ret = ptr.read().assume_init();
            // Shift elements after index to the left
            core::ptr::copy(ptr.add(1), ptr, len - index - 1);
            self.len -= 1;
            ret
        }
    }

    /// Returns the number of elements currently in the vector.
    pub fn len(&self) -> usize {
        self.len
    }
}

impl<T, A: StorageFamily> Deref for Vec<T, A> {
    type Target = [T];

    fn deref(&self) -> &Self::Target {
        // SAFETY: All elements up to `len` are guaranteed initialized by `try_push`.
        unsafe { self.inner.buffer()[..self.len()].assume_init_ref() }
    }
}

impl<T, A: StorageFamily> DerefMut for Vec<T, A> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        let len = self.len();
        // SAFETY: All elements up to `len` are guaranteed initialized by `try_push`.
        unsafe { self.inner.buffer_mut()[..len].assume_init_mut() }
    }
}

impl<T, A: StorageFamily> Drop for Vec<T, A> {
    fn drop(&mut self) {
        for element in self.iter_mut() {
            // SAFETY: All of these elements are initialized, and won't be accessed after dropping
            // them
            unsafe {
                core::ptr::drop_in_place(element);
            }
        }
    }
}

use crate::storage::ArrayStorage;

/// A vector collection backed by a stack-allocated fixed-size raw array.
pub type StackVec<T, const SIZE: usize> = Vec<T, ArrayStorage<SIZE>>;

/// A vector collection backed by a standard growable heap-allocated raw buffer.
#[cfg(feature = "std")]
pub type StdVec<T> = Vec<T, crate::storage::Global>;

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn test_vec_basic() {
        let mut vec = StackVec::<i32, 4>::new();
        assert_eq!(vec.len(), 0);
        assert_eq!(vec.capacity(), 0);

        vec.try_push(10).unwrap();
        assert_eq!(vec.capacity(), 4);
        vec.try_push(20).unwrap();
        assert_eq!(vec.len(), 2);
        assert_eq!(vec[0], 10);
        assert_eq!(vec[1], 20);

        assert_eq!(vec.pop(), Some(20));
        assert_eq!(vec.len(), 1);
        assert_eq!(vec.pop(), Some(10));
        assert_eq!(vec.len(), 0);
        assert_eq!(vec.pop(), None);
    }

    #[test]
    fn test_vec_remove() {
        let mut vec = StackVec::<i32, 4>::new();
        vec.try_push(10).unwrap();
        vec.try_push(20).unwrap();
        vec.try_push(30).unwrap();

        // Remove middle
        assert_eq!(vec.remove(1), 20);
        assert_eq!(vec.len(), 2);
        assert_eq!(vec[0], 10);
        assert_eq!(vec[1], 30);

        // Remove front
        assert_eq!(vec.remove(0), 10);
        assert_eq!(vec.len(), 1);
        assert_eq!(vec[0], 30);

        // Remove back
        assert_eq!(vec.remove(0), 30);
        assert_eq!(vec.len(), 0);
    }

    #[test]
    #[should_panic(expected = "Index out of bounds")]
    fn test_vec_remove_out_of_bounds() {
        let mut vec = StackVec::<i32, 4>::new();
        vec.try_push(10).unwrap();
        vec.remove(1);
    }

    #[test]
    fn test_vec_drop() {
        let counter = Rc::new(Cell::new(0));
        #[derive(Debug)]
        struct DropItem(Rc<Cell<i32>>);
        impl Drop for DropItem {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        {
            let mut vec = StackVec::<DropItem, 4>::new();
            vec.try_push(DropItem(counter.clone())).unwrap();
            vec.try_push(DropItem(counter.clone())).unwrap();
            // pop one
            vec.pop();
            assert_eq!(counter.get(), 1); // popped one should be dropped
        }
        assert_eq!(counter.get(), 2); // remaining one should be dropped when vec goes out of scope
    }

    mod proptests {
        use super::*;
        use proptest::prelude::*;

        #[derive(Debug, Clone)]
        enum VecOp<T> {
            Push(T),
            Pop,
            Remove(usize),
        }

        proptest! {
            #[test]
            fn test_vec_differential(ops in prop::collection::vec(
                prop_oneof![
                    any::<i32>().prop_map(VecOp::<i32>::Push),
                    Just(VecOp::Pop),
                    any::<usize>().prop_map(VecOp::<i32>::Remove),
                ],
                0..100
            )) {
                let mut custom_vec = StdVec::<i32>::new();
                let mut std_vec = std::vec::Vec::<i32>::new();

                for op in ops {
                    match op {
                        VecOp::Push(val) => {
                            custom_vec.try_push(val).unwrap();
                            std_vec.push(val);
                        }
                        VecOp::Pop => {
                            assert_eq!(custom_vec.pop(), std_vec.pop());
                        }
                        VecOp::Remove(idx) => {
                            let len = std_vec.len();
                            if len > 0 {
                                let target_idx = idx % len;
                                assert_eq!(custom_vec.remove(target_idx), std_vec.remove(target_idx));
                            }
                        }
                    }
                    assert_eq!(custom_vec.len(), std_vec.len());
                    assert_eq!(&custom_vec[..], &std_vec[..]);
                }
            }
        }
        proptest! {
            #[test]
            fn test_vec_zst(ops in prop::collection::vec(
                prop_oneof![
                    Just(VecOp::<()>::Push(())),
                    Just(VecOp::<()>::Pop),
                    any::<usize>().prop_map(VecOp::<()>::Remove),
                ],
                0..100
            )) {
                let mut custom_vec = StdVec::<()>::new();
                let mut std_vec = std::vec::Vec::<()>::new();

                for op in ops {
                    match op {
                        VecOp::Push(()) => {
                            custom_vec.try_push(()).unwrap();
                            std_vec.push(());
                        }
                        VecOp::Pop => {
                            custom_vec.pop();
                            std_vec.pop();
                        }
                        VecOp::Remove(idx) => {
                            let len = std_vec.len();
                            if len > 0 {
                                let target_idx = idx % len;
                                custom_vec.remove(target_idx);
                                std_vec.remove(target_idx);
                            }
                        }
                    }
                    assert_eq!(custom_vec.len(), std_vec.len());
                    assert_eq!(&custom_vec[..], &std_vec[..]);
                }
            }
        }
    }
}
