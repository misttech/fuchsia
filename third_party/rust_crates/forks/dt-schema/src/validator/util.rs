// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use std::{collections::HashMap, hash::Hash};

pub trait Mergeable {
    /// We use this to merge hashmaps with vector values.
    /// e.g.
    /// {"a": [1]}.merge({"a": [2]}) => {"a": [1, 2]}
    fn merge(&mut self, other: Self);
}

impl<K: Hash + Eq, V> Mergeable for HashMap<K, Vec<V>> {
    fn merge(&mut self, other: Self) {
        for (k, v) in other {
            match self.get_mut(&k) {
                Some(value) => value.extend(v),
                None => {
                    self.insert(k, v);
                }
            }
        }
    }
}

pub trait ContainsExt<K> {
    fn contains_any(&self, keys: &[K]) -> bool;
    fn contains_all(&self, keys: &[K]) -> bool;
}

impl<V> ContainsExt<&str> for HashMap<String, V> {
    fn contains_any(&self, keys: &[&str]) -> bool {
        keys.iter().any(|&k| self.contains_key(k))
    }

    fn contains_all(&self, keys: &[&str]) -> bool {
        keys.iter().all(|&k| self.contains_key(k))
    }
}

impl ContainsExt<&str> for serde_json::Map<String, serde_json::Value> {
    fn contains_any(&self, keys: &[&str]) -> bool {
        keys.iter().any(|&k| self.contains_key(k))
    }

    fn contains_all(&self, keys: &[&str]) -> bool {
        keys.iter().all(|&k| self.contains_key(k))
    }
}

pub trait IsSchema {
    fn is_string_schema(&self) -> bool;
    fn is_int_schema(&self) -> bool;
}

impl IsSchema for serde_json::Map<String, serde_json::Value> {
    fn is_string_schema(&self) -> bool {
        for (_, v) in self
            .iter()
            .filter(|(k, _)| *k == "const" || *k == "enum" || *k == "pattern")
        {
            if let Some(array) = v.as_array() {
                if array.iter().all(|e| e.is_string()) {
                    return true;
                }
            } else if v.is_string() {
                return true;
            }
        }

        false
    }

    fn is_int_schema(&self) -> bool {
        for (_, v) in self
            .iter()
            .filter(|(k, _)| *k == "const" || *k == "enum" || *k == "minimum" || *k == "maximum")
        {
            if let Some(array) = v.as_array() {
                if array.iter().all(|e| e.is_i64()) {
                    return true;
                }
            } else if v.is_i64() {
                return true;
            }
        }
        false
    }
}
