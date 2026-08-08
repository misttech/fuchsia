// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#[cfg(ktest)]
#[unittest::suite(name = "rust_ksync")]
/// Tests for Rust ksync bindings
mod ksync_tests {
    use pin_init::{pin_data, pin_init, stack_pin_init};
    use unittest::{assert_true, expect_false, expect_ok, expect_true};

    #[ksync::guarded]
    #[fbl::ref_counted]
    #[derive(fbl::Recyclable)]
    #[pin_data]
    #[repr(C)]
    struct GuardedMutexObj {
        #[mutex]
        mu: ksync::KMutex,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedSpinlockObj {
        #[mutex]
        mu: ksync::KMutex<ksync::RawSpinlock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedSpinlockObj2 {
        #[mutex]
        mu: ksync::KMutex<ksync::RawSpinlock>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedCriticalMutexObj {
        #[mutex]
        mu: ksync::KMutex<ksync::RawCriticalMutex>,
        #[guarded_by(mu)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedBrwLockObj {
        #[brwlock]
        lock: ksync::BrwLockPi,
        #[guarded_by(lock)]
        value: u32,
    }

    #[ksync::guarded]
    struct GuardedPhantomObj {
        #[mutex(GuardedMutexObjMuClass)]
        mu: ksync::KMutex<ksync::PhantomMutex>,
        #[guarded_by(mu)]
        target: fbl::RefPtr<GuardedMutexObj>,
    }

    #[ksync::guarded]
    #[fbl::ref_counted]
    #[derive(fbl::Recyclable)]
    #[pin_data]
    #[repr(C)]
    struct GuardedGenericObj<T> {
        #[mutex]
        mu: ksync::KMutex,
        #[guarded_by(mu)]
        value: T,
    }

    #[ksync::guarded]
    struct GuardedGenericPhantomObj<T> {
        #[mutex(GuardedGenericObjMuClass<T>)]
        mu: ksync::KMutex<ksync::PhantomMutex>,
        #[guarded_by(mu)]
        target: fbl::RefPtr<GuardedGenericObj<T>>,
    }

    unsafe extern "C" {
        fn cpp_verify_mutex_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_critical_mutex_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_spinlock_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
        fn cpp_verify_brwlock_id(
            lock: *const core::ffi::c_void,
            expected_id: *const core::ffi::c_void,
        ) -> bool;
    }

    /// test Rust KMutex ID
    #[test]
    fn mutex_id() {
        let obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        })
        .unwrap();
        unsafe {
            assert_true!(cpp_verify_mutex_id(
                &obj.mu as *const _ as *const core::ffi::c_void,
                <GuardedMutexObjMuClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust KCriticalMutex ID
    #[test]
    fn critical_mutex_id() {
        stack_pin_init!(let obj = pin_init!(GuardedCriticalMutexObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
        unsafe {
            assert_true!(cpp_verify_critical_mutex_id(
                &obj.mu as *const _ as *const core::ffi::c_void,
                <GuardedCriticalMutexObjMuClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust KSpinlock ID
    #[test]
    fn spinlock_id() {
        stack_pin_init!(let obj = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
        unsafe {
            assert_true!(cpp_verify_spinlock_id(
                &obj.mu as *const _ as *const core::ffi::c_void,
                <GuardedSpinlockObjMuClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust BrwLockPi ID
    #[test]
    fn brwlock_id() {
        stack_pin_init!(let obj = pin_init!(GuardedBrwLockObj {
            lock <- ksync::BrwLockPi::init(),
            value: 0.into(),
        }));
        unsafe {
            assert_true!(cpp_verify_brwlock_id(
                &obj.lock as *const _ as *const core::ffi::c_void,
                <GuardedBrwLockObjLockClass as ksync::LockClass>::ID,
            ));
        }
    }

    /// test Rust KSpinlock
    #[test]
    fn spinlock() {
        stack_pin_init!(let obj = pin_init!(GuardedSpinlockObj {
            mu <- ksync::KMutex::init(),
            value: 100.into(),
        }));
        stack_pin_init!(let obj2 = pin_init!(GuardedSpinlockObj2 {
            mu <- ksync::KMutex::init(),
            value: 100.into(),
        }));

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_true!(*guard.value() == 100);
            *guard.as_mut().value_mut() = 101;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_true!(*guard.value() == 101);
        }
        {
            ksync::lock!(let guard = obj.mu.lock());
            ksync::lock!(let guard2 = obj2.mu.lock_policy::<ksync::NoIrqSavePolicy>());
            // Expect a no irq save policy guard to be strictly smaller than one that had to
            // remember the irq state.
            expect_true!(core::mem::size_of_val(&*guard2) < core::mem::size_of_val(&*guard));
        }
    }

    /// test Rust KMutex
    #[test]
    fn mutex() {
        let obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 42.into(),
        })
        .unwrap();

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_true!(*guard.value() == 42);
            *guard.as_mut().value_mut() = 43;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_true!(*guard.value() == 43);
        }
    }

    /// test PhantomMutex and #[mutex(LockClass)]
    #[test]
    fn phantom_lock() {
        // This test demonstrates how an object (`phantom_obj`) can use a zero-sized `PhantomMutex`
        // to participate in `#[guarded]` type checking while physical mutual exclusion is
        // provided by an external physical lock (`real_obj.mu`).

        // Create an object with a real, physical `KMutex`, wrapped in an `fbl::RefPtr`.
        let real_obj = fbl::pin_make_ref_counted!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 1.into(),
        })
        .unwrap();

        // Create an object protected by a zero-sized `PhantomMutex` (no physical lock storage).
        // Its synchronization is provided externally by `real_obj.mu`.
        let target = real_obj.clone();
        stack_pin_init!(let phantom_obj = pin_init!(GuardedPhantomObj {
            mu: ksync::KMutex::new(ksync::PhantomMutex),
            target: target.into(),
        }));

        {
            // Acquire a guard for `phantom_obj` by passing the physical `real_obj.mu` to `lock_mu`.
            ksync::lock!(let mut guard = phantom_obj.lock_mu(&real_obj.mu));

            // Read the reference to `real_obj` from `phantom_obj.target`.
            let target = guard.target().clone();

            // Using the proof token from `guard`, read `target.value`.
            expect_true!(*target.guard_mu(guard.token()).value() == 1);

            // Modify `target.value` using the mutable token from `guard`.
            *target.guard_mu_mut(guard.as_mut().token_mut()).value_mut() = 200;
        }

        {
            // Lock `real_obj` directly using `GuardedMutexObj`'s own guard to verify
            // that we read back the same value (200) that was written via `phantom_obj`.
            ksync::lock!(let guard = real_obj.lock_mu());
            expect_true!(*guard.value() == 200);
        }
    }

    /// test PhantomMutex with generic LockClass
    #[test]
    fn generic_phantom_mutex() {
        let real_obj = fbl::pin_make_ref_counted!(GuardedGenericObj {
            mu <- ksync::KMutex::init(),
            value: 42.into(),
        })
        .unwrap();

        let real_obj_clone = real_obj.clone();
        stack_pin_init!(let phantom_obj = pin_init!(GuardedGenericPhantomObj {
            mu: ksync::KMutex::new(ksync::PhantomMutex),
            target: ksync::KCell::new(real_obj_clone),
        }));

        {
            ksync::lock!(let mut guard = phantom_obj.lock_mu(&real_obj.mu));
            let target = guard.target().clone();
            expect_true!(*target.guard_mu(guard.token()).value() == 42);
            *target.guard_mu_mut(guard.as_mut().token_mut()).value_mut() = 99;
        }

        {
            ksync::lock!(let guard = real_obj.lock_mu());
            expect_true!(*guard.value() == 99);
        }
    }

    /// test Rust KEvent
    #[test]
    fn event() {
        stack_pin_init!(let event = ksync::KEvent::init(false));
        expect_false!(event.wait_deadline(0).is_ok());
        event.signal();
        expect_ok!(event.wait_deadline(0));
        event.unsignal();
        expect_false!(event.wait_deadline(0).is_ok());
    }

    /// test Rust BrwLockPi
    #[test]
    fn brwlock() {
        stack_pin_init!(let obj = pin_init!(GuardedBrwLockObj {
            lock <- ksync::BrwLockPi::init(),
            value: 10.into(),
        }));

        {
            ksync::lock!(let guard = obj.read_lock());
            expect_true!(*guard.value() == 10);
        }

        {
            ksync::lock!(let mut guard = obj.write_lock());
            expect_true!(*guard.value() == 10);
            *guard.as_mut().value_mut() = 20;
        }

        {
            ksync::lock!(let guard = obj.read_lock());
            expect_true!(*guard.value() == 20);
        }
    }
}
