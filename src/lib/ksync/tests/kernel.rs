// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]
#![cfg_attr(not(ktest), allow(unused_crate_dependencies))]

#[cfg(ktest)]
#[unittest::test_suite(name = "rust_ksync")]
/// Tests for Rust ksync bindings
mod ksync_tests {
    use pin_init::{pin_init, stack_pin_init};
    use unittest::{assert_true, expect_false, expect_ok, expect_true};

    #[unsafe(no_mangle)]
    pub extern "C" fn ksync_tests_link_helper() {}

    #[ksync::guarded]
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
        stack_pin_init!(let obj = pin_init!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 0.into(),
        }));
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

        {
            ksync::lock!(let mut guard = obj.lock_mu());
            expect_true!(*guard.value() == 100);
            *guard.as_mut().value_mut() = 101;
        }

        {
            ksync::lock!(let guard = obj.lock_mu());
            expect_true!(*guard.value() == 101);
        }
    }

    /// test Rust KMutex
    #[test]
    fn mutex() {
        stack_pin_init!(let obj = pin_init!(GuardedMutexObj {
            mu <- ksync::KMutex::init(),
            value: 42.into(),
        }));

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
