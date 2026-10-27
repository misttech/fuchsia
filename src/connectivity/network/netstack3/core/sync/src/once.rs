// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! One-time initialization primitives.

use core::cell::UnsafeCell;
use core::fmt;
use core::mem::MaybeUninit;

/// A synchronization primitive which can be used to run a one-time
/// initialization.
///
/// # Safety
///
/// Implementations must guarantee that:
/// 1. [`Once::once`] executes the provided closure at most once across all
///    concurrent callers, and blocks all concurrent callers until that single
///    execution completes, establishing an acquire/release synchronization edge
///    from the completion of the closure to the return of [`Once::once`].
/// 2. [`Once::is_completed`] returns `true` if and only if an invocation of
///    [`Once::once`] has completed without panicking, establishing an acquire
///    synchronization edge with the completion of that closure.
/// 3. [`Default::default`] returns an uncompleted instance for which
///    [`Once::is_completed`] returns `false`.
pub unsafe trait Once: Default + Send + Sync + 'static {
    /// Performs a one-time initialization routine.
    ///
    /// The given closure `f` will be executed if and only if this [`Once`]
    /// has not already been completed. Concurrent calls to `once` block until
    /// initialization finishes.
    ///
    /// # Panics
    ///
    /// If `f` panics, the implementation is expected to panic on further
    /// attempts to initialize. Note that Netstack3 is usually used in
    /// `panic=abort` compilations, so we don't need anything stronger than that.
    fn once(&self, f: impl FnOnce());

    /// Returns `true` if some [`Once::once`] call has completed successfully.
    fn is_completed(&self) -> bool;
}

/// A synchronization primitive which can nominally be written to only once.
pub struct OnceLock<T, O: Once> {
    once: O,
    data: UnsafeCell<MaybeUninit<T>>,
}

// SAFETY: `OnceLock<T, O>` owns a `T` (once initialized) and an `O`. Sending
// it across threads requires `T: Send` and `O: Send`.
// This is the same bound as std::sync::OnceLock.
unsafe impl<T: Send, O: Once> Send for OnceLock<T, O> {}

// SAFETY: Sharing `&OnceLock<T, O>` across threads allows concurrent calls to
// `get_or_init` (which may initialize `T` on one thread and read `&T` on
// another) and `get` (which returns `&T`). This requires `T: Send + Sync` and
// `O: Sync`.
// This is the same bound as std::sync::OnceLock.
unsafe impl<T: Send + Sync, O: Once> Sync for OnceLock<T, O> {}

impl<T, O: Once> Default for OnceLock<T, O> {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl<T, O: Once> OnceLock<T, O> {
    /// Creates a new empty cell.
    #[inline]
    pub fn new() -> Self {
        Self { once: O::default(), data: UnsafeCell::new(MaybeUninit::uninit()) }
    }

    /// Gets the reference to the underlying value.
    ///
    /// Returns `None` if the cell is empty, or being initialized. This method
    /// never blocks.
    #[inline]
    pub fn get(&self) -> Option<&T> {
        if self.once.is_completed() {
            // SAFETY: `self.once.is_completed()` returned `true`, which by the
            // safety contract of `Once` guarantees `self.data` was initialized
            // in `get_or_init` with an acquire memory barrier and will not be
            // mutated again until `Drop`.
            Some(unsafe { self.get_unchecked() })
        } else {
            None
        }
    }

    /// Gets the contents of the cell, initializing it with `f` if the cell
    /// was empty.
    ///
    /// Many threads may call `get_or_init` concurrently with different
    /// initializing functions, but it is guaranteed that only one function
    /// will be executed.
    ///
    /// # Panics
    ///
    /// Panics if `f` panics. If `f` panics, the cell is left uninitialized and
    /// subsequent attempts to initialize the cell are expected to panic.
    #[inline]
    pub fn get_or_init<F: FnOnce() -> T>(&self, f: F) -> &T {
        if let Some(value) = self.get() {
            return value;
        }
        self.initialize(f);
        // SAFETY: `self.initialize` only returns after `self.once.once(...)`
        // has completed initializing `self.data`.
        unsafe { self.get_unchecked() }
    }

    #[cold]
    fn initialize<F: FnOnce() -> T>(&self, f: F) {
        let slot = &self.data;
        self.once.once(|| {
            let value = f();
            // SAFETY: `Once::once` guarantees exclusive access to this closure
            // while initializing `self.data` for the first and only time.
            let _: &mut T = unsafe { (*slot.get()).write(value) };
        });
    }

    /// # Safety
    ///
    /// The cell must be initialized (`self.once.is_completed()` must be `true`).
    #[inline]
    unsafe fn get_unchecked(&self) -> &T {
        // SAFETY: The caller guarantees that `self.data` has been initialized
        // and no mutable references can exist until `Drop`.
        unsafe { (*self.data.get()).assume_init_ref() }
    }
}

impl<T: fmt::Debug, O: Once> fmt::Debug for OnceLock<T, O> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut d = f.debug_tuple("OnceLock");
        let _: &mut fmt::DebugTuple<'_, '_> = match self.get() {
            Some(v) => d.field(&v),
            None => d.field(&format_args!("<uninit>")),
        };
        d.finish()
    }
}

impl<T: PartialEq, O: Once> PartialEq for OnceLock<T, O> {
    #[inline]
    fn eq(&self, other: &Self) -> bool {
        self.get() == other.get()
    }
}

impl<T: Eq, O: Once> Eq for OnceLock<T, O> {}

impl<T, O: Once> Drop for OnceLock<T, O> {
    #[inline]
    fn drop(&mut self) {
        if self.once.is_completed() {
            // SAFETY: `self.once.is_completed()` is `true`, so `self.data` is
            // initialized, and `&mut self` guarantees exclusive access.
            unsafe { self.data.get_mut().assume_init_drop() };
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use alloc::format;
    use alloc::sync::Arc;
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use std::thread;

    #[derive(Default)]
    struct StdOnce(std::sync::Once);

    // SAFETY: `std::sync::Once` upholds all the guarantees required by `Once`.
    unsafe impl Once for StdOnce {
        fn once(&self, f: impl FnOnce()) {
            let Self(once) = self;
            once.call_once(f);
        }

        fn is_completed(&self) -> bool {
            let Self(once) = self;
            once.is_completed()
        }
    }

    type TestOnceLock<T> = OnceLock<T, StdOnce>;

    #[test]
    fn once_lock_get_and_init() {
        let lock = TestOnceLock::<u32>::default();
        assert_eq!(lock.get(), None);
        assert_eq!(format!("{lock:?}"), "OnceLock(<uninit>)");

        let val = lock.get_or_init(|| 42);
        assert_eq!(*val, 42);
        assert_eq!(lock.get(), Some(&42));
        assert_eq!(format!("{lock:?}"), "OnceLock(42)");

        // Subsequent `get_or_init` calls must not run the closure.
        let val2 = lock.get_or_init(|| panic!("initializer called twice"));
        assert_eq!(*val2, 42);
    }

    #[test]
    fn once_lock_partial_eq() {
        let a = TestOnceLock::<u32>::new();
        let b = TestOnceLock::<u32>::new();
        assert_eq!(a, b);

        assert_eq!(*a.get_or_init(|| 10), 10);
        assert_ne!(a, b);

        assert_eq!(*b.get_or_init(|| 10), 10);
        assert_eq!(a, b);

        let c = TestOnceLock::<u32>::new();
        assert_eq!(*c.get_or_init(|| 20), 20);
        assert_ne!(a, c);
    }

    #[test]
    fn once_lock_drop_uninit() {
        // Dropping an uninitialized `OnceLock` must not drop `T`.
        struct DropBomb;
        impl Drop for DropBomb {
            fn drop(&mut self) {
                panic!("drop called");
            }
        }
        let _uninit = TestOnceLock::<DropBomb>::new();
    }

    #[test]
    fn once_lock_drop_init() {
        // Dropping an initialized `OnceLock` must drop `T` exactly once.
        struct DropTracker(Arc<AtomicUsize>);
        impl Drop for DropTracker {
            fn drop(&mut self) {
                let _: usize = self.0.fetch_add(1, Ordering::SeqCst);
            }
        }

        let drops = Arc::new(AtomicUsize::new(0));
        {
            let init = TestOnceLock::<DropTracker>::new();
            let _: &DropTracker = init.get_or_init(|| DropTracker(Arc::clone(&drops)));
            assert_eq!(drops.load(Ordering::SeqCst), 0);
        }
        assert_eq!(drops.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn once_lock_multithreaded_init() {
        const NUM_THREADS: usize = 8;

        let lock = TestOnceLock::<usize>::new();
        let init_calls = AtomicUsize::new(0);

        let loaded = thread::scope(|s| {
            let lock = &lock;
            let init_calls = &init_calls;
            let join = (0..NUM_THREADS)
                .map(|i| {
                    s.spawn(move || {
                        lock.get_or_init(|| {
                            let _: usize = init_calls.fetch_add(1, Ordering::SeqCst);
                            i
                        })
                    })
                })
                .collect::<Vec<_>>();
            // Every thread observes exactly the same value from the lock.
            join.into_iter()
                .fold(None, |acc, val| {
                    let val = val.join().expect("thread complete");
                    assert!(acc.is_none_or(|v| v == *val));
                    Some(*val)
                })
                .expect("at least one item")
        });

        assert_eq!(init_calls.load(Ordering::SeqCst), 1);
        assert_eq!(lock.get(), Some(&loaded));
    }

    #[test]
    #[cfg(panic = "unwind")]
    fn once_lock_panic() {
        #[derive(Debug, PartialEq)]
        struct DropBomb;
        impl Drop for DropBomb {
            fn drop(&mut self) {
                panic!("drop called on uninitialized value");
            }
        }

        let lock = TestOnceLock::<DropBomb>::new();

        // The first attempt to initialize panics.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: &DropBomb = lock.get_or_init(|| panic!("initialization failure"));
        }));
        assert!(result.is_err());

        // The lock remains uninitialized.
        assert_eq!(lock.get(), None);

        // Subsequent attempts to initialize must also panic.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _: &DropBomb = lock.get_or_init(|| DropBomb);
        }));
        assert!(result.is_err());

        // The lock remains uninitialized and dropping it must not drop `T`.
        assert_eq!(lock.get(), None);
    }
}
