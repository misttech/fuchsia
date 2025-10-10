// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fuchsia_rcu::{RcuReadScope, RcuWriteScope};
use fuchsia_rcu_collections::rcu_raw_hash_map::RcuRawHashMap;
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
        let scope = RcuWriteScope::new();
        let _guard = self.mutex.lock();
        // SAFETY: We have exclusive access to the map because we have exclusive access to the mutex.
        unsafe { self.map.insert(&scope, key, value) }
    }

    pub fn remove(&self, key: &K) -> Option<V> {
        let scope = RcuWriteScope::new();
        let _guard = self.mutex.lock();
        // SAFETY: We have exclusive access to the map because we have exclusive access to the mutex.
        unsafe { self.map.remove(&scope, key) }
    }
}
