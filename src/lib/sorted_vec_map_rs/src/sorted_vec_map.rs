// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::borrow::Borrow;
use std::cmp::Ordering;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::{iter, slice};

/// An ordered map built on a `Vec`.
///
/// This map is optimized for reducing the memory usage of data that rarely or never changes.
/// Insertions and removals take linear time while lookups take logarithmic time.
#[derive(Eq, PartialEq, PartialOrd, Ord, Hash, Clone, Default)]
pub struct SortedVecMap<K, V> {
    vec: Vec<(K, V)>,
}

impl<K, V> SortedVecMap<K, V> {
    /// Constructs a new, empty `SortedVecMap`.
    pub fn new() -> Self {
        Self { vec: Vec::new() }
    }

    /// Returns true if there are no entries in the map.
    pub fn is_empty(&self) -> bool {
        self.vec.is_empty()
    }

    /// Returns true if the map contains an entry for the given key.
    pub fn contains_key<Q>(&self, key: &Q) -> bool
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.index_of(key).is_ok()
    }

    /// Returns a reference to the value corresponding to the key.
    pub fn get<Q>(&self, key: &Q) -> Option<&V>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        if let Ok(index) = self.index_of(key) { Some(&self.vec[index].1) } else { None }
    }

    /// Returns a mutable reference to the value corresponding to the key.
    pub fn get_mut<Q>(&mut self, key: &Q) -> Option<&mut V>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        if let Ok(index) = self.index_of(key) { Some(&mut self.vec[index].1) } else { None }
    }

    /// Inserts a key-value pair into the map. If the map did not have this key present, `None` is
    /// returned. If the map did have this key present, the value is updated, and the old value is
    /// returned. The key is not updated.
    pub fn insert(&mut self, key: K, value: V) -> Option<V>
    where
        K: Ord,
    {
        match self.index_of(&key) {
            Ok(index) => {
                let old = std::mem::replace(&mut self.vec[index], (key, value));
                Some(old.1)
            }
            Err(index) => {
                self.vec.insert(index, (key, value));
                None
            }
        }
    }

    /// Returns an iterator over the entries of the map, sorted by key.
    pub fn iter(&self) -> Iter<'_, K, V> {
        self.vec.iter().map(|entry| (&entry.0, &entry.1))
    }

    /// Returns an iterator over the entries of the map, sorted by key.
    pub fn iter_mut(&mut self) -> IterMut<'_, K, V> {
        self.vec.iter_mut().map(|entry| (&entry.0, &mut entry.1))
    }

    /// Returns an iterator over the keys of the map, in sorted order.
    pub fn keys(&self) -> Keys<'_, K, V> {
        self.vec.iter().map(|e| &e.0)
    }

    /// Returns a mutable iterator over the values of the map, sorted by key.
    pub fn values_mut(&mut self) -> ValuesMut<'_, K, V> {
        self.vec.iter_mut().map(|e| &mut e.1)
    }

    /// Searches for the key in the map.
    ///
    /// If the key is found then `Result::Ok` is returned with the index of the entry. If the key is
    /// not found then `Result::Err` is return, containing the index where an entry with that key
    /// could be inserted while maintaining sorted order.
    fn index_of<Q>(&self, key: &Q) -> Result<usize, usize>
    where
        K: Borrow<Q> + Ord,
        Q: Ord + ?Sized,
    {
        self.vec.binary_search_by(|probe| probe.0.borrow().cmp(key))
    }
}

impl<K: Debug, V: Debug> Debug for SortedVecMap<K, V> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map().entries(self.iter()).finish()
    }
}

pub type Iter<'a, K, V> = iter::Map<slice::Iter<'a, (K, V)>, fn(&(K, V)) -> (&K, &V)>;
pub type IterMut<'a, K, V> = iter::Map<slice::IterMut<'a, (K, V)>, fn(&mut (K, V)) -> (&K, &mut V)>;
pub type Keys<'a, K, V> = iter::Map<slice::Iter<'a, (K, V)>, fn(&(K, V)) -> &K>;
pub type ValuesMut<'a, K, V> = iter::Map<slice::IterMut<'a, (K, V)>, fn(&mut (K, V)) -> &mut V>;

impl<K: Ord, V> FromIterator<(K, V)> for SortedVecMap<K, V> {
    fn from_iter<T: IntoIterator<Item = (K, V)>>(iter: T) -> Self {
        let mut vec = Vec::from_iter(iter);
        vec.shrink_to_fit();
        vec.sort_by(sort_comparator);
        vec.dedup_by(dedup_comparator);
        Self { vec }
    }
}

impl<'de, K, V> serde::Deserialize<'de> for SortedVecMap<K, V>
where
    K: Ord + serde::Deserialize<'de>,
    V: serde::Deserialize<'de>,
{
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct Visitor<K, V> {
            _map_type: PhantomData<SortedVecMap<K, V>>,
        }
        impl<'de, K, V> serde::de::Visitor<'de> for Visitor<K, V>
        where
            K: Ord + serde::Deserialize<'de>,
            V: serde::Deserialize<'de>,
        {
            type Value = SortedVecMap<K, V>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a map")
            }
            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut vec: Vec<(K, V)> = match map.size_hint() {
                    Some(hint) => Vec::with_capacity(hint),
                    None => Vec::new(),
                };
                let mut is_sorted_and_deduped = true;
                while let Some(entry) = map.next_entry()? {
                    if is_sorted_and_deduped {
                        if let Some(back) = vec.last() {
                            is_sorted_and_deduped = back.0.cmp(&entry.0) == Ordering::Less;
                        }
                    }
                    vec.push(entry);
                }
                if !is_sorted_and_deduped {
                    vec.sort_by(sort_comparator);
                    vec.dedup_by(dedup_comparator);
                }
                vec.shrink_to_fit();
                Ok(SortedVecMap { vec })
            }
        }
        deserializer.deserialize_map(Visitor { _map_type: PhantomData })
    }
}

impl<K: serde::Serialize, V: serde::Serialize> serde::Serialize for SortedVecMap<K, V> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.collect_map(self.iter())
    }
}

fn sort_comparator<K: Ord, V>(a: &(K, V), b: &(K, V)) -> Ordering {
    a.0.cmp(&b.0)
}

fn dedup_comparator<K: Ord, V>(a: &mut (K, V), b: &mut (K, V)) -> bool {
    a.0 == b.0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use test_case::test_case;

    #[test_case(vec![], 1 => None; "empty_map")]
    #[test_case(vec![(1, 21)], 1 => Some(21); "contains_key")]
    #[test_case(vec![(1, 21), (0, 20)], 0 => Some(20); "contains_key_unsorted_init")]
    #[test_case(vec![(1, 21), (0, 20), (2, 22)], 2 => Some(22); "contains_key_middle")]
    #[test_case(vec![(1, 21), (0, 20), (2, 22)], 3 => None; "missing_key")]
    fn test_get(initial: Vec<(i32, i32)>, lookup: i32) -> Option<i32> {
        let map: SortedVecMap<i32, i32> = initial.into_iter().collect();
        map.get(&lookup).copied()
    }

    #[test_case(vec![], (50, 50), None, vec![(50, 50)]; "insert_empty")]
    #[test_case(vec![(50, 50)], (47, 47), None, vec![(47, 47), (50, 50)]; "insert_lesser")]
    #[test_case(vec![(47, 47), (50, 50)], (48, 48), None, vec![(47, 47), (48, 48), (50, 50)]; "insert_middle")]
    #[test_case(vec![(47, 47), (50, 50)], (51, 51), None, vec![(47, 47), (50, 50), (51, 51)]; "insert_greater")]
    #[test_case(vec![(47, 47), (48, 48), (50, 50)], (48, 88), Some(48), vec![(47, 47), (48, 88), (50, 50)]; "insert_overwrite")]
    fn test_insert(
        initial: Vec<(i32, i32)>,
        to_insert: (i32, i32),
        expected_return: Option<i32>,
        expected_vec: Vec<(i32, i32)>,
    ) {
        let mut map: SortedVecMap<i32, i32> = initial.into_iter().collect();
        let ret = map.insert(to_insert.0, to_insert.1);
        assert_eq!(ret, expected_return);
        assert_eq!(map.vec, expected_vec);
    }

    #[test_case(vec![(56, 56), (47, 47), (53, 53), (51, 51), (49, 49)]; "normal_map")]
    #[test_case(vec![]; "empty_map")]
    fn test_serialize_deserialize(input: Vec<(i32, i32)>) {
        let map: SortedVecMap<i32, i32> = input.into_iter().collect();
        let serialized = bincode::serialize(&map).unwrap();
        let deserialized: SortedVecMap<i32, i32> = bincode::deserialize(&serialized).unwrap();
        assert_eq!(map, deserialized);
    }

    #[test_case(vec![(56, 56), (47, 47), (53, 53), (51, 51), (49, 49)]; "normal_map")]
    #[test_case(vec![]; "empty_map")]
    fn test_deserialize_from_btree_map(input: Vec<(i32, i32)>) {
        let map: BTreeMap<i32, i32> = input.into_iter().collect();
        let serialized = bincode::serialize(&map).unwrap();
        let deserialized: SortedVecMap<i32, i32> = bincode::deserialize(&serialized).unwrap();

        let map_entries: Vec<(i32, i32)> = map.into_iter().collect();
        assert_eq!(map_entries, deserialized.vec);
    }
}
