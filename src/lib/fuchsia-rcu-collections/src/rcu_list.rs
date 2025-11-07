// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(unsafe_op_in_unsafe_fn)]

use fuchsia_rcu::rcu_ptr::{RcuPtr, RcuPtrRef};
use fuchsia_rcu::{RcuReadScope, rcu_drop};

use crate::rcu_intrusive_list::{
    Link, RcuIntrusiveList, RcuIntrusiveListCursor, RcuListAdapter, rcu_list_adapter,
};

/// `Node` is a node in an `RcuList`.
///
/// `Node` is address-sensitive and cannot be moved once inserted into a list.
///
/// Eventually, we want to generalize `RcuList` to support intrusive lists in
/// which the `Link` is embedded within other objects. For now, we always
/// allocate a `Node` and store the data in it.
#[derive(Debug)]
struct Node<T> {
    /// The data stored in the node.
    data: T,

    /// The link to the next node in the list.
    link: Link,
}

impl<T> Node<T> {
    /// Allocates a new node.
    ///
    /// The node must be deallocated using `deferred_dealloc`.
    fn alloc(scope: &RcuReadScope, data: T) -> RcuPtrRef<'_, Node<T>> {
        let ptr = Box::into_raw(Box::new(Node { link: Link::default(), data }));
        // SAFETY: All nodes must be deallocated using `deferred_dealloc`, which defers their
        // deallocation until all in-flight read operations have completed.
        unsafe { RcuPtrRef::new(scope, ptr) }
    }

    /// Deallocates a node once all in-flight read operations have completed.
    ///
    /// The node must have been allocated using `alloc`.
    fn deferred_dealloc(node: RcuPtrRef<'_, Node<T>>)
    where
        T: Send + Sync + 'static,
    {
        // SAFETY: The node was allocated using `alloc`.
        let value = unsafe { Box::from_raw(node.as_mut_ptr()) };
        rcu_drop(value);
    }
}

impl<T> RcuListAdapter<Node<T>> for Node<T> {
    rcu_list_adapter!(Node<T>, link);
}

/// An `RcuList` is a doubly-linked list that supports concurrent access via
/// read-copy-update (RCU) synchronization.
///
/// An `RcuList` can be safely read by multiple readers, even while a writer
/// is modifying the list. To read from the list, you will need to enter an
/// `RcuReadScope`.
///
/// To modify the list, you will need to use some external synchronization,
/// such as a `Mutex`, to exclude concurrent writers.
#[derive(Debug)]
pub struct RcuList<T: Send + Sync + 'static> {
    list: RcuIntrusiveList<Node<T>, Node<T>>,
}

impl<T: Send + Sync + 'static> Default for RcuList<T> {
    fn default() -> Self {
        Self { list: RcuIntrusiveList::default() }
    }
}

impl<T: Send + Sync + 'static> RcuList<T> {
    /// Creates a new list with the given head and tail.
    pub fn new(head: RcuPtr<Link>, tail: RcuPtr<Link>) -> Self {
        Self { list: RcuIntrusiveList::new(head, tail) }
    }

    /// Pushes a new element to the front of the list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn push_front(&self, data: T) {
        let scope = RcuReadScope::new();
        let node = Node::alloc(&scope, data);
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe {
            self.list.push_front(&scope, node);
        }
    }

    /// Pushes a new element to the back of the list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn push_back(&self, data: T) {
        let scope = RcuReadScope::new();
        let node = Node::alloc(&scope, data);
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe {
            self.list.push_back(&scope, node);
        }
    }

    /// Appends another list to the end of this list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn append(&self, other: Self) {
        let scope = RcuReadScope::new();
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe {
            let items = other.list.split_off(&scope, 0);
            self.list.append(&scope, items);
        }
    }

    /// Splits the list into two lists at the given position.
    ///
    /// If the given position is past the end of the list, returns an empty list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn split_off(&self, pos: usize) -> Self {
        let scope = RcuReadScope::new();
        // SAFETY: Our caller promises to exclude concurrent writers.
        Self { list: unsafe { self.list.split_off(&scope, pos) } }
    }

    /// Removes all elements from the list.
    ///
    /// Concurrent readers may continue to see the old value of the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn clear(&self) {
        let scope = RcuReadScope::new();
        // SAFETY: Our caller promises to exclude concurrent writers.
        unsafe { self.list.clear(&scope, Node::deferred_dealloc) };
    }

    #[cfg(test)]
    fn is_empty(&self) -> bool {
        let scope = RcuReadScope::new();
        self.list.is_empty(&scope)
    }

    /// Returns a cursor that can be used to traverse and modify the list.
    ///
    /// Concurrent readers may continue to see the old value of the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn cursor<'a>(&'a self, scope: &'a RcuReadScope) -> RcuListCursor<'a, T> {
        RcuListCursor { cursor: self.list.cursor(scope) }
    }

    /// Returns an iterator over the elements in the list.
    pub fn iter<'a>(&self, scope: &'a RcuReadScope) -> impl Iterator<Item = &'a T> {
        self.list.iter(scope).map(|node| &node.data)
    }
}

pub struct RcuListCursor<'a, T: Send + Sync + 'static> {
    cursor: RcuIntrusiveListCursor<'a, Node<T>, Node<T>>,
}

impl<'a, T: Send + Sync + 'static> RcuListCursor<'a, T> {
    /// Returns the element at the current cursor position.
    pub fn current(&self) -> Option<&T> {
        self.cursor.current().map(|node| &node.data)
    }

    /// Advances the cursor to the next element in the list.
    pub fn advance(&mut self) {
        self.cursor.advance();
    }

    /// Removes the element at the current cursor position.
    ///
    /// After calling `remove`, the cursor will be positioned at the next element in the list.
    ///
    /// Concurrent readers may continue to see this entry in the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn remove(&mut self) {
        let removed_node = unsafe { self.cursor.remove() };
        Node::deferred_dealloc(removed_node);
    }
}

impl<T: Send + Sync + 'static> Drop for RcuList<T> {
    fn drop(&mut self) {
        // SAFETY: The list is being dropped, so there are no concurrent readers.
        unsafe { self.clear() };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_rcu::rcu_synchronize;

    #[test]
    fn test_rcu_list_push_front() {
        {
            let list = RcuList::default();
            unsafe {
                list.push_front(1);
                list.push_front(2);
                list.push_front(3);
            }

            let scope = RcuReadScope::new();
            let mut cursor = list.cursor(&scope);
            assert_eq!(cursor.current(), Some(&3));
            cursor.advance();
            assert_eq!(cursor.current(), Some(&2));
            cursor.advance();
            assert_eq!(cursor.current(), Some(&1));
            cursor.advance();
            assert_eq!(cursor.current(), None);
        }
        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_push_back() {
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let scope = RcuReadScope::new();
            let mut cursor = list.cursor(&scope);
            assert_eq!(cursor.current(), Some(&1));
            cursor.advance();
            assert_eq!(cursor.current(), Some(&2));
            cursor.advance();
            assert_eq!(cursor.current(), Some(&3));
            cursor.advance();
            assert_eq!(cursor.current(), None);
        }
        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_clear() {
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            unsafe { list.clear() };

            let scope = RcuReadScope::new();
            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), None);
        }

        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_drop_clears_objects() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        struct DropCounter {
            _id: usize,
            counter: Arc<AtomicUsize>,
        }

        impl Drop for DropCounter {
            fn drop(&mut self) {
                self.counter.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drop_count = Arc::new(AtomicUsize::new(0));
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(DropCounter { _id: 1, counter: Arc::clone(&drop_count) });
                list.push_back(DropCounter { _id: 2, counter: Arc::clone(&drop_count) });
                list.push_back(DropCounter { _id: 3, counter: Arc::clone(&drop_count) });
            }
            assert_eq!(drop_count.load(Ordering::SeqCst), 0);
        }

        rcu_synchronize();

        // The list is dropped here, so the contained objects should also be dropped.
        assert_eq!(drop_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_rcu_list_iter() {
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let scope = RcuReadScope::new();
            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), Some(&3));
            assert_eq!(iter.next(), None);
        }

        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_remove() {
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let scope = RcuReadScope::new();
            let mut cursor = list.cursor(&scope);
            cursor.advance(); // current is 2
            assert_eq!(cursor.current(), Some(&2));
            unsafe { cursor.remove() };

            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&3));
            assert_eq!(iter.next(), None);

            // Test removing head
            let mut cursor = list.cursor(&scope);
            unsafe { cursor.remove() };

            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&3));
            assert_eq!(iter.next(), None);

            // Test removing tail
            let mut cursor = list.cursor(&scope);
            unsafe { cursor.remove() };

            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), None);
        }

        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_remove_all() {
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let scope = RcuReadScope::new();
            let mut cursor = list.cursor(&scope);
            while cursor.current().is_some() {
                unsafe { cursor.remove() };
            }

            assert_eq!(list.iter(&scope).next(), None);
        }

        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_append() {
        {
            let list1 = RcuList::default();
            unsafe {
                list1.push_back(1);
                list1.push_back(2);
            }

            let list2 = RcuList::default();
            unsafe {
                list2.push_back(3);
                list2.push_back(4);
            }

            unsafe { list1.append(list2) };

            let scope = RcuReadScope::new();
            let mut iter = list1.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), Some(&3));
            assert_eq!(iter.next(), Some(&4));
            assert_eq!(iter.next(), None);
        }

        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_append_empty() {
        // Append to an empty list.
        {
            let list1 = RcuList::default();
            let list2 = RcuList::default();
            unsafe {
                list2.push_back(1);
                list2.push_back(2);
            }
            unsafe { list1.append(list2) };

            let scope = RcuReadScope::new();
            let mut iter = list1.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), None);
        }
        rcu_synchronize();

        // Append an empty list.
        {
            let list1 = RcuList::default();
            unsafe {
                list1.push_back(1);
                list1.push_back(2);
            }
            let list2 = RcuList::default();
            unsafe { list1.append(list2) };

            let scope = RcuReadScope::new();
            let mut iter = list1.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), None);
        }
        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_is_empty() {
        {
            let list = RcuList::default();
            assert!(list.is_empty());

            unsafe {
                list.push_back(1);
            }
            assert!(!list.is_empty());

            unsafe {
                list.clear();
            }
            assert!(list.is_empty());
        }

        rcu_synchronize();
    }

    #[test]
    fn test_rcu_list_split_off() {
        // Split at the beginning.
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let new_list = unsafe { list.split_off(0) };

            let scope = RcuReadScope::new();
            assert!(list.is_empty());
            let mut new_iter = new_list.iter(&scope);
            assert_eq!(new_iter.next(), Some(&1));
            assert_eq!(new_iter.next(), Some(&2));
            assert_eq!(new_iter.next(), Some(&3));
            assert_eq!(new_iter.next(), None);
        }
        rcu_synchronize();

        // Split in the middle.
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
                list.push_back(4);
            }

            let new_list = unsafe { list.split_off(2) };

            let scope = RcuReadScope::new();
            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), None);

            let mut new_iter = new_list.iter(&scope);
            assert_eq!(new_iter.next(), Some(&3));
            assert_eq!(new_iter.next(), Some(&4));
            assert_eq!(new_iter.next(), None);
        }
        rcu_synchronize();

        // Split at the last element.
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let new_list = unsafe { list.split_off(2) };

            let scope = RcuReadScope::new();
            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), None);

            let mut new_iter = new_list.iter(&scope);
            assert_eq!(new_iter.next(), Some(&3));
            assert_eq!(new_iter.next(), None);
        }
        rcu_synchronize();

        // Split one past the last element.
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let new_list = unsafe { list.split_off(3) };

            let scope = RcuReadScope::new();
            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), Some(&3));
            assert_eq!(iter.next(), None);

            assert!(new_list.is_empty());
        }
        rcu_synchronize();

        // Split far past the end of the list.
        {
            let list = RcuList::default();
            unsafe {
                list.push_back(1);
                list.push_back(2);
                list.push_back(3);
            }

            let new_list = unsafe { list.split_off(10) };

            let scope = RcuReadScope::new();
            let mut iter = list.iter(&scope);
            assert_eq!(iter.next(), Some(&1));
            assert_eq!(iter.next(), Some(&2));
            assert_eq!(iter.next(), Some(&3));
            assert_eq!(iter.next(), None);

            assert!(new_list.is_empty());
        }
        rcu_synchronize();
    }
}
