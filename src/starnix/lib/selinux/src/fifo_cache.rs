// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use indexmap::IndexMap;
use std::hash::Hash;
use std::ops::Add;
use std::sync::atomic::{AtomicU64, Ordering};

/// Describes the performance statistics of a cache implementation.
#[derive(Default, Debug, Clone, PartialEq)]
pub struct CacheStats {
    /// Cumulative count of lookups performed on the cache.
    pub lookups: u64,
    /// Cumulative count of lookups that returned data from an existing cache entry.
    pub hits: u64,
    /// Cumulative count of lookups that did not match any existing cache entry.
    pub misses: u64,
    /// Cumulative count of insertions into the cache.
    pub allocs: u64,
    /// Cumulative count of evictions from the cache, to make space for a new insertion.
    pub reclaims: u64,
    /// Cumulative count of evictions from the cache due to no longer being deemed relevant.
    /// This is not used in our current implementation.
    pub frees: u64,
}

impl Add for &CacheStats {
    type Output = CacheStats;

    fn add(self, other: &CacheStats) -> CacheStats {
        CacheStats {
            lookups: self.lookups + other.lookups,
            hits: self.hits + other.hits,
            misses: self.misses + other.misses,
            allocs: self.allocs + other.allocs,
            reclaims: self.reclaims + other.reclaims,
            frees: self.frees + other.frees,
        }
    }
}

#[derive(Default, Debug)]
struct AtomicCacheStats {
    lookups: AtomicU64,
    hits: AtomicU64,
    misses: AtomicU64,
    allocs: AtomicU64,
    reclaims: AtomicU64,
    frees: AtomicU64,
}

impl AtomicCacheStats {
    fn snapshot(&self) -> CacheStats {
        CacheStats {
            lookups: self.lookups.load(Ordering::Relaxed),
            hits: self.hits.load(Ordering::Relaxed),
            misses: self.misses.load(Ordering::Relaxed),
            allocs: self.allocs.load(Ordering::Relaxed),
            reclaims: self.reclaims.load(Ordering::Relaxed),
            frees: self.frees.load(Ordering::Relaxed),
        }
    }
}

/// Associative FIFO cache with capacity defined at creation.
///
/// Lookups in the cache are O(1), as are evictions.
///
/// This implementation is thread-hostile; it expects all operations to be executed on the same
/// thread.
pub(super) struct FifoCache<A: Hash + Eq, R> {
    cache: IndexMap<A, R>,
    capacity: usize,
    oldest_index: usize,
    stats: AtomicCacheStats,
}

impl<A: Hash + Eq, R> FifoCache<A, R> {
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "cannot instantiate fixed access vector cache of size 0");

        Self {
            // Request `capacity` plus one element working-space for insertions that trigger
            // an eviction.
            cache: IndexMap::with_capacity(capacity + 1),
            capacity,
            oldest_index: 0,
            stats: AtomicCacheStats::default(),
        }
    }

    /// Returns true if the cache has reached capacity.
    #[inline]
    pub fn is_full(&self) -> bool {
        self.cache.len() == self.capacity
    }

    /// Returns the capacity with which this instance was initialized.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Searches the cache and returns a reference to the result `R` matching
    /// the given `query_args` (or returns `None`).
    #[inline]
    pub fn get(&self, query_args: &A) -> Option<&R> {
        self.stats.lookups.fetch_add(1, Ordering::Relaxed);

        let result = self.cache.get(query_args);

        if result.is_some() {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.stats.misses.fetch_add(1, Ordering::Relaxed);
        }

        result
    }

    /// Replaces the entry for the specified `query` with the specified `result`.
    ///
    /// # Panics
    ///
    /// Panics if the specified `query` is not already in the cache.
    pub fn replace(&mut self, query: A, result: R) {
        let old_result = self.cache.insert(query, result);
        // We must be replacing an existing entry.
        assert!(old_result.is_some());
    }

    /// Inserts the specified `query` and `result` into the cache, evicting the oldest existing
    /// entry if capacity has been reached.
    #[inline]
    pub fn insert(&mut self, query: A, result: R) -> &mut R {
        self.stats.allocs.fetch_add(1, Ordering::Relaxed);

        // If the cache is already full then it will be necessary to evict an existing entry.
        // Eviction is performed after inserting the new entry, to allow the eviction operation to
        // be implemented via swap-into-place.
        let must_evict = self.is_full();

        // Insert the entry, at the end of the `IndexMap` queue, then evict the oldest element.
        let (mut index, _) = self.cache.insert_full(query, result);
        if must_evict {
            // The final element in the ordered container is the newly-added entry, so we can simply
            // swap it with the oldest element, and then remove the final element, to achieve FIFO
            // eviction.
            assert_eq!(index, self.capacity);

            self.cache.swap_remove_index(self.oldest_index);
            self.stats.reclaims.fetch_add(1, Ordering::Relaxed);

            index = self.oldest_index;

            self.oldest_index += 1;
            if self.oldest_index == self.capacity {
                self.oldest_index = 0;
            }
        }

        self.cache.get_index_mut(index).map(|(_, v)| v).expect("invalid index after insert!")
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.stats.snapshot()
    }
}
