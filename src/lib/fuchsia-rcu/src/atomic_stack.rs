// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::iter::Iterator;
use std::ptr;
use std::sync::atomic::{AtomicPtr, Ordering};

/// A node in the atomic stack.
struct Node<T: Send + Sync> {
    /// The next node in the stack.
    next: AtomicPtr<Node<T>>,

    /// The data in the node.
    data: T,
}

/// A stack of items that is thread-safe.
///
/// This stack is a singly linked list that is thread-safe and lock-free.
pub(crate) struct AtomicStack<T: Send + Sync> {
    /// The top element of the stack.
    head: AtomicPtr<Node<T>>,
}

impl<T: Send + Sync> Default for AtomicStack<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Send + Sync> AtomicStack<T> {
    /// Create an empty stack.
    pub(crate) const fn new() -> Self {
        Self { head: AtomicPtr::new(ptr::null_mut()) }
    }

    /// Push an element onto the front of the stack.
    pub(crate) fn push_front(&self, data: T) {
        let node = Box::new(Node { next: AtomicPtr::new(ptr::null_mut()), data });
        let node_ptr = Box::into_raw(node);
        // SAFETY: The node pointer is valid for reads until we drop the node.
        let node = unsafe { &*node_ptr };
        loop {
            let head = self.head.load(Ordering::Acquire);
            node.next.store(head, Ordering::Release);
            if self
                .head
                .compare_exchange(head, node_ptr, Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                break;
            }
        }
    }

    /// Swap the head of the stack with a null pointer.
    ///
    /// This function empties the stack. The caller takes ownership of the returned nodes.
    fn take_head(&self) -> *mut Node<T> {
        loop {
            let head = self.head.load(Ordering::Acquire);
            if self
                .head
                .compare_exchange(head, ptr::null_mut(), Ordering::AcqRel, Ordering::Relaxed)
                .is_ok()
            {
                return head;
            }
        }
    }

    /// Takes the contents of the stack and returns them as an iterator.
    ///
    /// This function empties the stack.
    fn take(&self) -> AtomicListIterator<T> {
        let head = self.take_head();
        AtomicListIterator { head }
    }

    /// Takes the contents of the stack and returns them as a vector.
    ///
    /// This function empties the stack.
    pub(crate) fn drain(&self) -> Vec<T> {
        self.take().collect()
    }
}

impl<T: Send + Sync> Drop for AtomicStack<T> {
    fn drop(&mut self) {
        for item in self.take() {
            std::mem::drop(item);
        }
    }
}

struct AtomicListIterator<T: Send + Sync> {
    // This pointer is the owning reference to the node.
    head: *mut Node<T>,
}

impl<T: Send + Sync> Iterator for AtomicListIterator<T> {
    type Item = T;
    fn next(&mut self) -> Option<Self::Item> {
        if self.head == ptr::null_mut() {
            None
        } else {
            // SAFETY: The node pointer is valid for reads until we drop the node. It is the owning
            // reference to the node. `T` is also Send + Sync, so we can access the object from
            // whatever thread we are currently on.
            let node = unsafe { Box::from_raw(self.head) };
            self.head = node.next.load(Ordering::Acquire);
            Some(node.data)
        }
    }
}

impl<T: Send + Sync> Drop for AtomicListIterator<T> {
    fn drop(&mut self) {
        for item in self {
            std::mem::drop(item);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn test_atomic_list() {
        let list = AtomicStack::new();
        list.push_front(1);
        list.push_front(2);
        list.push_front(3);
        assert_eq!(list.drain(), vec![3, 2, 1]);
        assert_eq!(list.drain(), vec![]);
        list.push_front(4);
        assert_eq!(list.drain(), vec![4]);
        assert_eq!(list.drain(), vec![]);
    }

    #[derive(Debug)]
    struct LeakCounter {
        drop_counter: Arc<AtomicUsize>,
    }

    impl Drop for LeakCounter {
        fn drop(&mut self) {
            self.drop_counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    impl PartialEq for LeakCounter {
        fn eq(&self, other: &Self) -> bool {
            Arc::ptr_eq(&self.drop_counter, &other.drop_counter)
        }
    }

    impl Eq for LeakCounter {}

    #[test]
    fn test_drain_drops_items() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let list = AtomicStack::new();
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        list.drain();
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
        assert_eq!(list.drain(), vec![]);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_drop_drops_items() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let list = AtomicStack::new();
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        drop(list);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn test_iterator_drops_items() {
        let drop_counter = Arc::new(AtomicUsize::new(0));
        let list = AtomicStack::new();
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        list.push_front(LeakCounter { drop_counter: drop_counter.clone() });
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        let iter = list.take();
        assert_eq!(drop_counter.load(Ordering::Relaxed), 0);
        drop(iter);
        assert_eq!(drop_counter.load(Ordering::Relaxed), 2);
    }
}
