// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![warn(unsafe_op_in_unsafe_fn)]

use crate::rcu_array::RcuArray;
use crate::rcu_list::RcuList;
use fuchsia_rcu::{RcuReadScope, RcuWriteScope};
use std::hash::{Hash, Hasher};
use std::sync::atomic::{AtomicUsize, Ordering};

/// The initial size of the hash map.
const INITIAL_SIZE: usize = 64;

/// A hash map that uses read-copy-update (RCU) to manage concurrent accesses.
#[derive(Debug)]
pub struct RcuRawHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// The table of buckets.
    table: RcuArray<RcuList<(K, V)>>,

    /// The number of entries in the map.
    num_entries: AtomicUsize,
}

impl<K, V> Default for RcuRawHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        let mut table = Vec::new();
        table.resize_with(INITIAL_SIZE, Default::default);
        Self { table: RcuArray::from(table), num_entries: AtomicUsize::new(0) }
    }
}

impl<K, V> RcuRawHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    /// Returns the hash of the key as a u64.
    fn hash_key(key: &K) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        key.hash(&mut hasher);
        hasher.finish()
    }

    /// Returns the bucket for the given key in the given table.
    fn get_bucket<'a>(table: &'a [RcuList<(K, V)>], key: &K) -> &'a RcuList<(K, V)> {
        let hash = Self::hash_key(key);
        let index = hash as usize % table.len();
        &table[index]
    }

    /// Returns a reference to the bucket for the given key.
    fn read_bucket<'a>(&self, scope: &'a RcuReadScope, key: &K) -> &'a RcuList<(K, V)> {
        let table = self.table.as_slice(scope);
        Self::get_bucket(table, key)
    }

    /// Returns a reference to the value corresponding to the key.
    ///
    /// Another thread running concurrently might see a different value for the object.
    pub fn get<'a>(&self, scope: &'a RcuReadScope, key: &K) -> Option<&'a V> {
        let bucket = self.read_bucket(scope, key);
        bucket.iter(scope).find(|(k, _)| k == key).map(|(_, v)| v)
    }

    /// Inserts a key-value pair into the map.
    ///
    /// If the map did not have this key present, `None` is returned.
    ///
    /// If the map did have this key present, the value is updated, and the old
    /// value is returned.
    ///
    /// Concurrent readers might not see the inserted value until the RCU state machine has made
    /// sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn insert(&self, scope: &RcuWriteScope, key: K, value: V) -> Option<V> {
        let read_scope = RcuReadScope::new();
        let mut table = self.table.as_slice(&read_scope);
        if self.needs_to_grow(table) {
            // SAFETY: Our caller is required to use external synchronization to exclude concurrent
            // writers.
            table = unsafe { self.grow(scope, &read_scope, table) };
        }
        let bucket = Self::get_bucket(table, &key);
        let mut cursor = bucket.cursor(&read_scope);
        while let Some((k, v)) = cursor.current() {
            if k == &key {
                let old_value = v.clone();
                // SAFETY: We have exclusive access to the bucket because we have exclusive access
                // to the table.
                unsafe {
                    cursor.remove(scope);
                    bucket.push_front((key, value));
                };
                return Some(old_value);
            }
            cursor.advance();
        }

        // SAFETY: We have exclusive access to the bucket because we have exclusive access to the
        // table.
        unsafe { bucket.push_front((key, value)) };
        self.num_entries.fetch_add(1, Ordering::Relaxed);
        None
    }

    /// Removes a key from the map, returning the value at the key if the key
    /// was previously in the map.
    ///
    /// Concurrent readers might see the removed value until the RCU state machine has made
    /// sufficient progress to ensure that no concurrent readers are holding read guards.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    pub unsafe fn remove(&self, scope: &RcuWriteScope, key: &K) -> Option<V> {
        let read_scope = RcuReadScope::new();
        let bucket = self.read_bucket(&read_scope, key);
        let mut cursor = bucket.cursor(&read_scope);
        while let Some((k, v)) = cursor.current() {
            if k == key {
                let old_value = v.clone();
                // SAFETY: We have exclusive access to the bucket because we have exclusive access
                // to the table.
                unsafe { cursor.remove(scope) };
                self.num_entries.fetch_sub(1, Ordering::Relaxed);
                return Some(old_value);
            }
            cursor.advance();
        }
        None
    }

    /// Whether the given table needs to grow to reduce the number of collisions.
    fn needs_to_grow(&self, table: &[RcuList<(K, V)>]) -> bool {
        self.num_entries.load(Ordering::Relaxed) > table.len() * 2
    }

    /// Grows the table to reduce the number of collisions.
    ///
    /// Returns a reference to the new table. Callers should be sure to update the table reference
    /// they are using to the returned value.
    ///
    /// # Safety
    ///
    /// Requires external synchronization to exclude concurrent writers.
    #[must_use]
    unsafe fn grow<'a>(
        &self,
        write_scope: &RcuWriteScope,
        read_scope: &'a RcuReadScope,
        old_table: &[RcuList<(K, V)>],
    ) -> &'a [RcuList<(K, V)>] {
        let new_size = old_table.len() * 2;
        let mut new_table = Vec::new();
        new_table.resize_with(new_size, Default::default);

        for bucket in old_table {
            for (k, v) in bucket.iter(read_scope) {
                let bucket = Self::get_bucket(&new_table, k);
                // SAFETY: We have exclusive access to new_table_vec because we just created it.
                unsafe { bucket.push_front((k.clone(), v.clone())) };
            }
        }

        self.table.update(write_scope, new_table);
        self.table.as_slice(read_scope)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_rcu_hash_map_insert_and_get() {
        let map = RcuRawHashMap::default();
        let write_scope = RcuWriteScope::new();
        unsafe {
            map.insert(&write_scope, 1, 10);
            map.insert(&write_scope, 2, 20);
        }

        let read_scope = RcuReadScope::new();
        assert_eq!(map.get(&read_scope, &1), Some(&10));
        assert_eq!(map.get(&read_scope, &2), Some(&20));
        assert_eq!(map.get(&read_scope, &3), None);
    }

    #[test]
    fn test_rcu_hash_map_remove() {
        let map = RcuRawHashMap::default();
        let write_scope = RcuWriteScope::new();
        unsafe {
            map.insert(&write_scope, 1, 10);
            map.insert(&write_scope, 2, 20);
        }

        let read_scope = RcuReadScope::new();
        assert_eq!(map.get(&read_scope, &1), Some(&10));

        unsafe {
            assert_eq!(map.remove(&write_scope, &1), Some(10));
        }

        let read_scope = RcuReadScope::new();
        assert_eq!(map.get(&read_scope, &1), None);
        assert_eq!(map.get(&read_scope, &2), Some(&20));
    }

    #[test]
    fn test_rcu_hash_map_insert_update() {
        let map = RcuRawHashMap::default();
        let write_scope = RcuWriteScope::new();
        unsafe {
            map.insert(&write_scope, 1, 10);
        }

        let read_scope = RcuReadScope::new();
        assert_eq!(map.get(&read_scope, &1), Some(&10));

        unsafe {
            assert_eq!(map.insert(&write_scope, 1, 100), Some(10));
        }

        let read_scope = RcuReadScope::new();
        assert_eq!(map.get(&read_scope, &1), Some(&100));
    }

    #[test]
    fn test_rcu_hash_map_grow() {
        let map = RcuRawHashMap::default();
        let write_scope = RcuWriteScope::new();
        for i in 0..(INITIAL_SIZE * 3) {
            unsafe {
                map.insert(&write_scope, i, i * 10);
            }
        }

        let read_scope = RcuReadScope::new();
        for i in 0..(INITIAL_SIZE * 3) {
            assert_eq!(map.get(&read_scope, &i), Some(&(i * 10)));
        }
    }
}
