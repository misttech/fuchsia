// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fifo_cache::FifoCache;
use crate::policy::{AccessDecision, IoctlAccessDecision};
use crate::security_server::SecurityServerBackend;
use crate::sync::Mutex;
use crate::{FsNodeClass, KernelClass, NullessByteStr, ObjectClass, SecurityId};
use std::sync::Arc;

pub use crate::fifo_cache::CacheStats;

/// Interface used internally by the `SecurityServer` implementation to implement policy queries
/// such as looking up the set of permissions to grant, or the Security Context to apply to new
/// files, etc.
///
/// This trait allows layering of caching, delegation, and thread-safety between the policy-backed
/// calculations, and the caller-facing permission-check interface.
pub(super) trait Query {
    /// Computes the [`AccessDecision`] permitted to `source_sid` for accessing `target_sid`, an
    /// object of of type `target_class`.
    fn compute_access_decision(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
    ) -> AccessDecision;

    /// Returns the security identifier (SID) with which to label a new `fs_node_class` instance
    /// created by `source_sid` in a parent directory labeled `target_sid` should be labeled,
    /// if no more specific SID was specified by `compute_new_fs_node_sid_with_name()`, based on
    /// the file's name.
    fn compute_new_fs_node_sid(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        fs_node_class: FsNodeClass,
    ) -> Result<SecurityId, anyhow::Error>;

    /// Returns the security identifier (SID) with which to label a new `fs_node_class` instance of
    /// name `fs_node_name`, created by `source_sid` in a parent directory labeled `target_sid`.
    /// If no filename-transition rules exist for the specified `fs_node_name` then `None` is
    /// returned.
    fn compute_new_fs_node_sid_with_name(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        fs_node_class: FsNodeClass,
        fs_node_name: NullessByteStr<'_>,
    ) -> Option<SecurityId>;

    /// Computes the [`IoctlAccessDecision`] permitted to `source_sid` for accessing `target_sid`,
    /// an object of of type `target_class`, for ioctls with high byte `ioctl_prefix`.
    fn compute_ioctl_access_decision(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
        ioctl_prefix: u8,
    ) -> IoctlAccessDecision;
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct AccessQueryArgs {
    source_sid: SecurityId,
    target_sid: SecurityId,
    target_class: ObjectClass,
}

#[derive(Clone)]
struct AccessQueryResult {
    access_decision: AccessDecision,
    new_file_sid: Option<SecurityId>,
}

#[derive(Clone, Hash, PartialEq, Eq)]
struct IoctlAccessQueryArgs {
    source_sid: SecurityId,
    target_sid: SecurityId,
    target_class: ObjectClass,
    ioctl_prefix: u8,
}

/// Thread-hostile associative cache with capacity defined at construction and FIFO eviction.
pub(super) struct FifoQueryCache {
    access_cache: FifoCache<AccessQueryArgs, AccessQueryResult>,
    ioctl_access_cache: FifoCache<IoctlAccessQueryArgs, IoctlAccessDecision>,
}

impl FifoQueryCache {
    // The multiplier used to compute the ioctl access cache capacity from the main cache capacity.
    const IOCTL_CAPACITY_MULTIPLIER: f32 = 0.25;

    /// Constructs a fixed-size access vector cache.
    ///
    /// # Panics
    ///
    /// This will panic if called with a `capacity` of zero.
    pub fn new(capacity: usize) -> Self {
        assert!(capacity > 0, "cannot instantiate fixed access vector cache of size 0");
        let ioctl_access_cache_capacity =
            (Self::IOCTL_CAPACITY_MULTIPLIER * (capacity as f32)) as usize;
        assert!(
            ioctl_access_cache_capacity > 0,
            "cannot instantiate ioctl cache partition of size 0"
        );

        Self {
            // Request `capacity` plus one element working-space for insertions that trigger
            // an eviction.
            access_cache: FifoCache::with_capacity(capacity),
            ioctl_access_cache: FifoCache::with_capacity(ioctl_access_cache_capacity),
        }
    }

    pub fn cache_stats(&self) -> CacheStats {
        &self.access_cache.cache_stats() + &self.ioctl_access_cache.cache_stats()
    }

    pub fn compute_access_decision(
        &mut self,
        delegate: &impl Query,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
    ) -> AccessDecision {
        let query_args =
            AccessQueryArgs { source_sid, target_sid, target_class: target_class.clone() };
        if let Some(result) = self.access_cache.get(&query_args) {
            return result.access_decision.clone();
        }

        let access_decision =
            delegate.compute_access_decision(source_sid, target_sid, target_class);

        self.access_cache.insert(
            query_args,
            AccessQueryResult { access_decision: access_decision.clone(), new_file_sid: None },
        );

        access_decision
    }

    pub fn compute_new_fs_node_sid(
        &mut self,
        delegate: &impl Query,
        source_sid: SecurityId,
        target_sid: SecurityId,
        fs_node_class: FsNodeClass,
    ) -> Result<SecurityId, anyhow::Error> {
        let target_class = ObjectClass::Kernel(KernelClass::from(fs_node_class));

        let query_args =
            AccessQueryArgs { source_sid, target_sid, target_class: target_class.clone() };
        let query_result = if let Some(result) = self.access_cache.get(&query_args) {
            result
        } else {
            let access_decision =
                delegate.compute_access_decision(source_sid, target_sid, target_class);
            self.access_cache.insert(
                query_args.clone(),
                AccessQueryResult { access_decision, new_file_sid: None },
            )
        };

        if let Some(new_file_sid) = query_result.new_file_sid {
            Ok(new_file_sid)
        } else {
            let new_file_sid =
                delegate.compute_new_fs_node_sid(source_sid, target_sid, fs_node_class);
            if let Ok(new_file_sid) = new_file_sid {
                let updated_query_result = AccessQueryResult {
                    access_decision: query_result.access_decision.clone(),
                    new_file_sid: Some(new_file_sid),
                };
                self.access_cache.replace(query_args, updated_query_result);
            }
            new_file_sid
        }
    }

    pub fn compute_new_fs_node_sid_with_name(
        &mut self,
        delegate: &impl Query,
        source_sid: SecurityId,
        target_sid: SecurityId,
        fs_node_class: FsNodeClass,
        fs_node_name: NullessByteStr<'_>,
    ) -> Option<SecurityId> {
        delegate.compute_new_fs_node_sid_with_name(
            source_sid,
            target_sid,
            fs_node_class,
            fs_node_name,
        )
    }

    pub fn compute_ioctl_access_decision(
        &mut self,
        delegate: &impl Query,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
        ioctl_prefix: u8,
    ) -> IoctlAccessDecision {
        let query_args = IoctlAccessQueryArgs {
            source_sid,
            target_sid,
            target_class: target_class.clone(),
            ioctl_prefix,
        };
        if let Some(result) = self.ioctl_access_cache.get(&query_args) {
            return result.clone();
        }

        let ioctl_access_decision = delegate.compute_ioctl_access_decision(
            source_sid,
            target_sid,
            target_class,
            ioctl_prefix,
        );

        self.ioctl_access_cache.insert(query_args, ioctl_access_decision.clone());

        ioctl_access_decision
    }

    pub fn reset(&mut self) -> bool {
        self.access_cache = FifoCache::with_capacity(self.access_cache.capacity());
        self.ioctl_access_cache = FifoCache::with_capacity(self.ioctl_access_cache.capacity());
        true
    }

    /// Returns true if the main access decision cache has reached capacity.
    #[cfg(test)]
    fn access_cache_is_full(&self) -> bool {
        self.access_cache.is_full()
    }

    /// Returns true if the ioctl access decision cache has reached capacity.
    #[cfg(test)]
    fn ioctl_access_cache_is_full(&self) -> bool {
        self.ioctl_access_cache.is_full()
    }
}

/// Default size of an access vector cache shared by all threads in the system.
const DEFAULT_SHARED_SIZE: usize = 1000;

/// An access vector cache.
#[derive(Clone)]
pub(super) struct AccessVectorCache {
    cache: Arc<Mutex<FifoQueryCache>>,
    backend: Arc<SecurityServerBackend>,
}

impl AccessVectorCache {
    pub fn new(backend: Arc<SecurityServerBackend>) -> Self {
        let cache = FifoQueryCache::new(DEFAULT_SHARED_SIZE);
        Self { cache: Arc::new(Mutex::new(cache)), backend }
    }

    pub fn cache_stats(&self) -> CacheStats {
        self.cache.lock().cache_stats()
    }

    pub fn reset(&self) -> bool {
        self.cache.lock().reset()
    }
}

impl Query for AccessVectorCache {
    fn compute_access_decision(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
    ) -> AccessDecision {
        self.cache.lock().compute_access_decision(
            self.backend.as_ref(),
            source_sid,
            target_sid,
            target_class,
        )
    }

    fn compute_new_fs_node_sid(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        fs_node_class: FsNodeClass,
    ) -> Result<SecurityId, anyhow::Error> {
        self.cache.lock().compute_new_fs_node_sid(
            self.backend.as_ref(),
            source_sid,
            target_sid,
            fs_node_class,
        )
    }

    fn compute_new_fs_node_sid_with_name(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        fs_node_class: FsNodeClass,
        fs_node_name: NullessByteStr<'_>,
    ) -> Option<SecurityId> {
        self.cache.lock().compute_new_fs_node_sid_with_name(
            self.backend.as_ref(),
            source_sid,
            target_sid,
            fs_node_class,
            fs_node_name,
        )
    }

    fn compute_ioctl_access_decision(
        &self,
        source_sid: SecurityId,
        target_sid: SecurityId,
        target_class: ObjectClass,
        ioctl_prefix: u8,
    ) -> IoctlAccessDecision {
        self.cache.lock().compute_ioctl_access_decision(
            self.backend.as_ref(),
            source_sid,
            target_sid,
            target_class,
            ioctl_prefix,
        )
    }
}

/// Test constants and helpers shared by `tests` and `starnix_tests`.
#[cfg(test)]
mod testing {
    use crate::SecurityId;

    use std::num::NonZeroU32;
    use std::sync::LazyLock;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// SID to use where any value will do.
    pub(super) static A_TEST_SID: LazyLock<SecurityId> = LazyLock::new(unique_sid);

    /// Default fixed cache capacity to request in tests.
    pub(super) const TEST_CAPACITY: usize = 10;

    /// Returns a new `SecurityId` with unique id.
    pub(super) fn unique_sid() -> SecurityId {
        static NEXT_ID: AtomicU32 = AtomicU32::new(1000);
        SecurityId(NonZeroU32::new(NEXT_ID.fetch_add(1, Ordering::AcqRel)).unwrap())
    }

    /// Returns a vector of `count` unique `SecurityIds`.
    pub(super) fn unique_sids(count: usize) -> Vec<SecurityId> {
        (0..count).map(|_| unique_sid()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;
    use crate::KernelClass;
    use crate::policy::{AccessVector, XpermsBitmap};

    use std::sync::atomic::{AtomicUsize, Ordering};

    /// No-op policy query delegate that allows all permissions and maintains no internal state, for testing.
    #[derive(Default)]
    struct TestDelegate {
        query_count: AtomicUsize,
    }

    impl TestDelegate {
        fn query_count(&self) -> usize {
            self.query_count.load(Ordering::Relaxed)
        }
    }

    impl Query for TestDelegate {
        fn compute_access_decision(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: ObjectClass,
        ) -> AccessDecision {
            self.query_count.fetch_add(1, Ordering::Relaxed);
            AccessDecision::allow(AccessVector::ALL)
        }

        fn compute_new_fs_node_sid(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _fs_node_class: FsNodeClass,
        ) -> Result<SecurityId, anyhow::Error> {
            unreachable!()
        }

        fn compute_new_fs_node_sid_with_name(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _fs_node_class: FsNodeClass,
            _fs_node_name: NullessByteStr<'_>,
        ) -> Option<SecurityId> {
            unreachable!()
        }

        fn compute_ioctl_access_decision(
            &self,
            _source_sid: SecurityId,
            _target_sid: SecurityId,
            _target_class: ObjectClass,
            _ioctl_prefix: u8,
        ) -> IoctlAccessDecision {
            self.query_count.fetch_add(1, Ordering::Relaxed);
            IoctlAccessDecision::ALLOW_ALL
        }
    }

    #[test]
    fn fixed_access_vector_cache_add_entry() {
        let delegate = TestDelegate::default();
        let mut avc = FifoQueryCache::new(TEST_CAPACITY);
        assert_eq!(0, delegate.query_count());
        assert_eq!(
            AccessVector::ALL,
            avc.compute_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into()
            )
            .allow
        );
        assert_eq!(1, delegate.query_count());
        assert_eq!(
            AccessVector::ALL,
            avc.compute_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into()
            )
            .allow
        );
        assert_eq!(1, delegate.query_count());
        assert_eq!(false, avc.access_cache_is_full());
    }

    #[test]
    fn fixed_access_vector_cache_reset() {
        let delegate = TestDelegate::default();
        let mut avc = FifoQueryCache::new(TEST_CAPACITY);

        avc.reset();
        assert_eq!(false, avc.access_cache_is_full());

        assert_eq!(0, delegate.query_count());
        assert_eq!(
            AccessVector::ALL,
            avc.compute_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into()
            )
            .allow
        );
        assert_eq!(1, delegate.query_count());
        assert_eq!(false, avc.access_cache_is_full());

        avc.reset();
        assert_eq!(false, avc.access_cache_is_full());
    }

    #[test]
    fn fixed_access_vector_cache_fill() {
        let delegate = TestDelegate::default();
        let mut avc = FifoQueryCache::new(TEST_CAPACITY);

        for sid in unique_sids(avc.access_cache.capacity()) {
            avc.compute_access_decision(
                &delegate,
                sid,
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
            );
        }
        assert_eq!(true, avc.access_cache_is_full());

        avc.reset();
        assert_eq!(false, avc.access_cache_is_full());

        for sid in unique_sids(avc.access_cache.capacity()) {
            avc.compute_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                sid,
                KernelClass::Process.into(),
            );
        }
        assert_eq!(true, avc.access_cache_is_full());

        avc.reset();
        assert_eq!(false, avc.access_cache_is_full());
    }

    #[test]
    fn fixed_access_vector_cache_full_miss() {
        let delegate = TestDelegate::default();
        let mut avc = FifoQueryCache::new(TEST_CAPACITY);

        // Make the test query, which will trivially miss.
        avc.compute_access_decision(
            &delegate,
            A_TEST_SID.clone(),
            A_TEST_SID.clone(),
            KernelClass::Process.into(),
        );
        assert!(!avc.access_cache_is_full());

        // Fill the cache with new queries, which should evict the test query.
        for sid in unique_sids(avc.access_cache.capacity()) {
            avc.compute_access_decision(
                &delegate,
                sid,
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
            );
        }
        assert!(avc.access_cache_is_full());

        // Making the test query should result in another miss.
        let delegate_query_count = delegate.query_count();
        avc.compute_access_decision(
            &delegate,
            A_TEST_SID.clone(),
            A_TEST_SID.clone(),
            KernelClass::Process.into(),
        );
        assert_eq!(delegate_query_count + 1, delegate.query_count());

        // Because the cache is not LRU, making `capacity()` unique queries, each preceded by
        // the test query, will still result in the test query result being evicted.
        // Each test query will hit, and the interleaved queries will miss, with the final of the
        // interleaved queries evicting the test query.
        for sid in unique_sids(avc.access_cache.capacity()) {
            avc.compute_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
            );
            avc.compute_access_decision(
                &delegate,
                sid,
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
            );
        }

        // The test query should now miss.
        let delegate_query_count = delegate.query_count();
        avc.compute_access_decision(
            &delegate,
            A_TEST_SID.clone(),
            A_TEST_SID.clone(),
            KernelClass::Process.into(),
        );
        assert_eq!(delegate_query_count + 1, delegate.query_count());
    }

    #[test]
    fn access_vector_cache_ioctl_hit() {
        let delegate = TestDelegate::default();
        let mut avc = FifoQueryCache::new(TEST_CAPACITY);
        assert_eq!(0, delegate.query_count());
        assert_eq!(
            XpermsBitmap::ALL,
            avc.compute_ioctl_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
                0x0,
            )
            .allow
        );
        assert_eq!(1, delegate.query_count());
        // The second request for the same key is a cache hit.
        assert_eq!(
            XpermsBitmap::ALL,
            avc.compute_ioctl_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
                0x0
            )
            .allow
        );
        assert_eq!(1, delegate.query_count());
    }

    #[test]
    fn access_vector_cache_ioctl_miss() {
        let delegate = TestDelegate::default();
        let mut avc = FifoQueryCache::new(TEST_CAPACITY);

        // Make the test query, which will trivially miss.
        avc.compute_ioctl_access_decision(
            &delegate,
            A_TEST_SID.clone(),
            A_TEST_SID.clone(),
            KernelClass::Process.into(),
            0x0,
        );

        // Fill the ioctl cache with new queries, which should evict the test query.
        for ioctl_prefix in 0x1..(1 + avc.ioctl_access_cache.capacity())
            .try_into()
            .expect("assumed that test ioctl cache capacity was < 255")
        {
            avc.compute_ioctl_access_decision(
                &delegate,
                A_TEST_SID.clone(),
                A_TEST_SID.clone(),
                KernelClass::Process.into(),
                ioctl_prefix,
            );
        }
        // Make sure that we've fulfilled at least one new cache miss since the original test query,
        // and that the cache is now full.
        assert!(delegate.query_count() > 1);
        assert!(avc.ioctl_access_cache_is_full());
        let delegate_query_count = delegate.query_count();

        // Making the original test query again should result in another miss.
        avc.compute_ioctl_access_decision(
            &delegate,
            A_TEST_SID.clone(),
            A_TEST_SID.clone(),
            KernelClass::Process.into(),
            0x0,
        );
        assert_eq!(delegate_query_count + 1, delegate.query_count());
    }
}
