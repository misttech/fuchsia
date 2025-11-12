// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_rcu::RcuReadScope;
use fuchsia_rcu_collections::rcu_raw_hash_map::{InsertionResult, RcuRawHashMap};
use starnix_sync::Mutex;
use std::hash::Hash;

#[derive(Debug)]
pub struct RcuHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    map: RcuRawHashMap<K, V>,
    mutex: Mutex<()>,
}

impl<K, V> Default for RcuHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    fn default() -> Self {
        Self { map: Default::default(), mutex: Mutex::new(()) }
    }
}

impl<K, V> RcuHashMap<K, V>
where
    K: Eq + Hash + Clone + Send + Sync + 'static,
    V: Clone + Send + Sync + 'static,
{
    pub fn get<'a>(&self, scope: &'a RcuReadScope, key: &K) -> Option<&'a V> {
        self.map.get(scope, key)
    }

    pub fn insert(&self, key: K, value: V) -> Option<V> {
        let _guard = self.mutex.lock();
        let scope = RcuReadScope::new();
        // SAFETY: We have exclusive access to the map because we have exclusive access to the mutex.
        match unsafe { self.map.insert(&scope, key, value) } {
            InsertionResult::Inserted(_) => None,
            InsertionResult::Updated(old_value) => Some(old_value),
        }
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let _guard = self.mutex.lock();
        // SAFETY: We have exclusive access to the map because we have exclusive access to the mutex.
        unsafe { self.map.remove(key) }
    }
}
