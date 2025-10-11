// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_rcu::rcu_cell::RcuCell;
use fuchsia_rcu::rcu_read_scope::RcuReadScope;
use fuchsia_rcu::rcu_write_scope::RcuWriteScope;

/// An array-like data structure that can be read without locking.
///
/// `RcuArray` provides a way to share a growable array between multiple threads.
/// Writers create a new copy of the array when it needs to be grown, and readers
/// are guaranteed to see a consistent snapshot of the array without blocking
/// writers.
#[derive(Default, Debug)]
pub struct RcuArray<T: Send + Sync + 'static> {
    inner: RcuCell<Box<[T]>>,
}

impl<T: Send + Sync + 'static> RcuArray<T> {
    /// Returns a reference to the element at the given `index`, or `None` if the
    /// index is out of bounds.
    pub fn get<'a>(&self, scope: &'a RcuReadScope, index: usize) -> Option<&'a T> {
        let array = self.inner.as_ref(scope);
        array.get(index)
    }

    /// Returns a slice containing the entire array.
    pub fn as_slice<'a>(&self, scope: &'a RcuReadScope) -> &'a [T] {
        let array = self.inner.as_ref(scope);
        array.as_ref()
    }

    /// Ensures that the array has at least `requested_size` elements, filling with
    /// `value` if the array needs to be grown.
    ///
    /// If the array is already large enough, this function does nothing. Otherwise,
    /// the array is grown to at least `requested_size`. To avoid frequent reallocations,
    /// the array will at least double in size.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn ensure_at_least(&self, scope: &RcuWriteScope, requested_size: usize, value: T)
    where
        T: Clone,
    {
        let array = self.inner.read();
        if array.len() >= requested_size {
            return;
        }
        let new_size = std::cmp::max(requested_size, array.len() * 2);
        self.copy_update(scope, &array, new_size, value);
    }

    /// Updates the array to contain the given vector.
    pub fn update(&self, scope: &RcuWriteScope, new_array: Vec<T>) {
        self.inner.update(scope, new_array.into_boxed_slice());
    }

    fn copy_update(&self, scope: &RcuWriteScope, array: &[T], new_size: usize, value: T)
    where
        T: Clone,
    {
        let mut new_array = Vec::new();
        new_array.reserve_exact(new_size);
        for item in array.iter() {
            new_array.push(item.clone());
        }
        for _ in array.len()..new_size {
            new_array.push(value.clone());
        }
        self.inner.update(scope, new_array.into_boxed_slice());
    }
}

/// Creates an `RcuArray` from a `Vec<T>`.
impl<T: Send + Sync + 'static> From<Vec<T>> for RcuArray<T> {
    fn from(value: Vec<T>) -> Self {
        Self { inner: RcuCell::new(value.into_boxed_slice()) }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_rcu::rcu_read_scope::RcuReadScope;
    use fuchsia_rcu::rcu_write_scope::RcuWriteScope;

    #[test]
    fn test_rcu_array_get() {
        let array = RcuArray::from(vec![1, 2, 3]);
        let scope = RcuReadScope::new();
        assert_eq!(array.get(&scope, 0), Some(&1));
        assert_eq!(array.get(&scope, 1), Some(&2));
        assert_eq!(array.get(&scope, 2), Some(&3));
        assert_eq!(array.get(&scope, 3), None);
    }

    #[test]
    fn test_rcu_array_as_slice() {
        let array = RcuArray::from(vec![1, 2, 3]);
        let scope = RcuReadScope::new();
        assert_eq!(array.as_slice(&scope), &[1, 2, 3]);
    }

    #[test]
    fn test_rcu_array_ensure_at_least() {
        let array = RcuArray::from(vec![1, 2, 3]);
        let write_scope = RcuWriteScope::new();

        unsafe { array.ensure_at_least(&write_scope, 5, 0) };
        let read_scope = RcuReadScope::new();
        // Should at least double.
        assert_eq!(array.as_slice(&read_scope), &[1, 2, 3, 0, 0, 0]);

        unsafe { array.ensure_at_least(&write_scope, 2, 0) };
        let read_scope = RcuReadScope::new();
        // Should not shrink below current size.
        assert_eq!(array.as_slice(&read_scope), &[1, 2, 3, 0, 0, 0]);

        unsafe { array.ensure_at_least(&write_scope, 12, 5) };
        let read_scope = RcuReadScope::new();
        assert_eq!(array.as_slice(&read_scope), &[1, 2, 3, 0, 0, 0, 5, 5, 5, 5, 5, 5]);
    }

    #[test]
    fn test_rcu_array_from_vec() {
        let vec = vec![1, 2, 3];
        let array = RcuArray::from(vec.clone());
        let scope = RcuReadScope::new();
        assert_eq!(array.as_slice(&scope), vec.as_slice());
    }
}
