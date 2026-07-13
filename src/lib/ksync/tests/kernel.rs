// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![no_std]

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
struct GuardedBrwLockObj {
    #[brwlock]
    lock: ksync::BrwLockPi,
    #[guarded_by(lock)]
    value: u32,
}

#[unsafe(no_mangle)]
pub extern "C" fn test_ksync_spinlock() -> bool {
    ksync::pin_init::stack_pin_init!(let obj = ksync::pin_init::pin_init!(GuardedSpinlockObj {
        mu <- ksync::KMutex::init(),
        value: 100.into(),
    }));

    {
        ksync::lock!(let mut guard = obj.lock_mu());
        if *guard.value() != 100 {
            return false;
        }
        *guard.as_mut().value_mut() = 101;
    }

    {
        ksync::lock!(let guard = obj.lock_mu());
        if *guard.value() != 101 {
            return false;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_ksync_mutex() -> bool {
    ksync::pin_init::stack_pin_init!(let obj = ksync::pin_init::pin_init!(GuardedMutexObj {
        mu <- ksync::KMutex::init(),
        value: 42.into(),
    }));

    {
        ksync::lock!(let mut guard = obj.lock_mu());
        if *guard.value() != 42 {
            return false;
        }
        *guard.as_mut().value_mut() = 43;
    }

    {
        ksync::lock!(let guard = obj.lock_mu());
        if *guard.value() != 43 {
            return false;
        }
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_ksync_event() -> bool {
    ksync::pin_init::stack_pin_init!(let event = ksync::KEvent::init(false));
    if event.wait_deadline(0).is_ok() {
        return false;
    }
    event.signal();
    if event.wait_deadline(0).is_err() {
        return false;
    }
    event.unsignal();
    if event.wait_deadline(0).is_ok() {
        return false;
    }
    true
}

#[unsafe(no_mangle)]
pub extern "C" fn test_ksync_brwlock() -> bool {
    ksync::pin_init::stack_pin_init!(let obj = ksync::pin_init::pin_init!(GuardedBrwLockObj {
        lock <- ksync::BrwLockPi::init(),
        value: 10.into(),
    }));

    {
        ksync::lock!(let guard = obj.read_lock());
        if *guard.value() != 10 {
            return false;
        }
    }

    {
        ksync::lock!(let mut guard = obj.write_lock());
        if *guard.value() != 10 {
            return false;
        }
        *guard.as_mut().value_mut() = 20;
    }

    {
        ksync::lock!(let guard = obj.read_lock());
        if *guard.value() != 20 {
            return false;
        }
    }
    true
}
