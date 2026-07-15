// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::mem::MaybeUninit;
use core::ops::Deref;
use counters_rs::define_kcounter;
use fbl::Canary;
use ksync::{KMutex, RawCriticalMutex, guarded};
use pin_init::{PinInit, pin_init};
use zr::ToMutPtr;
use zx_status::Status;
use zx_types::{
    ZX_COUNTER_NON_POSITIVE, ZX_COUNTER_POSITIVE, ZX_OBJ_TYPE_COUNTER, ZX_RIGHT_DUPLICATE,
    ZX_RIGHT_INSPECT, ZX_RIGHT_READ, ZX_RIGHT_SIGNAL, ZX_RIGHT_TRANSFER, ZX_RIGHT_WAIT,
    ZX_RIGHT_WRITE, zx_rights_t,
};

use crate::counter_dispatcher_ffi::cpp_counter_dispatcher_create;
use crate::dispatcher::Dispatcher;
use crate::{DispatcherOps, KernelHandle};

use object_constants_rs as object_constants;

// TODO(https://fxbug.dev/532573303): Share this definition with
// zircon/system/public/zircon/rights.h
pub const DEFAULT_RIGHTS: zx_rights_t = ZX_RIGHT_TRANSFER
    | ZX_RIGHT_DUPLICATE
    | ZX_RIGHT_WAIT
    | ZX_RIGHT_INSPECT
    | ZX_RIGHT_READ
    | ZX_RIGHT_WRITE
    | ZX_RIGHT_SIGNAL;

// Ensure size and alignment match the constants in object-constants.
zr::static_assert_size_and_align!(
    CounterDispatcherState,
    object_constants::kCounterDispatcherStateSize,
    object_constants::kCounterDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_COUNTER_CREATE_COUNT, "dispatcher.counter.create", Sum);
define_kcounter!(DISPATCHER_COUNTER_DESTROY_COUNT, "dispatcher.counter.destroy", Sum);

#[guarded]
#[repr(C)]
pub struct CounterDispatcherState {
    canary: Canary<{ fbl::magic(b"SOLO") }>,

    #[guarded_by(lock)]
    value: i64,

    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl CounterDispatcherState {
    pub fn init() -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_COUNTER_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            value: 0.into(),
            lock <- KMutex::init(),
        })
    }
}

#[repr(C)]
pub struct CounterDispatcher {
    _facade: fbl::OpaqueRefCountedFacade<Dispatcher>,
}

impl Deref for CounterDispatcher {
    type Target = Dispatcher;
    fn deref(&self) -> &Self::Target {
        unsafe { &*self.dispatcher() }
    }
}

unsafe impl fbl::IsOpaqueRefCounted for CounterDispatcher {
    type TargetBase = Dispatcher;
}

impl crate::DispatcherOps for CounterDispatcher {
    type LockClass = CounterDispatcherStateLockClass;
    const TYPE: zx_types::zx_obj_type_t = ZX_OBJ_TYPE_COUNTER;

    fn dispatcher(&self) -> *const Dispatcher {
        self.as_raw_ptr() as *const Dispatcher
    }
}

impl CounterDispatcher {
    /// Returns a reference to the underlying Rust state object.
    pub fn state(&self) -> &CounterDispatcherState {
        unsafe {
            let ptr = self
                .as_raw_ptr()
                .cast::<u8>()
                .add(object_constants::kCounterDispatcherStateOffset as usize)
                .cast::<CounterDispatcherState>();
            &*ptr
        }
    }

    /// Returns a raw pointer to `self` for passing to state callbacks.
    pub fn as_raw_ptr(&self) -> *const CounterDispatcher {
        self as *const Self
    }

    /// Returns the counter's value.
    pub fn value(&self) -> i64 {
        ksync::lock!(let guard = self.state().lock_lock());
        let fields = guard.fields();
        *fields.value
    }

    /// Sets the counter's value, asserting/deasserting signals as appropriate.
    pub fn set_value(&self, new_val: i64) {
        ksync::lock!(let mut guard = self.state().lock_lock());
        let fields = guard.as_mut().fields_mut();
        let old_val = *fields.value;
        *fields.value = new_val;
        self.update_signals_locked(guard.token(), old_val, new_val);
    }

    /// Adds `amount` to this counter.
    pub fn add(&self, amount: i64) -> Result<(), Status> {
        ksync::lock!(let mut guard = self.state().lock_lock());
        let fields = guard.as_mut().fields_mut();
        let old_val = *fields.value;
        let new_val = match old_val.checked_add(amount) {
            Some(val) => val,
            None => return Err(Status::OUT_OF_RANGE),
        };
        *fields.value = new_val;
        self.update_signals_locked(guard.token(), old_val, new_val);
        Ok(())
    }

    fn update_signals_locked(
        &self,
        token: &ksync::LockToken<'_, CounterDispatcherStateLockClass>,
        old_val: i64,
        new_val: i64,
    ) {
        if old_val <= 0 && new_val > 0 {
            self.update_state_locked(token, ZX_COUNTER_NON_POSITIVE, ZX_COUNTER_POSITIVE);
        } else if old_val > 0 && new_val <= 0 {
            self.update_state_locked(token, ZX_COUNTER_POSITIVE, ZX_COUNTER_NON_POSITIVE);
        }
    }

    /// Creates a new CounterDispatcher via C++ and returns its kernel handle and rights.
    pub fn create() -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let mut handle_out = MaybeUninit::<KernelHandle<Self>>::uninit();
        let status = unsafe { cpp_counter_dispatcher_create(handle_out.as_mut_ptr()) };
        Status::ok(status)?;
        // SAFETY: cpp_counter_dispatcher_create initialized the handle.
        unsafe { Ok((handle_out.assume_init(), DEFAULT_RIGHTS)) }
    }
}

/// # Safety
///
/// The caller must ensure `ptr` points to an initialized `CounterDispatcherState`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_counter_dispatcher_state_get_lock(
    ptr: *const CounterDispatcherState,
) -> *mut KMutex<CounterDispatcherStateLockClass, RawCriticalMutex> {
    unsafe {
        let lock_ref = &(*ptr).lock;
        lock_ref.to_mut_ptr()
    }
}
