// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use starnix_rcu::rcu_cache::RcuCacheInsertionResult;
use starnix_rcu::{RcuCache, RcuReadScope};
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

    fn reset(&self) {
        self.lookups.store(0, Ordering::Relaxed);
        self.hits.store(0, Ordering::Relaxed);
        self.misses.store(0, Ordering::Relaxed);
        self.allocs.store(0, Ordering::Relaxed);
        self.reclaims.store(0, Ordering::Relaxed);
        self.frees.store(0, Ordering::Relaxed);
    }
}

/// Associative FIFO cache with capacity defined at creation.
///
/// Lookups in the cache are O(1), as are evictions.
///
/// This implementation is thread-hostile; it expects all operations to be executed on the same
/// thread.
pub(super) struct AccessCache<A, R>
where
    A: Eq + Hash + Clone + Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    cache: RcuCache<A, R>,
    stats: AtomicCacheStats,
}

impl<A, R> AccessCache<A, R>
where
    A: Hash + Eq + Clone + Send + Sync + 'static,
    R: Clone + Send + Sync + 'static,
{
    /// Creates an access cache with the given capacity.
    ///
    /// # Panics
    ///
    /// Panics if `capacity` is 0.
    pub fn with_capacity(capacity: usize) -> Self {
        assert!(capacity > 0, "cannot instantiate fixed access vector cache of size 0");
        Self { cache: RcuCache::new(capacity), stats: AtomicCacheStats::default() }
    }

    /// Returns the capacity with which this instance was initialized.
    #[cfg(test)]
    pub fn capacity(&self) -> usize {
        self.cache.capacity()
    }

    /// Searches the cache and returns a reference to the result `R` matching
    /// the given `query_args`.
    ///
    /// If the result is not found in the cache, the `callback` is invoked to
    /// compute the result, which is then inserted into the cache.
    #[inline]
    pub fn get_or_insert(&self, query_args: A, callback: impl FnOnce() -> R) -> R {
        self.get_or_try_insert::<()>(query_args, || Ok(callback())).expect("infallible callback")
    }

    /// Searches the cache and returns a reference to the result `R` matching
    /// the given `query_args`.
    ///
    /// If the result is not found in the cache, the `callback` is invoked to
    /// compute the result, which is then inserted into the cache.
    ///
    /// # Errors
    ///
    /// If the `callback` returns an error, it is propagated to the caller.
    #[inline]
    pub fn get_or_try_insert<E>(
        &self,
        query_args: A,
        callback: impl FnOnce() -> Result<R, E>,
    ) -> Result<R, E> {
        self.stats.lookups.fetch_add(1, Ordering::Relaxed);

        if let Some(result) = self.cache.get(&RcuReadScope::new(), &query_args) {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(result.clone());
        }

        // Wait for our turn to write to the cache.
        let guard = self.cache.lock();
        let scope = RcuReadScope::new();

        // Check to see if another thread has already populated this entry in the cache.
        if let Some(result) = self.cache.get(&scope, &query_args) {
            self.stats.hits.fetch_add(1, Ordering::Relaxed);
            return Ok(result.clone());
        }

        self.stats.misses.fetch_add(1, Ordering::Relaxed);
        let result = callback()?;

        self.stats.allocs.fetch_add(1, Ordering::Relaxed);
        if let RcuCacheInsertionResult::Evicted(_) =
            guard.insert(&scope, query_args, result.clone())
        {
            self.stats.reclaims.fetch_add(1, Ordering::Relaxed);
        }
        Ok(result)
    }

    #[cfg(test)]
    pub fn is_full(&self) -> bool {
        self.cache.len() >= self.capacity()
    }

    pub fn reset(&self) {
        self.cache.clear();
        self.stats.reset();
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.stats.snapshot()
    }
}
