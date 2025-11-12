// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_rcu::RcuReadScope;
use fuchsia_rcu_collections::rcu_raw_hash_map::{InsertionResult, RcuRawHashMap};
use starnix_sync::Mutex;
use std::hash::Hash;

/// A cache that uses RCU to provide thread-safe access to a hash map.
///
/// This is similar to `RcuHashMap`, but it also evicts items when the cache
/// exceeds a specified capacity.
///
/// Entries are evicted in a FIFO manner.
#[derive(Debug)]
pub struct RcuCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// The maximum number of entries in the cache.
    capacity: usize,

    /// The underlying hash map.
    map: RcuRawHashMap<K, V>,

    /// A mutex to provide synchronization for writing to the map.
    mutex: Mutex<()>,
}

impl<K, V> RcuCache<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Creates a new `RcuCache` with the specified capacity.
    pub fn new(capacity: usize) -> Self {
        Self { capacity, map: Default::default(), mutex: Mutex::new(()) }
    }

    /// Returns a reference to the value associated with the key.
    pub fn get<'a>(&self, scope: &'a RcuReadScope, key: &K) -> Option<&'a V> {
        self.map.get(scope, key)
    }

    /// Inserts a key-value pair into the cache.
    ///
    /// If the cache exceeds its capacity, entries are evicted in a FIFO manner.
    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let _guard = self.mutex.lock();
        let scope = RcuReadScope::new();
        // SAFETY: We have exclusive access to the map because we have exclusive access to the mutex.
        match unsafe { self.map.insert(&scope, key, value) } {
            InsertionResult::Inserted(count) => {
                if count > self.capacity {
                    // The mutex should prevent any other modifications to the map while the insert
                    // operation is in progress.
                    assert!(count == self.capacity + 1);
                    let mut cursor = self.map.cursor(&scope);
                    // SAFETY: We have exclusive access to the map because we have exclusive access
                    // to the mutex.
                    unsafe { cursor.remove() };
                }
                None
            }
            InsertionResult::Updated(old_value) => Some(old_value),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fuchsia_rcu::rcu_synchronize;

    #[test]
    fn test_rcu_cache_fifo_eviction() {
        let capacity = 3;
        let cache = RcuCache::new(capacity);
        let scope = RcuReadScope::new();

        // Insert items up to capacity
        cache.insert(1, 10);
        cache.insert(2, 20);
        cache.insert(3, 30);

        assert_eq!(cache.get(&scope, &1), Some(&10));
        assert_eq!(cache.get(&scope, &2), Some(&20));
        assert_eq!(cache.get(&scope, &3), Some(&30));

        // Insert an item beyond capacity, should evict 1
        cache.insert(4, 40);

        assert_eq!(cache.get(&scope, &1), None);
        assert_eq!(cache.get(&scope, &2), Some(&20));
        assert_eq!(cache.get(&scope, &3), Some(&30));
        assert_eq!(cache.get(&scope, &4), Some(&40));

        // Insert another item, should evict 2
        cache.insert(5, 50);

        assert_eq!(cache.get(&scope, &1), None);
        assert_eq!(cache.get(&scope, &2), None);
        assert_eq!(cache.get(&scope, &3), Some(&30));
        assert_eq!(cache.get(&scope, &4), Some(&40));
        assert_eq!(cache.get(&scope, &5), Some(&50));

        // Update an existing item, should not evict and not change order for eviction
        cache.insert(3, 300);

        assert_eq!(cache.get(&scope, &1), None);
        assert_eq!(cache.get(&scope, &2), None);
        assert_eq!(cache.get(&scope, &3), Some(&300));
        assert_eq!(cache.get(&scope, &4), Some(&40));
        assert_eq!(cache.get(&scope, &5), Some(&50));

        // Insert another item, should evict 4 (because 3 was updated, not re-inserted)
        cache.insert(6, 60);

        assert_eq!(cache.get(&scope, &1), None);
        assert_eq!(cache.get(&scope, &2), None);
        assert_eq!(cache.get(&scope, &3), Some(&300));
        assert_eq!(cache.get(&scope, &4), None);
        assert_eq!(cache.get(&scope, &5), Some(&50));
        assert_eq!(cache.get(&scope, &6), Some(&60));

        std::mem::drop(scope);
        rcu_synchronize();
    }
}
