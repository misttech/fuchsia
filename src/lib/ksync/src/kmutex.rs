// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::marker::PhantomData;
use core::pin::Pin;
use pin_init::{PinInit, pin_data, pin_init, pin_init_from_closure, pinned_drop};

use crate::{LockToken, RawLock, RawMutex};
use lockdep::LockClass;

/// A safe, Zircon-compatible mutual exclusion lock supporting compile-time order validation.
///
/// `KMutex` wraps a platform-specific `RawLock` abstraction. It is pinned in memory to support FFI
/// loop-detector active list registrations safely under the lock class `Class`.
#[repr(transparent)] // Ensure KMutex has the same layout as the underlying RawLock M.
#[pin_data]
pub struct KMutex<Class: LockClass, M: RawLock = RawMutex> {
    #[pin]
    mutex: M,
    _marker: PhantomData<Class>,
}

impl<Class: LockClass, M: RawLock> KMutex<Class, M> {
    /// Create a new KMutex with a pre-initialized raw lock.
    pub const fn new(mutex: M) -> Self {
        Self { mutex, _marker: PhantomData }
    }

    /// Safe dynamic initialization of the validation lock inside pin context.
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            mutex <- unsafe { M::init(Self::class_id()) },
            _marker: PhantomData,
        })
    }

    /// Acquires the lock and registers the active loop node.
    #[inline]
    pub fn lock(&self) -> impl PinInit<KMutexGuard<'_, Class, M>, core::convert::Infallible> {
        KMutexGuard::new(self)
    }

    const fn class_id() -> *const core::ffi::c_void {
        if cfg!(feature = "lock_dep") { Class::ID } else { core::ptr::null() }
    }
}

impl<Class: LockClass, M: RawLock> core::fmt::Debug for KMutex<Class, M> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("KMutex").field("class", &core::any::type_name::<Class>()).finish()
    }
}

/// A validation guard representing exclusive lock ownership and active list participation.
///
/// The guard is pinned in memory to ensure that its `lock_entry` pointer remains safe and valid
/// inside the C++ loop detector active thread list.
#[repr(C)]
#[pin_data(PinnedDrop)]
pub struct KMutexGuard<'a, Class: LockClass, M: RawLock = RawMutex> {
    mutex: &'a KMutex<Class, M>,

    #[pin]
    lock_entry: M::LockEntry,

    state: M::GuardState,

    token: LockToken<'a, Class>,
}

impl<'a, Class: LockClass, M: RawLock> KMutexGuard<'a, Class, M> {
    /// Creates a new stack-pinned validation guard initialization block.
    pub fn new(mutex: &'a KMutex<Class, M>) -> impl PinInit<Self, core::convert::Infallible> {
        // SAFETY: The closure correctly initializes all fields of the allocated `KMutexGuard`
        // and satisfies all safety requirements of `pin_init_from_closure`.
        unsafe {
            pin_init_from_closure(move |this: *mut Self| -> Result<(), core::convert::Infallible> {
                // SAFETY: `this` is a valid pointer to uninitialized memory allocated for
                // `KMutexGuard`.

                let mutex_addr = core::ptr::addr_of_mut!((*this).mutex);
                core::ptr::write(mutex_addr, mutex);

                let entry_addr = core::ptr::addr_of_mut!((*this).lock_entry);
                core::ptr::write(entry_addr, M::LockEntry::default());

                let state = mutex.mutex.acquire(entry_addr);

                let state_addr = core::ptr::addr_of_mut!((*this).state);
                core::ptr::write(state_addr, state);

                let token_addr = core::ptr::addr_of_mut!((*this).token);
                core::ptr::write(token_addr, LockToken::new());

                Ok(())
            })
        }
    }

    /// Returns a shared reference to the lock proof `LockToken`.
    #[inline]
    pub fn token(&self) -> &LockToken<'a, Class> {
        &self.token
    }

    /// Returns a mutable reference to the lock proof `LockToken` inside this pinned projection.
    #[inline]
    pub fn token_mut(self: Pin<&mut Self>) -> &mut LockToken<'a, Class> {
        // SAFETY: Modifying the non-pinned raw `token` field does not violate pinning invariants
        // since the token has no drop logic or pointer-location sensitivity.
        let me = unsafe { self.get_unchecked_mut() };
        &mut me.token
    }
}

#[pinned_drop]
impl<'a, Class: LockClass, M: RawLock> PinnedDrop for KMutexGuard<'a, Class, M> {
    // SAFETY: The stack slot `lock_entry` remains valid and pinned on the stack until this drop
    // block completes. Accessing the fields directly to release the raw lock and remove the
    // active list node is safe and correct under the current thread context.
    fn drop(self: Pin<&mut Self>) {
        unsafe {
            let me = self.get_unchecked_mut();
            let entry_addr = &mut me.lock_entry as *mut _;
            me.mutex.mutex.release(entry_addr, me.state);
        }
    }
}

#[cfg(not(feature = "kernel"))]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::{KCell, RawMutex, guarded};
    use lockdep::LockClass;
    use pin_init::{pin_init, stack_pin_init};

    struct MyClass;
    impl LockClass for MyClass {
        const ID: *mut core::ffi::c_void = core::ptr::null_mut();
    }

    #[pin_init::pin_data]
    struct MyStruct {
        #[pin]
        mu: KMutex<MyClass>,
        data1: KCell<u32, MyClass>,
        data2: KCell<i32, MyClass>,
    }

    #[test]
    fn test_basic_token_access() {
        stack_pin_init!(let s = pin_init!(MyStruct {
            mu <- KMutex::init(),
            data1: KCell::new(10),
            data2: KCell::new(-5),
        }));

        lock!(let mut guard = s.mu.lock());

        unsafe {
            assert_eq!(*s.data1.get(guard.token()), 10);
            assert_eq!(*s.data2.get(guard.token()), -5);
        }
        unsafe {
            let token_mut = guard.as_mut().token_mut();
            *s.data1.get_mut(token_mut) = 20;
            assert_eq!(*s.data1.get(guard.token()), 20);
        }
    }

    #[guarded]
    struct MyGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data1: u32,
        #[guarded_by(mu)]
        data2: i32,
    }

    #[test]
    fn test_macro_guarded() {
        stack_pin_init!(let s = pin_init!(MyGuardedStruct {
            mu <- KMutex::init(),
            data1: 100.into(),
            data2: (-50).into(),
        }));

        {
            lock!(let mut guard = s.lock_mu());

            // Safe individual field access
            assert_eq!(*guard.data1(), 100);
            assert_eq!(*guard.data2(), -50);

            *guard.as_mut().data1_mut() = 200;
            assert_eq!(*guard.data1(), 200);

            // Safe disjoint/split access
            let fields = guard.as_mut().fields_mut();
            *fields.data1 += 50;
            *fields.data2 += 50;
        }

        // Verify fields
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data1(), 250);
        assert_eq!(*guard.data2(), 0);
    }

    #[test]
    fn test_kmutex_init() {
        stack_pin_init!(let mu = KMutex::<MyClass>::init());
        lock!(mu.lock());
    }

    #[test]
    fn test_kmutex_debug() {
        extern crate std;
        stack_pin_init!(let mu = KMutex::<MyClass>::init());
        let debug_str = std::format!("{:?}", mu);
        assert!(debug_str.contains("KMutex"));
    }

    #[guarded]
    struct MyMultiGuardedStruct {
        #[mutex]
        mu1: KMutex,
        #[mutex]
        mu2: KMutex,
        #[guarded_by(mu1)]
        data1: u32,
        #[guarded_by(mu2)]
        data2: i32,
    }

    #[test]
    fn test_macro_multi_guarded() {
        stack_pin_init!(let s = pin_init!(MyMultiGuardedStruct {
            mu1 <- KMutex::init(),
            mu2 <- KMutex::init(),
            data1: 10.into(),
            data2: 20.into(),
        }));

        lock!(let mut guard1 = s.lock_mu1());
        lock!(let mut guard2 = s.lock_mu2());

        assert_eq!(*guard1.data1(), 10);
        assert_eq!(*guard2.data2(), 20);
        *guard1.as_mut().data1_mut() = 15;
        *guard2.as_mut().data2_mut() = 25;
        assert_eq!(*guard1.data1(), 15);
        assert_eq!(*guard2.data2(), 25);
    }

    #[guarded]
    struct MyDefaultGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: u32,
    }

    #[test]
    fn test_derive_default_guarded() {
        stack_pin_init!(let s = pin_init!(MyDefaultGuardedStruct {
            mu <- KMutex::init(),
            data: 0.into(),
        }));
        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 0);
    }

    #[guarded]
    struct MyGenericLockGuardedStruct<L: RawLock> {
        #[mutex]
        mu: KMutex<L>,
        #[guarded_by(mu)]
        data: u32,
    }

    #[test]
    fn test_macro_generic_lock_guarded() {
        stack_pin_init!(let s = pin_init!(MyGenericLockGuardedStruct::<RawMutex> {
            mu <- KMutex::init(),
            data: 100.into(),
        }));

        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 100);
    }

    #[guarded]
    struct MyGenericGuardedStruct<T> {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: T,
    }

    #[test]
    fn test_macro_generic_guarded() {
        stack_pin_init!(let s = pin_init!(MyGenericGuardedStruct::<u32> {
            mu <- KMutex::init(),
            data: 0.into(),
        }));
        lock!(let mut guard = s.lock_mu());
        assert_eq!(*guard.data(), 0);

        *guard.as_mut().data_mut() = 42;
        assert_eq!(*guard.data(), 42);

        let fields = guard.as_mut().fields_mut();
        *fields.data = 100;

        let fields_shared = guard.fields();
        assert_eq!(*fields_shared.data, 100);
    }

    #[guarded]
    struct MyExplicitParentGuardedStruct {
        #[mutex]
        mu: KMutex,
        #[guarded_by(mu)]
        data: u32,
        pub label: &'static str,
    }

    impl MyExplicitParentGuardedStruct {
        pub fn has_label(&self) -> bool {
            !self.label.is_empty()
        }
    }

    impl<'a> MyExplicitParentGuardedStructMuGuard<'a> {
        pub fn process_with_context(self: Pin<&mut Self>) {
            let me = unsafe { self.get_unchecked_mut() };
            let has_label = me.parent.has_label();
            let label = me.parent.label;
            if has_label && label == "apply_update" {
                unsafe {
                    let mut_self = Pin::new_unchecked(me);
                    let fields = mut_self.fields_mut();
                    *fields.data = 100;
                }
            }
        }
    }

    #[test]
    fn test_macro_guard_explicit_parent_access() {
        stack_pin_init!(let s = pin_init!(MyExplicitParentGuardedStruct {
            mu <- KMutex::init(),
            data: 0.into(),
            label: "apply_update",
        }));

        {
            lock!(let mut guard = s.lock_mu());
            guard.as_mut().process_with_context();
        }

        lock!(let guard = s.lock_mu());
        assert_eq!(*guard.data(), 100);
    }
}
