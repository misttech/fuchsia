// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(unsafe_op_in_unsafe_fn)]

use fuchsia_rcu::rcu_ptr::{RcuPtr, RcuPtrRef};
use fuchsia_rcu::rcu_read_scope::RcuReadScope;
use fuchsia_rcu::rcu_write_scope::RcuWriteScope;

/// `Link` is an intrusive structure in a doubly-linked list.
///
/// Links are address-sensitive and cannot be moved once inserted into a list.
struct Link {
    /// The next node in the list.
    ///
    /// This field can be used to traverse the list within an RcuReadScope.
    next: RcuPtr<Link>,

    /// The previous node in the list.
    ///
    /// This pointer cannot be used without external synchronization.
    prev: RcuPtr<Link>,
}

impl Default for Link {
    fn default() -> Self {
        Self { next: RcuPtr::null(), prev: RcuPtr::null() }
    }
}

/// Returns the container of a given field.
///
/// # Safety
///
/// The pointer must point to the given field in a valid instance of the container.
macro_rules! container_of {
    ($ptr:expr, $container:path, $field:ident) => {{ $ptr.sub_byte_offset::<$container>(memoffset::offset_of!($container, $field)) }};
}

/// Returns the field of a given container.
///
/// # Safety
///
/// The pointer must point to a valid instance of the container.
macro_rules! field_of {
    ($ptr:expr, $container:path, $field:ident, $field_type:ty) => {{ $ptr.add_byte_offset::<$field_type>(memoffset::offset_of!($container, $field)) }};
}

/// `Node` is a node in an `RcuList`.
///
/// `Node` is address-sensitive and cannot be moved once inserted into a list.
///
/// Eventually, we want to generalize `RcuList` to support intrusive lists in
/// which the `Link` is embedded within other objects. For now, we always
/// allocate a `Node` and store the data in it.
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
    fn deferred_dealloc(scope: &RcuWriteScope, node: RcuPtrRef<'_, Node<T>>)
    where
        T: Send + Sync + 'static,
    {
        unsafe { scope.drop_box(node.as_mut_ptr()) };
    }

    /// Returns a pointer to the Link embedded in a Node.
    fn to_link(node: RcuPtrRef<'_, Node<T>>) -> RcuPtrRef<'_, Link> {
        if node.is_null() {
            return RcuPtrRef::null();
        }
        unsafe { field_of!(node, Node<T>, link, Link) }
    }

    /// Returns a pointer to the Node containing the given Link.
    fn from_link(link: RcuPtrRef<'_, Link>) -> RcuPtrRef<'_, Node<T>> {
        if link.is_null() {
            return RcuPtrRef::null();
        }
        unsafe { container_of!(link, Node<T>, link) }
    }
}

/// An `RcuList` is a doubly-linked list that supports concurrent access via
/// read-copy-update (RCU) synchronization.
///
/// An `RcuList` can be safely read by multiple readers, even while a writer
/// is modifying the list. To read from the list, you will need to enter an
/// `RcuReadScope`.
///
/// To modify the list, you will need to enter a `RcuWriteScope` and use some
/// external synchronization, such as a `Mutex`, to exclude concurrent writers.
pub struct RcuList<T: Send + Sync + 'static> {
    /// The first element of the list, if any.
    ///
    /// This field can be used to traverse the list within an RcuReadScope.
    head: RcuPtr<Link>,

    /// The last element of the list, if any.
    ///
    /// This pointer cannot be used without external synchronization.
    tail: RcuPtr<Link>,

    _marker: std::marker::PhantomData<T>,
}

impl<T: Send + Sync + 'static> Default for RcuList<T> {
    fn default() -> Self {
        Self { head: RcuPtr::null(), tail: RcuPtr::null(), _marker: std::marker::PhantomData }
    }
}

impl<T: Send + Sync + 'static> RcuList<T> {
    /// Pushes a new element to the front of the list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn push_front(&self, data: T) {
        let scope = RcuReadScope::new();
        let node = Node::alloc(&scope, data);
        let link_ptr = Node::to_link(node);
        let link = link_ptr.as_ref().unwrap();
        let head_ptr = self.head.read(&scope);
        if let Some(head) = head_ptr.as_ref() {
            head.prev.assign_ptr(link_ptr);
            link.next.assign_ptr(head_ptr);
        } else {
            self.tail.assign_ptr(link_ptr);
        }
        self.head.assign_ptr(link_ptr);
    }

    /// Pushes a new element to the back of the list.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn push_back(&self, data: T) {
        let scope = RcuReadScope::new();
        let node = Node::alloc(&scope, data);
        let link_ptr = Node::to_link(node);
        let link = link_ptr.as_ref().unwrap();
        let tail_ptr = self.tail.read(&scope);
        if let Some(tail) = tail_ptr.as_ref() {
            link.prev.assign_ptr(tail_ptr);
            tail.next.assign_ptr(link_ptr);
        } else {
            self.head.assign_ptr(link_ptr);
        }
        self.tail.assign_ptr(link_ptr);
    }

    /// Removes all elements from the list.
    ///
    /// Concurrent readers may continue to see the old value of the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn clear(&self, scope: &RcuWriteScope) {
        let read_scope = RcuReadScope::new();
        let mut current = self.head.read(&read_scope);

        self.head.assign(std::ptr::null_mut());
        self.tail.assign(std::ptr::null_mut());

        while let Some(link) = current.as_ref() {
            let next = link.next.read(&read_scope);

            // Other readers may continue to see this entry in the list and use the `next` pointer,
            // but they should not read the `prev` pointer anymore.
            link.prev.poison();
            let node = Node::<T>::from_link(current);
            Node::deferred_dealloc(scope, node);
            current = next;
        }
    }

    /// Returns a cursor that can be used to traverse and modify the list.
    ///
    /// Concurrent readers may continue to see the old value of the list until the RCU state machine
    /// has made sufficient progress to ensure that no concurrent readers are holding read guards.
    pub fn cursor<'a>(&'a self, scope: &'a RcuReadScope) -> RcuListCursor<'a, T> {
        let current = self.head.read(scope);
        RcuListCursor { scope, list: self, current }
    }

    /// Returns an iterator over the elements in the list.
    pub fn iter<'a>(&self, scope: &'a RcuReadScope) -> impl Iterator<Item = &'a T> {
        let next = self.head.read(&scope);
        RcuListIter { scope, next, _marker: std::marker::PhantomData }
    }
}

impl<T: Send + Sync + 'static> Drop for RcuList<T> {
    fn drop(&mut self) {
        let scope = RcuWriteScope::default();
        // SAFETY: The list is being dropped, so there are no concurrent readers.
        unsafe { self.clear(&scope) };
    }
}

/// A cursor for traversing and modifying an `RcuList`.
///
/// See `RcuList::cursor` for more information.
pub struct RcuListCursor<'a, T: Send + Sync + 'static> {
    scope: &'a RcuReadScope,
    list: &'a RcuList<T>,
    current: RcuPtrRef<'a, Link>,
}

impl<'a, T: Send + Sync + 'static> RcuListCursor<'a, T> {
    /// Returns the element at the current cursor position.
    pub fn current(&self) -> Option<&T> {
        let node = Node::from_link(self.current);
        node.as_ref().map(|node| &node.data)
    }

    /// Advances the cursor to the next element in the list.
    pub fn advance(&mut self) {
        if let Some(link) = self.current.as_ref() {
            self.current = link.next.read(&self.scope);
        }
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
    pub unsafe fn remove(&mut self, scope: &RcuWriteScope)
    where
        T: Send + Sync + 'static,
    {
        let node_ptr = Node::<T>::from_link(self.current);
        if let Some(node) = node_ptr.as_ref() {
            let link = &node.link;
            let prev = link.prev.read(&self.scope);
            let next = link.next.read(&self.scope);

            self.current = next;

            if let Some(next) = next.as_ref() {
                next.prev.assign_ptr(prev);
            } else {
                self.list.tail.assign_ptr(prev);
            }
            if let Some(prev) = prev.as_ref() {
                prev.next.assign_ptr(next);
            } else {
                self.list.head.assign_ptr(next);
            }

            // Other readers may continue to see this entry in the list and use the `next` pointer,
            // but they should not read the `prev` pointer anymore.
            link.prev.poison();
            Node::deferred_dealloc(scope, node_ptr);
        }
    }
}

struct RcuListIter<'a, T: 'static> {
    scope: &'a RcuReadScope,
    next: RcuPtrRef<'a, Link>,
    _marker: std::marker::PhantomData<T>,
}

impl<'a, T: 'static> Iterator for RcuListIter<'a, T> {
    type Item = &'a T;

    fn next(&mut self) -> Option<Self::Item> {
        if let Some(node) = Node::from_link(self.next).as_ref() {
            self.next = node.link.next.read(&self.scope);
            Some(&node.data)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rcu_list_push_front() {
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

    #[test]
    fn test_rcu_list_push_back() {
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

    #[test]
    fn test_rcu_list_clear() {
        let list = RcuList::default();
        unsafe {
            list.push_back(1);
            list.push_back(2);
            list.push_back(3);
        }

        let write_scope = RcuWriteScope::default();
        unsafe { list.clear(&write_scope) };

        let scope = RcuReadScope::new();
        let mut iter = list.iter(&scope);
        assert_eq!(iter.next(), None);
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
        // The list is dropped here, so the contained objects should also be dropped.
        assert_eq!(drop_count.load(Ordering::SeqCst), 3);
    }

    #[test]
    fn test_rcu_list_iter() {
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

    #[test]
    fn test_rcu_list_remove() {
        let list = RcuList::default();
        unsafe {
            list.push_back(1);
            list.push_back(2);
            list.push_back(3);
        }

        let write_scope = RcuWriteScope::default();
        let scope = RcuReadScope::new();
        let mut cursor = list.cursor(&scope);
        cursor.advance(); // current is 2
        assert_eq!(cursor.current(), Some(&2));
        unsafe { cursor.remove(&write_scope) };

        let scope = RcuReadScope::new();
        let mut iter = list.iter(&scope);
        assert_eq!(iter.next(), Some(&1));
        assert_eq!(iter.next(), Some(&3));
        assert_eq!(iter.next(), None);

        // Test removing head
        let scope = RcuReadScope::new();
        let mut cursor = list.cursor(&scope);
        unsafe { cursor.remove(&write_scope) };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next(), Some(&3));
        assert_eq!(iter.next(), None);

        // Test removing tail
        let scope = RcuReadScope::new();
        let mut cursor = list.cursor(&scope);
        unsafe { cursor.remove(&write_scope) };

        let mut iter = list.iter(&scope);
        assert_eq!(iter.next(), None);
    }

    #[test]
    fn test_rcu_list_remove_all() {
        let list = RcuList::default();
        unsafe {
            list.push_back(1);
            list.push_back(2);
            list.push_back(3);
        }

        let write_scope = RcuWriteScope::default();
        let scope = RcuReadScope::new();
        let mut cursor = list.cursor(&scope);
        while cursor.current().is_some() {
            unsafe { cursor.remove(&write_scope) };
        }

        let scope = RcuReadScope::new();
        assert_eq!(list.iter(&scope).next(), None);
    }
}
