// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;
use core::pin::Pin;
use core::ptr::addr_of_mut;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};

use crate::{
    ExclusiveLockPolicy, LockPolicy, LockdepTracked, RawBrwLockPi, RawBrwLockPiReadPolicy,
    RawBrwLockPiStorage, RawLock,
};
use lockdep::LockClass;

type LockdepTrackedBrwLockReadPolicy = LockdepTracked<RawBrwLockPiStorage, RawBrwLockPiReadPolicy>;
type LockdepTrackedBrwLockWritePolicy = <RawBrwLockPi as RawLock>::DefaultPolicy;

/// A priority-inheriting reader-writer lock.
#[repr(transparent)]
#[pin_data]
pub struct BrwLockPi<Class: LockClass> {
    #[pin]
    lock: RawBrwLockPi,
    _marker: PhantomData<Class>,
}

// SAFETY: BrwLockPi is safe to share across threads because the underlying RawBrwLockPi is Sync.
unsafe impl<Class: LockClass> Sync for BrwLockPi<Class> {}
// SAFETY: BrwLockPi is safe to send across threads because the underlying RawBrwLockPi is Send.
unsafe impl<Class: LockClass> Send for BrwLockPi<Class> {}

impl<Class: LockClass> BrwLockPi<Class> {
    /// Safe dynamic initialization inside pin context.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            // SAFETY: `Class::ID` is either null or a valid, static `LockClassId`.
            lock <- unsafe { <RawBrwLockPi as RawLock>::init(Class::ID) },
            _marker: PhantomData,
        })
    }

    /// Acquire the read lock and return a stack-pinned validation guard.
    #[inline]
    pub fn read_lock(
        &self,
    ) -> impl PinInit<BrwLockPiReadGuard<'_, Class>, core::convert::Infallible> {
        BrwLockPiReadGuard::new(self)
    }

    /// Acquire the write lock and return a stack-pinned validation guard.
    #[inline]
    pub fn write_lock(
        &self,
    ) -> impl PinInit<BrwLockPiWriteGuard<'_, Class>, core::convert::Infallible> {
        BrwLockPiWriteGuard::new(self)
    }

    /// Returns a reference to the underlying raw lock.
    #[inline]
    pub fn raw_lock(&self) -> &RawBrwLockPi {
        &self.lock
    }
}

/// A validation guard representing reader lock ownership and active list participation.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct BrwLockPiReadGuard<'a, Class: LockClass> {
    lock: &'a BrwLockPi<Class>,
    #[pin]
    state: <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::GuardState,
    token: crate::LockToken<'a, Class>,
}

impl<'a, Class: LockClass> BrwLockPiReadGuard<'a, Class> {
    /// Creates a new stack-pinned validation guard initialization block.
    pub fn new(lock: &'a BrwLockPi<Class>) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: The closure correctly initializes all fields of the allocated
        // `BrwLockPiReadGuard` and satisfies all safety requirements of `pin_init_from_closure`.
        unsafe {
            pin_init::pin_init_from_closure(
                move |this: *mut Self| -> Result<(), core::convert::Infallible> {
                    let lock_addr = addr_of_mut!((*this).lock);
                    core::ptr::write(lock_addr, lock);

                    let state_addr = addr_of_mut!((*this).state);
                    core::ptr::write(state_addr, Default::default());

                    <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::acquire(
                        &lock.lock,
                        &mut *state_addr,
                        (),
                    );

                    let token_addr = addr_of_mut!((*this).token);
                    core::ptr::write(token_addr, crate::LockToken::new());

                    Ok(())
                },
            )
        }
    }

    /// Returns a shared reference to the lock proof `LockToken`.
    #[inline]
    pub fn token(&self) -> &crate::LockToken<'a, Class> {
        &self.token
    }

    /// Returns true if this guard holds the given `target` lock instance.
    #[inline]
    pub fn is_locked_by(&self, target: &BrwLockPi<Class>) -> bool {
        core::ptr::eq(self.lock, target)
    }

    /// Temporarily releases the read lock before executing the given callable `f` and then
    /// re-acquires the read lock.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        // SAFETY: `state` is pinned on the stack and valid.
        unsafe {
            let me = self.get_unchecked_mut();
            <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::release(
                &me.lock.lock,
                &mut me.state,
            );
            let result = f();
            <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::reacquire(
                &me.lock.lock,
                &mut me.state,
            );
            result
        }
    }
}

#[pinned_drop]
impl<'a, Class: LockClass> PinnedDrop for BrwLockPiReadGuard<'a, Class> {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `get_unchecked_mut` is safe because we do not move the fields out of Pin.
        // `release` is safe because the read lock was acquired when creating this guard,
        // and we are releasing it with the same pinned state.
        unsafe {
            let me = self.get_unchecked_mut();
            <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::release(
                &me.lock.lock,
                &mut me.state,
            );
        }
    }
}

/// A validation guard representing writer lock ownership and active list participation.
#[repr(C)]
pub struct BrwLockPiWriteGuard<
    'a,
    Class: LockClass,
    P: ExclusiveLockPolicy<RawBrwLockPi> = LockdepTrackedBrwLockWritePolicy,
> {
    lock: &'a BrwLockPi<Class>,
    state: P::GuardState,
    token: crate::LockToken<'a, Class>,
}

impl<'a, Class: LockClass, P: ExclusiveLockPolicy<RawBrwLockPi>> BrwLockPiWriteGuard<'a, Class, P> {
    /// Creates a new stack-pinned validation guard initialization block.
    pub fn new(lock: &'a BrwLockPi<Class>) -> impl PinInit<Self, core::convert::Infallible>
    where
        P: ExclusiveLockPolicy<RawBrwLockPi, AcquireArgs = ()>,
    {
        // SAFETY: The closure correctly initializes all fields of the allocated
        // `BrwLockPiWriteGuard` and satisfies all safety requirements of `pin_init_from_closure`.
        unsafe {
            pin_init::pin_init_from_closure(
                move |this: *mut Self| -> Result<(), core::convert::Infallible> {
                    let lock_addr = addr_of_mut!((*this).lock);
                    core::ptr::write(lock_addr, lock);

                    let state_addr = addr_of_mut!((*this).state);
                    core::ptr::write(state_addr, Default::default());

                    P::acquire(&lock.lock, &mut *state_addr, ());

                    let token_addr = addr_of_mut!((*this).token);
                    core::ptr::write(token_addr, crate::LockToken::new());

                    Ok(())
                },
            )
        }
    }

    /// Returns a shared reference to the lock proof `LockToken`.
    #[inline]
    pub fn token(&self) -> &crate::LockToken<'a, Class> {
        &self.token
    }

    /// Returns a mutable reference to the lock proof `LockToken` inside this pinned projection.
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut crate::LockToken<'a, Class> {
        // SAFETY: We are accessing `token` mutably but `LockToken` is a ZST and does not require
        // pinning invariants to be maintained.
        let me = unsafe { self.get_unchecked_mut() };
        &mut me.token
    }

    /// Returns true if this guard holds the given `target` lock instance.
    #[inline]
    pub fn is_locked_by(&self, target: &BrwLockPi<Class>) -> bool {
        core::ptr::eq(self.lock, target)
    }

    /// Temporarily releases the write lock before executing the given callable `f` and then
    /// re-acquires the write lock.
    #[inline]
    pub fn call_unlocked<R, F: FnOnce() -> R>(self: Pin<&mut Self>, f: F) -> R {
        // SAFETY: `state` is pinned on the stack and valid.
        unsafe {
            let me = self.get_unchecked_mut();
            P::release(&me.lock.lock, &mut me.state);
            let result = f();
            P::reacquire(&me.lock.lock, &mut me.state);
            result
        }
    }
}

impl<'a, Class: LockClass, P: ExclusiveLockPolicy<RawBrwLockPi>> Drop
    for BrwLockPiWriteGuard<'a, Class, P>
{
    #[inline]
    fn drop(&mut self) {
        // SAFETY: A guard is only created pinned, by `new`, and `self` is not moved after it is
        // dropped. `release` is safe because the write lock was acquired when creating this guard,
        // and we are releasing it with the same pinned state.
        unsafe {
            P::release(&self.lock.lock, &mut self.state);
        }
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::{LockdepTrackedBrwLockReadPolicy, LockdepTrackedBrwLockWritePolicy};
    use crate::{BrwLockPi, LockPolicy, RawBrwLockPi, RawLock, guarded};
    use pin_init::{pin_init, stack_pin_init};
    use std::string::String;
    use std::vec::Vec;

    #[guarded]
    struct Database {
        #[brwlock]
        lock: BrwLockPi,

        #[guarded_by(lock)]
        data: Vec<String>,

        #[guarded_by(lock)]
        query_count: u64,
    }

    fn read_data(db: &Database) -> usize {
        lock!(let guard = db.read_lock());
        let len = guard.data().len();

        let fields = guard.fields();
        let _ = fields.data.len();
        let _ = *fields.query_count;

        len
    }

    fn append_data(db: &Database, value: String) {
        lock!(let mut guard = db.write_lock());
        let fields = guard.as_mut().fields_mut();
        fields.data.push(value);
        *fields.query_count += 1;
    }

    #[test]
    fn test_brwlock_projections() {
        stack_pin_init!(let db = pin_init!(Database {
            lock <- BrwLockPi::init(),
            data: Vec::new().into(),
            query_count: 0.into(),
        }));

        assert_eq!(read_data(&db), 0);
        append_data(&db, String::from("hello"));
        assert_eq!(read_data(&db), 1);

        lock!(let guard = db.read_lock());
        assert_eq!(guard.data()[0], "hello");
        assert_eq!(*guard.query_count(), 1);
    }

    #[pin_init::pin_data]
    struct BrwLockTest {
        #[pin]
        lock: RawBrwLockPi,
        state: std::sync::atomic::AtomicU32,
        kill: std::sync::atomic::AtomicBool,
    }

    fn run_test(readers: usize, writers: usize) {
        use std::sync::atomic::Ordering;

        stack_pin_init!(let test = pin_init!(BrwLockTest {
            // SAFETY: Null class ID is valid for an untracked test lock.
            lock <- unsafe { <RawBrwLockPi as RawLock>::init(core::ptr::null()) },
            state: std::sync::atomic::AtomicU32::new(0),
            kill: std::sync::atomic::AtomicBool::new(false),
        }));

        std::thread::scope(|s| {
            let mut threads = std::vec::Vec::new();

            for _ in 0..readers {
                threads.push(s.spawn(|| {
                    let mut state =
                        <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::GuardState::default();
                    while !test.kill.load(Ordering::Relaxed) {
                        // SAFETY: lock is initialized and pinned.
                        unsafe {
                            <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::acquire(
                                &test.lock,
                                &mut state,
                                (),
                            );
                        }
                        let _ = test.state.fetch_add(1, Ordering::Relaxed);
                        std::thread::yield_now();
                        let _ = test.state.fetch_sub(1, Ordering::Relaxed);
                        // SAFETY: lock is held in read mode.
                        unsafe {
                            <LockdepTrackedBrwLockReadPolicy as LockPolicy<RawBrwLockPi>>::release(
                                &test.lock,
                                &mut state,
                            );
                        }
                        std::thread::yield_now();
                    }
                }));
            }

            for _ in 0..writers {
                threads.push(s.spawn(|| {
                    let mut state = <LockdepTrackedBrwLockWritePolicy as LockPolicy<
                        RawBrwLockPi,
                    >>::GuardState::default();
                    while !test.kill.load(Ordering::Relaxed) {
                        // SAFETY: lock is initialized and pinned.
                        unsafe {
                            <LockdepTrackedBrwLockWritePolicy as LockPolicy<RawBrwLockPi>>::acquire(
                                &test.lock,
                                &mut state,
                                (),
                            );
                        }
                        let _ = test.state.fetch_add(0x10000, Ordering::Relaxed);
                        std::thread::yield_now();
                        let _ = test.state.fetch_sub(0x10000, Ordering::Relaxed);
                        // SAFETY: lock is held in write mode.
                        unsafe {
                            <LockdepTrackedBrwLockWritePolicy as LockPolicy<RawBrwLockPi>>::release(
                                &test.lock, &mut state,
                            );
                        }
                        std::thread::yield_now();
                    }
                }));
            }

            let start = std::time::Instant::now();
            while start.elapsed() < std::time::Duration::from_millis(300) {
                let local_state = test.state.load(Ordering::Relaxed);
                let num_readers = (local_state & 0xffff) as usize;
                let num_writers = (local_state >> 16) as usize;

                assert!(num_readers <= readers, "Too many readers: {}", num_readers);
                assert!(num_writers <= 1, "Too many writers: {}", num_writers);
                assert!(
                    num_readers == 0 || num_writers == 0,
                    "Both readers ({}) and writers ({}) active!",
                    num_readers,
                    num_writers
                );

                std::thread::yield_now();
            }

            test.kill.store(true, Ordering::SeqCst);
        });
    }

    #[test]
    fn test_parallel_readers() {
        run_test(8, 0);
    }

    #[test]
    fn test_single_writer() {
        run_test(0, 4);
    }

    #[test]
    fn test_readers_and_writers() {
        run_test(4, 2);
    }

    #[allow(dead_code)]
    struct CustomBrwLockClass;
    impl lockdep::LockClass for CustomBrwLockClass {
        const ID: *mut core::ffi::c_void = core::ptr::null_mut();
    }

    #[guarded]
    struct CustomBrwLockStruct {
        #[brwlock(CustomBrwLockClass)]
        lock: BrwLockPi<CustomBrwLockClass>,
        #[guarded_by(lock)]
        value: u32,
    }

    #[test]
    fn test_custom_brwlock_class() {
        stack_pin_init!(let s = pin_init!(CustomBrwLockStruct {
            lock <- BrwLockPi::init(),
            value: 42.into(),
        }));

        lock!(let guard = s.read_lock());
        assert_eq!(*guard.value(), 42);
    }

    #[guarded]
    struct MultiLockBrwStruct {
        #[brwlock]
        rw: BrwLockPi,
        #[mutex]
        mu: crate::KMutex,
        #[guarded_by(rw)]
        only_rw: u32,
        #[guarded_by(rw, mu)]
        shared: u32,
        #[pin]
        #[guarded_by(rw, mu)]
        pinned_shared: u32,
        #[guarded_by(rw, mu)]
        once_shared: crate::KOnceCell<u32>,
    }

    #[test]
    fn test_multi_lock_brwlock_and_mutex() {
        stack_pin_init!(let s = pin_init!(MultiLockBrwStruct {
            rw <- BrwLockPi::init(),
            mu <- crate::KMutex::init(),
            only_rw: 1.into(),
            shared: 10.into(),
            pinned_shared: 100.into(),
            once_shared: crate::KOnceCell::new(),
        }));

        {
            lock!(let rg = s.read_rw());
            assert_eq!(*rg.only_rw(), 1);
            assert_eq!(*rg.shared(), 10);
            assert_eq!(*rg.pinned_shared(), 100);
            assert_eq!(rg.once_shared(), None);
            assert_eq!(*rg.fields().shared, 10);
            assert_eq!(*rg.as_token_guard().shared(), 10);
        }
        {
            lock!(let mg = s.lock_mu());
            assert_eq!(*mg.shared(), 10);
            assert_eq!(*mg.pinned_shared(), 100);
            assert_eq!(mg.once_shared(), None);
        }
        {
            lock!(let mut wg = s.write_rw());
            lock!(let mut mg = s.lock_mu());
            *wg.as_mut().shared_mut(&mut mg) = 25;
            assert_eq!(*wg.shared(), 25);
            *mg.as_mut().shared_mut(&mut wg) = 50;
            assert_eq!(*mg.shared(), 50);

            *wg.as_mut().pinned_shared_mut(&mut mg) = 200;
            assert_eq!(*mg.pinned_shared(), 200);

            let _ = wg.as_mut().once_shared_cell(&mut mg).get_or_init(|| 300);
            assert_eq!(wg.once_shared(), Some(&300));

            let wf_mut = wg.as_mut().fields_mut();
            *wf_mut.only_rw = 2;
            assert_eq!(*wf_mut.shared, 50);
            assert_eq!(*wf_mut.pinned_shared, 200);
            assert_eq!(wf_mut.once_shared, Some(&300));

            // Test `BrwLockPi` `WriteTokenGuard` as both receiver and argument.
            let mut wtg = wg.as_mut().as_token_guard_mut();
            let mut mtg = mg.as_mut().as_token_guard_mut();
            *wtg.reborrow().shared_mut(&mut mtg) = 75;
            assert_eq!(*wtg.as_token_guard().shared(), 75);
            *mtg.reborrow().shared_mut(&mut wtg) = 90;
            assert_eq!(*wtg.shared(), 90);
        }
    }
}
