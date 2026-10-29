// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::rcu_raw_hash_map::*;
use fuchsia_rcu::{RcuReadScope, rcu_run_callbacks};

#[test]
fn test_rcu_hash_map_custom_hasher() {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::BuildHasherDefault;
    let hasher = BuildHasherDefault::<DefaultHasher>::default();
    let map = RcuRawHashMap::with_capacity_and_hasher(10, hasher);
    let scope = RcuReadScope::new();
    unsafe {
        map.insert(&scope, 1, 10);
    }
    assert_eq!(map.get(&scope, &1), Some(&10));
}

#[test]
fn test_rcu_hash_map_insert_and_get() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    unsafe {
        map.insert(&scope, 1, 10);
        map.insert(&scope, 2, 20);
    }

    assert_eq!(map.get(&scope, &1), Some(&10));
    assert_eq!(map.get(&scope, &2), Some(&20));
    assert_eq!(map.get(&scope, &3), None);

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_remove() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    unsafe {
        map.insert(&scope, 1, 10);
        map.insert(&scope, 2, 20);
    }

    assert_eq!(map.get(&scope, &1), Some(&10));

    unsafe {
        assert_eq!(map.remove(&1), Some(10));
    }

    assert_eq!(map.get(&scope, &1), None);
    assert_eq!(map.get(&scope, &2), Some(&20));

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_insert_update() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    unsafe {
        map.insert(&scope, 1, 10);
    }

    assert_eq!(map.get(&scope, &1), Some(&10));

    let result = unsafe { map.insert(&scope, 1, 100) };
    assert!(matches!(result, InsertionResult::Updated(10)));

    assert_eq!(map.get(&scope, &1), Some(&100));

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_cursor() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    unsafe {
        map.insert(&scope, 1, 10);
        map.insert(&scope, 2, 20);
        map.insert(&scope, 3, 30);
    }

    let mut cursor = map.cursor(&scope);

    assert_eq!(cursor.current(), Some((&1, &10)));
    cursor.advance();
    assert_eq!(cursor.current(), Some((&2, &20)));

    unsafe {
        cursor.remove();
    }

    assert_eq!(cursor.current(), Some((&3, &30)));
    assert_eq!(map.get(&scope, &2), None);

    cursor.advance();
    assert_eq!(cursor.current(), None);

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_grow_maintains_order() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    let num_elements = INITIAL_CAPACITY * 3;
    let mut expected_order = Vec::new();

    for i in 0..num_elements {
        unsafe {
            map.insert(&scope, i, i * 10);
        }
        expected_order.push((i, i * 10));
    }

    let mut cursor = map.cursor(&scope);
    let mut actual_order = Vec::new();

    while let Some((key, value)) = cursor.current() {
        actual_order.push((*key, *value));
        cursor.advance();
    }

    assert_eq!(actual_order, expected_order);

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_grow_overwrites_maintain_order() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    let num_elements = INITIAL_CAPACITY * 3;
    let mut expected_order = Vec::new();

    for i in 0..num_elements {
        unsafe {
            map.insert(&scope, i, i * 10);
        }
        expected_order.push((i, i * 10));
    }

    // Overwrite some existing entries and add new ones
    unsafe {
        map.insert(&scope, 5, 500);
        map.insert(&scope, INITIAL_CAPACITY * 3, (INITIAL_CAPACITY * 3) * 10); // New entry
    }
    expected_order.retain(|(k, _)| *k != 5);
    expected_order.push((5, 500));
    expected_order.push((INITIAL_CAPACITY * 3, (INITIAL_CAPACITY * 3) * 10));

    let mut cursor = map.cursor(&scope);
    let mut actual_order = Vec::new();

    while let Some((key, value)) = cursor.current() {
        actual_order.push((*key, *value));
        cursor.advance();
    }

    assert_eq!(actual_order, expected_order);

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_grow() {
    let map = RcuRawHashMap::default();
    let scope = RcuReadScope::new();
    for i in 0..(INITIAL_CAPACITY * 3) {
        unsafe {
            map.insert(&scope, i, i * 10);
        }
    }

    for i in 0..(INITIAL_CAPACITY * 3) {
        assert_eq!(map.get(&scope, &i), Some(&(i * 10)));
    }

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_capacity_zero() {
    let map = RcuRawHashMap::with_capacity(0);
    let scope = RcuReadScope::new();

    assert_eq!(map.get(&scope, &1), None);

    unsafe {
        map.insert(&scope, 1, 10);
    }
    assert_eq!(map.get(&scope, &1), Some(&10));

    unsafe {
        assert_eq!(map.remove(&1), Some(10));
    }
    assert_eq!(map.get(&scope, &1), None);

    std::mem::drop(scope);
    rcu_run_callbacks();
}

#[test]
fn test_rcu_hash_map_bucket_lengths() {
    #[derive(Default)]
    struct ConstantHasher;
    impl std::hash::Hasher for ConstantHasher {
        fn finish(&self) -> u64 {
            0
        }
        fn write(&mut self, _bytes: &[u8]) {}
    }

    let hasher = std::hash::BuildHasherDefault::<ConstantHasher>::default();
    let map = RcuRawHashMap::with_capacity_and_hasher(10, hasher);
    let scope = RcuReadScope::new();

    assert_eq!(map.bucket_lengths(&scope).len(), 5);
    assert_eq!(map.bucket_lengths(&scope).sum::<usize>(), 0);

    unsafe {
        map.insert(&scope, 1, 10);
        map.insert(&scope, 2, 20);
        map.insert(&scope, 3, 30);
    }

    let lengths: Vec<usize> = map.bucket_lengths(&scope).collect();
    assert_eq!(lengths.len(), 5);
    assert_eq!(lengths.iter().sum::<usize>(), 3);
    assert_eq!(lengths.iter().filter(|&&len| len > 0).count(), 1);
    assert_eq!(lengths.iter().copied().max(), Some(3));

    std::mem::drop(scope);
    rcu_run_callbacks();
}
