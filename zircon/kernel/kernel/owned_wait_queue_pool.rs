// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Global pool for [`OwnedWaitQueue`] objects.

use crate::counters::define_kcounter;
use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::object_cache::{ObjectCache, UniquePtr};
use core::convert::Infallible;
use core::mem::{MaybeUninit, align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use core::sync::atomic::{AtomicU32, Ordering};
use fbl::DoublyLinkedList;
use ksync::{KMutex, KSpinlock, LockToken, PhantomMutex};
use lazy_init::LazyInit;
use owned_wait_queue_pool_bindings as bindings;
use pin_init::{InPlaceWrite, PinInit, init_from_closure, pin_data, pin_init};
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_status_t;

define_kcounter!(OWQ_POOL_EXCESS_CAPACITY, "owned_wait_queue.pool.excess_capacity", Sum);

ksync::declare_singleton_spinlock!(
    /// Global lock for [`OwnedWaitQueuePool`].
    pub OwnedWaitQueuePoolLock
);

static G_POOL: LazyInit<OwnedWaitQueuePool> = LazyInit::uninit();

fn owned_wait_queue_pool_init(_level: init::LkInitLevel) {
    // SAFETY: Called once during single-threaded early boot at `LK_INIT_LEVEL_THREADING - 1`
    // before any concurrent access to `G_POOL`.
    let _ = unsafe { Pin::static_ref(&G_POOL).init_pin(OwnedWaitQueuePool::new()) };
}

init::lk_init_hook!(
    owq_pool,
    owned_wait_queue_pool_init,
    init::LkInitLevel(init::LK_INIT_LEVEL_THREADING.0 - 1)
);

/// Global pool for [`OwnedWaitQueue`] objects.
#[ksync::guarded]
#[pin_data]
#[repr(C)]
pub struct OwnedWaitQueuePool {
    #[pin]
    cache: ObjectCache<OwnedWaitQueue>,

    #[mutex(OwnedWaitQueuePoolLock)]
    lock: KMutex<PhantomMutex>,

    /// Use a [`DoublyLinkedList`] so that we can reuse OWQ's existing storage and mitigate
    /// contention from optimistic concurrency by pushing/popping from opposite ends.
    /// [`OwnedWaitQueue::assert_safe_for_pooling`] is used to ensure that we can safely reuse the
    /// storage.
    #[pin]
    #[guarded_by(lock)]
    free_list: DoublyLinkedList<NonNull<OwnedWaitQueue>>,

    active_threads: AtomicU32,
    total_allocated: AtomicU32,
}

// SAFETY: `OwnedWaitQueuePool` contains `ObjectCache<OwnedWaitQueue>` (which is `Send + Sync`),
// atomic counters, and `free_list` guarded by `OwnedWaitQueuePoolLock`. The `NonNull` pointers in
// `free_list` refer to `OwnedWaitQueue` instances which are `Send + Sync`.
unsafe impl Send for OwnedWaitQueuePool {}
// SAFETY: All interior mutation of `free_list` is synchronized by `OwnedWaitQueuePoolLock`, and
// `cache`, `active_threads`, and `total_allocated` are thread-safe (`Sync`).
unsafe impl Sync for OwnedWaitQueuePool {}

const EXPECTED_CACHE_SIZE: usize = size_of::<ObjectCache<OwnedWaitQueue>>();
const EXPECTED_POOL_SIZE: usize = EXPECTED_CACHE_SIZE + 16;

static_assert!(size_of::<OwnedWaitQueuePool>() == size_of::<bindings::OwnedWaitQueuePool>());
static_assert!(align_of::<OwnedWaitQueuePool>() == align_of::<bindings::OwnedWaitQueuePool>());
static_assert!(size_of::<OwnedWaitQueuePool>() == EXPECTED_POOL_SIZE);
static_assert!(align_of::<OwnedWaitQueuePool>() == 8);
static_assert!(offset_of!(OwnedWaitQueuePool, cache) == 0);
static_assert!(offset_of!(OwnedWaitQueuePool, free_list) == EXPECTED_CACHE_SIZE);
static_assert!(offset_of!(OwnedWaitQueuePool, active_threads) == EXPECTED_CACHE_SIZE + 8);
static_assert!(offset_of!(OwnedWaitQueuePool, total_allocated) == EXPECTED_CACHE_SIZE + 12);

impl OwnedWaitQueuePool {
    /// Creates an in-place initializer for an [`OwnedWaitQueuePool`].
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            cache <- ObjectCache::new(0),
            lock: KMutex::new(PhantomMutex),
            free_list <- ksync::kcell_init(DoublyLinkedList::new()),
            active_threads: AtomicU32::new(0),
            total_allocated: AtomicU32::new(0),
        })
    }

    /// Returns the global singleton instance.
    #[inline]
    pub fn get() -> &'static Self {
        G_POOL.get()
    }

    /// Increases the pool size by one if the number of active threads exceeds the current pool
    /// capacity.
    ///
    /// May block the caller.
    ///
    /// Must be called during [`Thread`](crate::kernel::thread::Thread) construction to ensure pool
    /// invariants are upheld.
    ///
    /// Returns `Err(Status::NO_MEMORY)` if allocation fails.
    pub fn grow(&self) -> Result<(), Status> {
        OWQ_POOL_EXCESS_CAPACITY.add(-1);
        // Ordering: `AcqRel` synchronizes with `Release` updates to `active_threads` and
        // `total_allocated` across threads.
        let active = self.active_threads.fetch_add(1, Ordering::AcqRel) + 1;
        // Ordering: `Acquire` pairs with the `Release` increment of `total_allocated` after a
        // newly allocated queue is recycled into the pool.
        let capacity = self.total_allocated.load(Ordering::Acquire);

        if active <= capacity {
            return Ok(());
        }
        core::hint::cold_path();

        // SAFETY: `OwnedWaitQueue::init()` initializes every field of `slot` infallibly, and the
        // `OwnedWaitQueue` allocated in the `ObjectCache` slab slot is never moved before being
        // dropped in place by `UniquePtr::drop`.
        let result = self.cache.allocate_init(unsafe {
            init_from_closure(|slot: *mut OwnedWaitQueue| -> Result<(), Status> {
                let Ok(()) = OwnedWaitQueue::init().__pinned_init(slot);
                Ok(())
            })
        });
        let queue = match result {
            Ok(queue) => queue,
            Err(status) => {
                core::hint::cold_path();
                // Ordering: `Release` orders prior operations before rolling back the active thread
                // count.
                self.active_threads.fetch_sub(1, Ordering::Release);
                OWQ_POOL_EXCESS_CAPACITY.add(1);
                return Err(status);
            }
        };

        ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
        // SAFETY: `queue` was just allocated and initialized by `self.cache.allocate_init` with no
        // owner and no waiters, and is not in any list. `UniquePtr::into_raw` is non-null.
        unsafe {
            self.recycle_locked(
                NonNull::new_unchecked(UniquePtr::into_raw(queue)),
                guard.as_mut().token_mut(),
            );
        }
        // Ordering: `Release` ensures that recycling the queue into `free_list` happens-before
        // `total_allocated` is observed by `Acquire` loads.
        self.total_allocated.fetch_add(1, Ordering::Release);
        OWQ_POOL_EXCESS_CAPACITY.add(1);
        Ok(())
    }

    /// Decreases the active thread count.
    ///
    /// Must be called during [`Thread`](crate::kernel::thread::Thread) destruction or resource
    /// cleanup.
    ///
    /// Note that this does not currently shrink the size of the pool, it shrinks the number of
    /// active threads tracked by the pool. This allows new threads in the future to reuse already
    /// allocated [`OwnedWaitQueue`]s.
    #[inline]
    pub fn shrink(&self) {
        // Ordering: `Release` synchronizes with the `AcqRel` increment of `active_threads` in
        // `grow`.
        let old_active = self.active_threads.fetch_sub(1, Ordering::Release);
        debug_assert!(old_active > 0, "old_active must be greater than 0, got {old_active}");
        OWQ_POOL_EXCESS_CAPACITY.add(1);

        // This would be a reasonable place to free unneeded OWQs, but we lack a mechanism to
        // guarantee that no thread exit frees a queue from the pool that is still being used by a
        // stale chainlock transaction.
        // TODO(https://fxbug.dev/488166372) actually free excess OWQs
    }

    /// Acquires the pool lock for external synchronization.
    ///
    /// If this lock will be held at the same time as [`OwnedWaitQueue`]'s lock, then
    /// [`OwnedWaitQueue`]'s lock must be acquired first.
    #[inline]
    pub fn get_lock(&self) -> &'static KSpinlock<OwnedWaitQueuePoolLock> {
        OwnedWaitQueuePoolLock::get()
    }

    /// Borrow a queue from the pool.
    #[inline]
    pub fn borrow_locked(
        &self,
        token: &mut LockToken<'_, OwnedWaitQueuePoolLock>,
    ) -> NonNull<OwnedWaitQueue> {
        // SAFETY: `OwnedWaitQueuePoolLock` is the global lock protecting `self.free_list`, and
        // `self` is pinned by its `PinInit` construction invariant.
        let mut guard = unsafe { self.guard_lock_mut(token) };
        let queue = guard.free_list_mut().pop_front();
        assert!(queue.is_some(), "Pool cannot be empty.");
        queue.unwrap()
    }

    /// Recycle a queue back to the pool.
    ///
    /// The queue must be in a clean state (no owner, no waiters).
    ///
    /// # Safety
    ///
    /// `queue` must point to a valid, pinned [`OwnedWaitQueue`] allocated from this pool's cache
    /// in a clean state (no owner, no waiters, and no concurrent mutation of its owner or waiter
    /// fields) that is not currently in any list, and must remain valid and unmoved while in the
    /// pool.
    #[inline]
    pub unsafe fn recycle_locked(
        &self,
        queue: NonNull<OwnedWaitQueue>,
        token: &mut LockToken<'_, OwnedWaitQueuePoolLock>,
    ) {
        // SAFETY: The caller guarantees `queue` points to a valid `OwnedWaitQueue` with no
        // concurrent mutation of its owner or waiter fields.
        unsafe {
            queue.as_ref().assert_safe_for_pooling();
        }

        // Pushing a recycled queue onto the back of the list means that we are much less likely to
        // immediately hand the same queue back out in a `borrow_locked` call while another thread
        // still sees the pointer in its `LazyOwnedWaitQueue`. Mutex code checks that it still has
        // the same pointer in its `LazyOwnedWaitQueue` *after* acquiring the OWQ's lock which means
        // that a just-recycled OWQ's lock may be briefly acquired after being recycled before the
        // caller realizes that the OWQ pointer is stale. We don't want those accesses to contend
        // with legitimate attempts to lock the queue from other threads that just allocated the
        // queue for themselves from the front of the list.
        // SAFETY: `OwnedWaitQueuePoolLock` is the global lock protecting `self.free_list`, `self`
        // is pinned by its `PinInit` construction invariant, and the caller guarantees `queue` is
        // valid, pinned, and not currently in any list.
        unsafe {
            let mut guard = self.guard_lock_mut(token);
            guard.free_list_mut().push_back_raw(queue);
        }
    }

    /// Returns the value of the excess capacity OWQ kcounter.
    #[inline]
    pub fn get_excess_capacity_for_test(&self) -> i64 {
        OWQ_POOL_EXCESS_CAPACITY.sum_across_all_cpus()
    }
}

/// C++ FFI entry point for `OwnedWaitQueuePoolLock::Get()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/lazy_owned_wait_queue.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_owned_wait_queue_pool_lock_get() -> *mut KSpinlock<OwnedWaitQueuePoolLock> {
    core::ptr::from_ref(OwnedWaitQueuePoolLock::get()).cast_mut()
}

/// C++ FFI entry point for `OwnedWaitQueuePool::OwnedWaitQueuePool()`.
///
/// # Safety
///
/// `pool` must point to valid, aligned, uninitialized storage for an [`OwnedWaitQueuePool`], and
/// the initialized object must remain pinned in memory until dropped by
/// [`rust_owned_wait_queue_pool_destroy`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_pool_new(
    pool: *mut MaybeUninit<OwnedWaitQueuePool>,
) {
    debug_assert!(!pool.is_null());
    // SAFETY: Caller guarantees `pool` points to valid, aligned, uninitialized storage that
    // remains pinned after initialization.
    let slot = unsafe { &mut *pool };
    let _ = slot.write_pin_init(OwnedWaitQueuePool::new());
}

/// C++ FFI entry point for `OwnedWaitQueuePool::~OwnedWaitQueuePool()`.
///
/// # Safety
///
/// `pool` must point to a valid, initialized [`OwnedWaitQueuePool`] with exclusive access, an
/// empty `free_list`, and that will not be used again.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_pool_destroy(pool: *mut OwnedWaitQueuePool) {
    debug_assert!(!pool.is_null());
    // SAFETY: Caller guarantees `pool` points to a valid, initialized `OwnedWaitQueuePool`.
    unsafe {
        core::ptr::drop_in_place(pool);
    }
}

/// C++ FFI entry point for `OwnedWaitQueuePool::Get()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_owned_wait_queue_pool_get() -> *mut OwnedWaitQueuePool {
    core::ptr::from_ref(OwnedWaitQueuePool::get()).cast_mut()
}

/// C++ FFI entry point for `OwnedWaitQueuePool::Grow()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_owned_wait_queue_pool_grow(pool: &OwnedWaitQueuePool) -> zx_status_t {
    Status::result_into_raw(pool.grow())
}

/// C++ FFI entry point for `OwnedWaitQueuePool::Shrink()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_owned_wait_queue_pool_shrink(pool: &OwnedWaitQueuePool) {
    pool.shrink();
}

/// C++ FFI entry point for `OwnedWaitQueuePool::RecycleLocked()`.
///
/// # Safety
///
/// `queue` must point to a valid, pinned [`OwnedWaitQueue`] allocated from this pool's cache in a
/// clean state (no owner, no waiters, and no concurrent mutation of its owner or waiter fields)
/// that is not currently in any list, must remain valid and unmoved while in the pool, and the
/// caller must hold [`OwnedWaitQueuePoolLock`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_pool_recycle_locked(
    pool: &OwnedWaitQueuePool,
    queue: *mut OwnedWaitQueue,
) {
    debug_assert!(!queue.is_null(), "Cannot recycle a null queue.");
    // SAFETY: The caller guarantees `queue` is a valid, non-null pointer.
    let queue = unsafe { NonNull::new_unchecked(queue) };
    // SAFETY: The C++ declaration is `TA_REQ(OwnedWaitQueuePoolLock::Get())`.
    let mut token = unsafe { LockToken::new() };
    // SAFETY: Caller upholds the safety requirements of `recycle_locked`.
    unsafe {
        pool.recycle_locked(queue, &mut token);
    }
}

/// C++ FFI entry point for `OwnedWaitQueuePool::BorrowLocked()`.
///
/// # Safety
///
/// The caller must hold [`OwnedWaitQueuePoolLock`].
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_owned_wait_queue_pool_borrow_locked(
    pool: &OwnedWaitQueuePool,
) -> *mut OwnedWaitQueue {
    // SAFETY: The C++ declaration is `TA_REQ(OwnedWaitQueuePoolLock::Get())`.
    let mut token = unsafe { LockToken::new() };
    pool.borrow_locked(&mut token).as_ptr()
}

/// C++ FFI entry point for `OwnedWaitQueuePool::GetExcessCapacityForTest()`.
// SAFETY: Symbol name is unique across the kernel link unit and matches the C++ declaration in
// `zircon/kernel/include/kernel/owned_wait_queue_pool.h`.
#[unsafe(no_mangle)]
pub extern "C" fn rust_owned_wait_queue_pool_get_excess_capacity_for_test(
    pool: &OwnedWaitQueuePool,
) -> i64 {
    pool.get_excess_capacity_for_test()
}

/// Tests for [`OwnedWaitQueuePool`].
#[cfg(ktest)]
#[unittest::suite(name = "owned_wait_queue_pool_rust")]
mod tests {
    use super::{OwnedWaitQueue, OwnedWaitQueuePool, OwnedWaitQueuePoolLock, UniquePtr};
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, ThreadPtr};
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::c_void;
    use pin_init::stack_pin_init;
    use unittest::{assert_true, expect_eq, unwrap_ok};
    use zx_types::ZX_OK;

    /// Verifies `grow`, `borrow_locked`, FIFO `recycle_locked`, and reuse after `shrink`.
    #[test]
    fn grow_borrow_recycle_test() {
        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());
        unwrap_ok!(stack_pool.grow());
        unwrap_ok!(stack_pool.grow());

        let (q1, q2) = {
            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            let q1 = stack_pool.borrow_locked(guard.as_mut().token_mut());
            let q2 = stack_pool.borrow_locked(guard.as_mut().token_mut());
            assert_true!(q1 != q2);

            // Recycle `q1` then `q2`; `recycle_locked` pushes to the back and `borrow_locked`
            // pops from the front, so borrowing again must return `q1` followed by `q2`.
            // SAFETY: `q1` and `q2` were just borrowed from `stack_pool` and have no owner or
            // waiters.
            unsafe {
                stack_pool.recycle_locked(q1, guard.as_mut().token_mut());
                stack_pool.recycle_locked(q2, guard.as_mut().token_mut());
            }

            let b1 = stack_pool.borrow_locked(guard.as_mut().token_mut());
            let b2 = stack_pool.borrow_locked(guard.as_mut().token_mut());
            expect_eq!(q1.as_ptr(), b1.as_ptr());
            expect_eq!(q2.as_ptr(), b2.as_ptr());

            // SAFETY: `b1` and `b2` were just borrowed from `stack_pool` and have no owner or
            // waiters.
            unsafe {
                stack_pool.recycle_locked(b1, guard.as_mut().token_mut());
                stack_pool.recycle_locked(b2, guard.as_mut().token_mut());
            }
            (q1, q2)
        };

        // Shrinking and then growing reuses the existing allocated capacity without allocating
        // another queue.
        stack_pool.shrink();
        unwrap_ok!(stack_pool.grow());

        let (b1, b2) = {
            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            let b1 = stack_pool.borrow_locked(guard.as_mut().token_mut());
            let b2 = stack_pool.borrow_locked(guard.as_mut().token_mut());
            (b1, b2)
        };
        expect_eq!(q1.as_ptr(), b1.as_ptr());
        expect_eq!(q2.as_ptr(), b2.as_ptr());
        // SAFETY: `b1` and `b2` were originally allocated via `UniquePtr::into_raw` in
        // `stack_pool.grow()` and are no longer in `free_list`. Dropped outside the spinlock
        // because `ObjectCache` deallocation acquires a mutex.
        unsafe {
            drop(UniquePtr::<OwnedWaitQueue>::from_raw(b1.as_ptr()));
            drop(UniquePtr::<OwnedWaitQueue>::from_raw(b2.as_ptr()));
        }
    }

    /// Verifies excess capacity kcounter updates across `grow` and `shrink`.
    #[test]
    fn pool_counters_test() {
        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());

        let initial = stack_pool.get_excess_capacity_for_test();

        unwrap_ok!(stack_pool.grow());
        let after_grow = stack_pool.get_excess_capacity_for_test();
        expect_eq!(initial, after_grow);

        stack_pool.shrink();
        let after_shrink = stack_pool.get_excess_capacity_for_test();
        expect_eq!(after_grow + 1, after_shrink);

        // Growing again reuses the excess capacity and restores the counter to `initial`.
        unwrap_ok!(stack_pool.grow());
        expect_eq!(initial, stack_pool.get_excess_capacity_for_test());

        // Drain the pool so `DoublyLinkedList` is empty on drop and free the allocated queue.
        let queue = {
            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            stack_pool.borrow_locked(guard.as_mut().token_mut())
        };
        // SAFETY: `queue` was allocated via `UniquePtr::into_raw` in `stack_pool.grow()` and is
        // no longer in `free_list`.
        unsafe {
            drop(UniquePtr::<OwnedWaitQueue>::from_raw(queue.as_ptr()));
        }
    }

    /// Stress-tests concurrent `borrow_locked` and `recycle_locked` operations across threads.
    #[test]
    fn pool_stress_test() {
        const NUM_THREADS: usize = 16;
        const ITERATIONS: usize = 5000;

        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());
        for _ in 0..NUM_THREADS {
            unwrap_ok!(stack_pool.grow());
        }

        extern "C" fn worker(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `stack_pool`, which outlives all worker threads.
            let pool = unsafe { &*arg.cast::<OwnedWaitQueuePool>() };
            for _ in 0..ITERATIONS {
                let queue = {
                    ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                    pool.borrow_locked(guard.as_mut().token_mut())
                };
                {
                    ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                    // SAFETY: `queue` was borrowed above by this thread and remains in a clean
                    // state with no owner and no waiters.
                    unsafe {
                        pool.recycle_locked(queue, guard.as_mut().token_mut());
                    }
                }
            }
            0
        }

        let mut threads: [Option<ThreadPtr>; NUM_THREADS] = [None; NUM_THREADS];
        for slot in threads.iter_mut() {
            // SAFETY: `worker` only accesses `stack_pool`, and every spawned thread is joined
            // below before `stack_pool` is dropped.
            let t = unwrap_ok!(unsafe {
                thread::create_with_priority(
                    c"pool_rust_stress_worker".as_ptr(),
                    worker,
                    core::ptr::from_ref(&*stack_pool).cast_mut().cast(),
                    DEFAULT_PRIORITY,
                )
            });
            // SAFETY: `t` was just created and has not been resumed or joined yet.
            unsafe { t.resume() };
            *slot = Some(t);
        }

        for t in threads.into_iter().flatten() {
            // SAFETY: `t` has not been joined yet.
            let ret = unwrap_ok!(unsafe { t.join(InstantMono::INFINITE) });
            expect_eq!(ZX_OK, ret);
        }

        for _ in 0..NUM_THREADS {
            let queue = {
                ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                stack_pool.borrow_locked(guard.as_mut().token_mut())
            };
            // SAFETY: `queue` was allocated via `UniquePtr::into_raw` in `stack_pool.grow()` and
            // is no longer in `free_list`.
            unsafe {
                drop(UniquePtr::<OwnedWaitQueue>::from_raw(queue.as_ptr()));
            }
        }
    }
}
