// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

// Use these crates so that we don't need to make the dependencies conditional.
use fuchsia_sync as _;
use lock_api as _;

use crate::{
    LockAfter, LockBefore, LockDepMutex, LockDepRwLock, LockFor, Locked, RwLockFor,
    UninterruptibleLock,
};

use lock_api::RawMutex;
use std::{any, fmt};

pub use fuchsia_sync::{
    MappedMutexGuard, Mutex, MutexGuard, RwLock, RwLockReadGuard, RwLockWriteGuard,
};

/// A trait for lock guards that can be temporarily unlocked asynchronously.
/// This is useful for performing async operations while holding a lock, without
/// causing deadlocks or holding the lock for an extended period.
#[async_trait::async_trait(?Send)]
pub trait AsyncUnlockable {
    /// Temporarily unlocks the guard `s`, executes the async function `f`, and then
    /// re-locks the guard.
    /// The lock is guaranteed to be re-acquired before this function returns.
    async fn unlocked_async<F, U>(s: &mut Self, f: F) -> U
    where
        F: AsyncFnOnce() -> U;
}

#[async_trait::async_trait(?Send)]
impl<'a, T> crate::AsyncUnlockable for MutexGuard<'a, T> {
    async fn unlocked_async<F, U>(s: &mut Self, f: F) -> U
    where
        F: AsyncFnOnce() -> U,
    {
        // SAFETY: The guard always have a lock mutex.
        unsafe {
            Self::mutex(s).raw().unlock();
        }
        scopeguard::defer!(
            // SAFETY: The mutex has been unlocked previously.
            unsafe { Self::mutex(s).raw().lock() }
        );
        f().await
    }
}

/// A generic mutex for the ordered_lock operations.
pub trait MutexLike {
    type Guard<'a>
    where
        Self: 'a;
    type Context;

    fn context() -> Self::Context;

    /// Lock the mutex. `level` is the index of the locked mutex in the lock ordering.
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_>;

    #[inline(always)]
    fn key(&self) -> *const ()
    where
        Self: Sized,
    {
        self as *const Self as *const ()
    }
}

impl<T> MutexLike for Mutex<T> {
    type Guard<'a>
        = MutexGuard<'a, T>
    where
        T: 'a;
    type Context = ();

    #[inline(always)]
    fn context() -> Self::Context {
        ()
    }

    #[inline(always)]
    fn lock(&self, _context: &mut Self::Context) -> Self::Guard<'_> {
        return self.lock();
    }
}

/// A generic rwlock for the ordered_lock operations.
pub trait RwLockLike {
    type ReadGuard<'a>
    where
        Self: 'a;
    type WriteGuard<'a>
    where
        Self: 'a;
    type Context;

    fn context() -> Self::Context;

    fn read(&self, context: &mut Self::Context) -> Self::ReadGuard<'_>;
    fn write(&self, context: &mut Self::Context) -> Self::WriteGuard<'_>;

    #[inline(always)]
    fn key(&self) -> *const ()
    where
        Self: Sized,
    {
        self as *const Self as *const ()
    }
}

impl<T> RwLockLike for RwLock<T> {
    type ReadGuard<'a>
        = RwLockReadGuard<'a, T>
    where
        T: 'a;
    type WriteGuard<'a>
        = RwLockWriteGuard<'a, T>
    where
        T: 'a;
    type Context = ();

    #[inline(always)]
    fn context() -> Self::Context {
        ()
    }

    #[inline(always)]
    fn read(&self, _context: &mut Self::Context) -> Self::ReadGuard<'_> {
        self.read()
    }

    #[inline(always)]
    fn write(&self, _context: &mut Self::Context) -> Self::WriteGuard<'_> {
        self.write()
    }
}

/// Lock `m1` and `m2` in a consistent order (using the memory address of m1 and m2 and returns the
/// associated guard. This ensure that `ordered_lock(m1, m2)` and `ordered_lock(m2, m1)` will not
/// deadlock.
pub fn ordered_lock<'a, M: MutexLike>(m1: &'a M, m2: &'a M) -> (M::Guard<'a>, M::Guard<'a>) {
    let mut context = M::context();
    if m1.key() < m2.key() {
        let g1 = m1.lock(&mut context);
        let g2 = m2.lock(&mut context);
        (g1, g2)
    } else {
        let g2 = m2.lock(&mut context);
        let g1 = m1.lock(&mut context);
        (g1, g2)
    }
}

/// Acquires multiple mutexes in a consistent order based on their memory addresses.
/// This helps prevent deadlocks.
pub fn ordered_lock_vec<'a, M: MutexLike>(mutexes: &[&'a M]) -> Vec<M::Guard<'a>> {
    let mut context = M::context();

    // Create a vector of tuples containing the mutex and its original index.
    let mut indexed_mutexes = mutexes.iter().enumerate().map(|(i, m)| (i, *m)).collect::<Vec<_>>();

    // Sort the indexed mutexes by their keys.
    indexed_mutexes.sort_by_key(|(_, m)| m.key());

    // Acquire the locks in the sorted order.
    let mut guards =
        indexed_mutexes.into_iter().map(|(i, m)| (i, m.lock(&mut context))).collect::<Vec<_>>();

    // Reorder the guards to match the original order of the mutexes.
    guards.sort_by_key(|(i, _)| *i);

    guards.into_iter().map(|(_, g)| g).collect::<Vec<_>>()
}

/// Lock `r1` and `r2` in a consistent order (using the memory address of r1 and r2) for reading.
pub fn ordered_read_lock<'a, R: RwLockLike>(
    r1: &'a R,
    r2: &'a R,
) -> (R::ReadGuard<'a>, R::ReadGuard<'a>) {
    let w1 = RwLockReadWrapper(r1);
    let w2 = RwLockReadWrapper(r2);
    ordered_lock(&w1, &w2)
}

/// Lock `r1` and `r2` in a consistent order (using the memory address of r1 and r2) for writing.
pub fn ordered_write_lock<'a, R: RwLockLike>(
    r1: &'a R,
    r2: &'a R,
) -> (R::WriteGuard<'a>, R::WriteGuard<'a>) {
    let w1 = RwLockWriteWrapper(r1);
    let w2 = RwLockWriteWrapper(r2);
    ordered_lock(&w1, &w2)
}

/// Acquires multiple rwlocks in a consistent order based on their memory addresses for reading.
pub fn ordered_read_lock_vec<'a, R: RwLockLike>(rwlocks: &[&'a R]) -> Vec<R::ReadGuard<'a>> {
    let wrappers = rwlocks.iter().map(|r| RwLockReadWrapper(*r)).collect::<Vec<_>>();
    let wrapper_refs = wrappers.iter().collect::<Vec<_>>();
    ordered_lock_vec(&wrapper_refs)
}

/// Acquires multiple rwlocks in a consistent order based on their memory addresses for writing.
pub fn ordered_write_lock_vec<'a, R: RwLockLike>(rwlocks: &[&'a R]) -> Vec<R::WriteGuard<'a>> {
    let wrappers = rwlocks.iter().map(|r| RwLockWriteWrapper(*r)).collect::<Vec<_>>();
    let wrapper_refs = wrappers.iter().collect::<Vec<_>>();
    ordered_lock_vec(&wrapper_refs)
}

/// A wrapper for mutex that requires a `Locked` context to acquire.
/// This context must be of a level that precedes `L` in the lock ordering graph
/// where `L` is a level associated with this mutex.
pub struct OrderedMutex<T, L: LockAfter<UninterruptibleLock> + crate::LockLevel> {
    mutex: LockDepMutex<T, L>,
}

impl<T: Default, L: LockAfter<UninterruptibleLock> + crate::LockLevel> Default
    for OrderedMutex<T, L>
{
    fn default() -> Self {
        Self { mutex: T::default().into() }
    }
}

impl<T: fmt::Debug, L: LockAfter<UninterruptibleLock> + crate::LockLevel> fmt::Debug
    for OrderedMutex<T, L>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OrderedMutex({:?}, {})", self.mutex, any::type_name::<L>())
    }
}

impl<T, L: LockAfter<UninterruptibleLock> + crate::LockLevel> LockFor<L> for OrderedMutex<T, L> {
    type Data = T;
    type Guard<'a>
        = crate::LockDepGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    fn lock(&self) -> Self::Guard<'_> {
        self.mutex.lock()
    }
}

impl<T, L: LockAfter<UninterruptibleLock> + crate::LockLevel> OrderedMutex<T, L> {
    pub const fn new(t: T) -> Self {
        Self { mutex: LockDepMutex::new(t) }
    }

    pub fn lock<'a, P>(&'a self, locked: &'a mut Locked<P>) -> <Self as LockFor<L>>::Guard<'a>
    where
        P: LockBefore<L>,
    {
        locked.lock(self)
    }

    pub fn lock_and<'a, P>(
        &'a self,
        locked: &'a mut Locked<P>,
    ) -> (<Self as LockFor<L>>::Guard<'a>, &'a mut Locked<L>)
    where
        P: LockBefore<L>,
    {
        locked.lock_and(self)
    }
}

/// Lock two OrderedMutex of the same level in the consistent order. Returns both
/// guards and a new locked context.
pub fn lock_both<'a, T, L: LockAfter<UninterruptibleLock> + crate::LockLevel, P>(
    locked: &'a mut Locked<P>,
    m1: &'a OrderedMutex<T, L>,
    m2: &'a OrderedMutex<T, L>,
) -> (crate::LockDepGuard<'a, T>, crate::LockDepGuard<'a, T>, &'a mut Locked<L>)
where
    P: LockBefore<L>,
{
    locked.lock_both_and(m1, m2)
}

/// A wrapper for an RwLock that requires a `Locked` context to acquire.
/// This context must be of a level that precedes `L` in the lock ordering graph
/// where `L` is a level associated with this RwLock.
pub struct OrderedRwLock<T, L: LockAfter<UninterruptibleLock> + crate::LockLevel> {
    rwlock: LockDepRwLock<T, L>,
}

impl<T: Default, L: LockAfter<UninterruptibleLock> + crate::LockLevel> Default
    for OrderedRwLock<T, L>
{
    fn default() -> Self {
        Self { rwlock: T::default().into() }
    }
}

impl<T: fmt::Debug, L: LockAfter<UninterruptibleLock> + crate::LockLevel> fmt::Debug
    for OrderedRwLock<T, L>
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "OrderedRwLock({:?}, {})", self.rwlock, any::type_name::<L>())
    }
}

impl<T, L: LockAfter<UninterruptibleLock> + crate::LockLevel> RwLockFor<L> for OrderedRwLock<T, L> {
    type Data = T;
    type ReadGuard<'a>
        = crate::LockDepReadGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    type WriteGuard<'a>
        = crate::LockDepWriteGuard<'a, T>
    where
        T: 'a,
        L: 'a;
    fn read_lock(&self) -> Self::ReadGuard<'_> {
        self.rwlock.read()
    }
    fn write_lock(&self) -> Self::WriteGuard<'_> {
        self.rwlock.write()
    }
}

impl<T, L: LockAfter<UninterruptibleLock> + crate::LockLevel> OrderedRwLock<T, L> {
    pub const fn new(t: T) -> Self {
        Self { rwlock: LockDepRwLock::new(t) }
    }

    pub fn read<'a, P>(&'a self, locked: &'a mut Locked<P>) -> <Self as RwLockFor<L>>::ReadGuard<'a>
    where
        P: LockBefore<L>,
    {
        locked.read_lock(self)
    }

    pub fn write<'a, P>(
        &'a self,
        locked: &'a mut Locked<P>,
    ) -> <Self as RwLockFor<L>>::WriteGuard<'a>
    where
        P: LockBefore<L>,
    {
        locked.write_lock(self)
    }

    pub fn read_and<'a, P>(
        &'a self,
        locked: &'a mut Locked<P>,
    ) -> (<Self as RwLockFor<L>>::ReadGuard<'a>, &'a mut Locked<L>)
    where
        P: LockBefore<L>,
    {
        locked.read_lock_and(self)
    }

    pub fn write_and<'a, P>(
        &'a self,
        locked: &'a mut Locked<P>,
    ) -> (<Self as RwLockFor<L>>::WriteGuard<'a>, &'a mut Locked<L>)
    where
        P: LockBefore<L>,
    {
        locked.write_lock_and(self)
    }
}

struct RwLockReadWrapper<'a, R>(&'a R);
struct RwLockWriteWrapper<'a, R>(&'a R);

impl<'a, R: RwLockLike> MutexLike for RwLockReadWrapper<'a, R> {
    type Guard<'b>
        = R::ReadGuard<'a>
    where
        Self: 'b;
    type Context = R::Context;

    #[inline(always)]
    fn context() -> Self::Context {
        R::context()
    }

    #[inline(always)]
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_> {
        self.0.read(context)
    }

    #[inline(always)]
    fn key(&self) -> *const () {
        self.0.key()
    }
}

impl<'a, R: RwLockLike> MutexLike for RwLockWriteWrapper<'a, R> {
    type Guard<'b>
        = R::WriteGuard<'a>
    where
        Self: 'b;
    type Context = R::Context;

    #[inline(always)]
    fn context() -> Self::Context {
        R::context()
    }

    #[inline(always)]
    fn lock(&self, context: &mut Self::Context) -> Self::Guard<'_> {
        self.0.write(context)
    }

    #[inline(always)]
    fn key(&self) -> *const () {
        self.0.key()
    }
}
#[cfg(test)]
mod test {
    use super::*;
    use crate::Unlocked;

    #[::fuchsia::test]
    fn test_lock_ordering() {
        let l1 = Mutex::new(1);
        let l2 = Mutex::new(2);

        {
            let (g1, g2) = ordered_lock(&l1, &l2);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
        {
            let (g2, g1) = ordered_lock(&l2, &l1);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
    }

    #[::fuchsia::test]
    fn test_vec_lock_ordering() {
        let l1 = Mutex::new(1);
        let l0 = Mutex::new(0);
        let l2 = Mutex::new(2);

        {
            let guards = ordered_lock_vec(&[&l0, &l1, &l2]);
            assert_eq!(*guards[0], 0);
            assert_eq!(*guards[1], 1);
            assert_eq!(*guards[2], 2);
        }
        {
            let guards = ordered_lock_vec(&[&l2, &l1, &l0]);
            assert_eq!(*guards[0], 2);
            assert_eq!(*guards[1], 1);
            assert_eq!(*guards[2], 0);
        }
    }

    mod lock_levels {
        //! Lock ordering tree:
        //! Unlocked -> A -> B -> C
        //!          -> D -> E -> F
        use crate::{LockAfter, UninterruptibleLock, Unlocked};
        use lock_ordering_macro::lock_ordering;
        lock_ordering! {
            Unlocked => A,
            A => B,
            B => C,
            Unlocked => D,
            D => E,
            E => F,
        }

        impl LockAfter<UninterruptibleLock> for A {}
        impl LockAfter<UninterruptibleLock> for B {}
        impl LockAfter<UninterruptibleLock> for C {}
        impl LockAfter<UninterruptibleLock> for D {}
        impl LockAfter<UninterruptibleLock> for E {}
        impl LockAfter<UninterruptibleLock> for F {}
    }

    use lock_levels::{A, B, C, D, E, F};

    #[test]
    fn test_ordered_mutex() {
        let a: OrderedMutex<u8, A> = OrderedMutex::new(15);
        let _b: OrderedMutex<u16, B> = OrderedMutex::new(30);
        let c: OrderedMutex<u32, C> = OrderedMutex::new(45);

        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        let locked = unsafe { Unlocked::new() };

        let (a_data, mut next_locked) = a.lock_and(locked);
        let c_data = c.lock(&mut next_locked);

        // This won't compile
        //let _b_data = _b.lock(locked);
        //let _b_data = _b.lock(&mut next_locked);

        assert_eq!(&*a_data, &15);
        assert_eq!(&*c_data, &45);
    }
    #[test]
    fn test_ordered_rwlock() {
        let d: OrderedRwLock<u8, D> = OrderedRwLock::new(15);
        let _e: OrderedRwLock<u16, E> = OrderedRwLock::new(30);
        let f: OrderedRwLock<u32, F> = OrderedRwLock::new(45);

        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        let locked = unsafe { Unlocked::new() };
        {
            let (d_data, mut next_locked) = d.write_and(locked);
            let f_data = f.read(&mut next_locked);

            // This won't compile
            //let _e_data = _e.read(locked);
            //let _e_data = _e.read(&mut next_locked);

            assert_eq!(&*d_data, &15);
            assert_eq!(&*f_data, &45);
        }
        {
            let (d_data, mut next_locked) = d.read_and(locked);
            let f_data = f.write(&mut next_locked);

            // This won't compile
            //let _e_data = _e.write(locked);
            //let _e_data = _e.write(&mut next_locked);

            assert_eq!(&*d_data, &15);
            assert_eq!(&*f_data, &45);
        }
    }

    #[test]
    fn test_lock_both() {
        let a1: OrderedMutex<u8, A> = OrderedMutex::new(15);
        let a2: OrderedMutex<u8, A> = OrderedMutex::new(30);
        #[allow(
            clippy::undocumented_unsafe_blocks,
            reason = "Force documented unsafe blocks in Starnix"
        )]
        let locked = unsafe { Unlocked::new() };
        {
            let (a1_data, a2_data, _) = lock_both(locked, &a1, &a2);
            assert_eq!(&*a1_data, &15);
            assert_eq!(&*a2_data, &30);
        }
        {
            let (a2_data, a1_data, _) = lock_both(locked, &a2, &a1);
            assert_eq!(&*a1_data, &15);
            assert_eq!(&*a2_data, &30);
        }
    }

    #[::fuchsia::test]
    fn test_ordered_rwlock_wrappers() {
        let l1: LockDepRwLock<u8, A> = 1.into();
        let l2: LockDepRwLock<u8, A> = 2.into();

        {
            let (g1, g2) = ordered_read_lock(&l1, &l2);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
        {
            let (g2, g1) = ordered_read_lock(&l2, &l1);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
        {
            let (g1, g2) = ordered_write_lock(&l1, &l2);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
        {
            let (g2, g1) = ordered_write_lock(&l2, &l1);
            assert_eq!(*g1, 1);
            assert_eq!(*g2, 2);
        }
    }

    #[::fuchsia::test]
    fn test_ordered_rwlock_vec() {
        let l1: LockDepRwLock<u8, A> = 1.into();
        let l0: LockDepRwLock<u8, A> = 0.into();
        let l2: LockDepRwLock<u8, A> = 2.into();

        {
            let guards = ordered_read_lock_vec(&[&l0, &l1, &l2]);
            assert_eq!(*guards[0], 0);
            assert_eq!(*guards[1], 1);
            assert_eq!(*guards[2], 2);
        }
        {
            let guards = ordered_write_lock_vec(&[&l2, &l1, &l0]);
            assert_eq!(*guards[0], 2);
            assert_eq!(*guards[1], 1);
            assert_eq!(*guards[2], 0);
        }
    }
}
