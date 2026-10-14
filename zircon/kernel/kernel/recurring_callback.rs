// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::kernel::deadline::{Deadline, SlackMode, TimerSlack};
use crate::kernel::thread;
use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
use crate::platform_rs::timer::{DurationMono, DurationUnknown, InstantMono};
use core::ffi::c_void;
use core::mem::MaybeUninit;
use ksync::{KMutex, KSpinlock, guarded, kcell_init};
use pin_init::{InPlaceWrite, PinInit, pin_init};
use zx_types::zx_instant_mono_t;

const SLACK: TimerSlack =
    TimerSlack::new(DurationUnknown(DurationMono::from_millis(10).into_nanos()), SlackMode::Center);

/// Function pointer type for recurring callbacks.
pub type CallbackFunc = extern "C" fn();

/// A recurring callback that fires once per second when toggled on.
#[guarded]
#[repr(C)]
pub struct RecurringCallback {
    #[mutex]
    lock: KSpinlock,
    #[pin]
    #[guarded_by(lock)]
    timer: Timer,
    #[guarded_by(lock)]
    started: bool,
    func: CallbackFunc,
}

const EXPECTED_SPINLOCK_SIZE: usize = if cfg!(scheduler_lock_spin_tracing_enabled) {
    24
} else if cfg!(with_lock_dep) {
    16
} else {
    8
};
const EXPECTED_RECURRING_CALLBACK_SIZE: usize = EXPECTED_SPINLOCK_SIZE + 88;

zr::static_assert!(core::mem::size_of::<RecurringCallback>() == EXPECTED_RECURRING_CALLBACK_SIZE);
zr::static_assert!(core::mem::align_of::<RecurringCallback>() == 8);
zr::static_assert!(core::mem::offset_of!(RecurringCallback, lock) == 0);
zr::static_assert!(core::mem::offset_of!(RecurringCallback, timer) == EXPECTED_SPINLOCK_SIZE);
zr::static_assert!(
    core::mem::offset_of!(RecurringCallback, started) == EXPECTED_SPINLOCK_SIZE + 72
);
zr::static_assert!(core::mem::offset_of!(RecurringCallback, func) == EXPECTED_SPINLOCK_SIZE + 80);

impl RecurringCallback {
    /// Creates an in-place initializer for a `RecurringCallback` with the given `callback`.
    pub fn init(callback: CallbackFunc) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            lock <- KSpinlock::init(),
            timer <- kcell_init(Timer::init(ZX_CLOCK_MONOTONIC)),
            started: false.into(),
            func: callback,
        })
    }

    /// # Safety
    ///
    /// `arg` must be a valid pointer to an initialized [`RecurringCallback`] that remains live for
    /// the duration of this callback, and this callback must be invoked from interrupt context
    /// with interrupts and preemption disabled.
    unsafe extern "C" fn callback_wrapper(
        _t: *mut Timer,
        now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` was passed as a pointer to `Self` in `toggle` or a previous
        // `callback_wrapper` invocation, and remains valid for the duration of the callback.
        let cb = unsafe { &*(arg as *const Self) };
        (cb.func)();

        {
            ksync::lock!(let mut guard = cb.lock_lock());
            let fields = guard.as_mut().fields_mut();

            if *fields.started {
                let deadline =
                    Deadline::new((InstantMono(now) + DurationMono::from_seconds(1)).into(), SLACK);
                // SAFETY: `Self::callback_wrapper` and `arg` are valid for the timer callback.
                unsafe {
                    fields.timer.set_deadline(&deadline, Self::callback_wrapper, arg);
                }
            }
        }

        // Reschedule to give the debuglog a chance to run
        thread::preempt_set_pending();
    }

    /// Toggles the recurring callback on or off.
    pub fn toggle(&self) {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();

        if !*fields.started {
            let deadline = Deadline::after_mono(DurationMono::from_seconds(1), SLACK);
            // Start the timer
            // SAFETY: `Self::callback_wrapper` is a valid FFI timer callback and `self` points to
            // a pinned `RecurringCallback` that remains valid while the timer is active.
            unsafe {
                fields.timer.set_deadline(
                    &deadline,
                    Self::callback_wrapper,
                    (self as *const Self).cast_mut().cast(),
                );
            }
            *fields.started = true;
        } else {
            fields.timer.cancel();
            *fields.started = false;
        }
    }
}

/// FFI trampoline to initialize a [`RecurringCallback`] in place.
///
/// # Safety
///
/// `cb` must point to valid, aligned, uninitialized storage for a [`RecurringCallback`], and the
/// initialized object must remain pinned in memory until dropped by
/// [`rust_recurring_callback_destroy`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_recurring_callback_init(
    cb: *mut MaybeUninit<RecurringCallback>,
    callback: CallbackFunc,
) {
    // SAFETY: Caller guarantees `cb` points to valid, aligned, uninitialized storage that remains
    // pinned after initialization.
    let slot = unsafe { &mut *cb };
    let _ = slot.write_pin_init(RecurringCallback::init(callback));
}

/// FFI trampoline to destroy a [`RecurringCallback`] in place.
///
/// # Safety
///
/// `cb` must point to a valid, initialized [`RecurringCallback`] that will not be used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_recurring_callback_destroy(cb: *mut RecurringCallback) {
    // SAFETY: Caller guarantees `cb` points to a valid, initialized `RecurringCallback`.
    unsafe {
        core::ptr::drop_in_place(cb);
    }
}

/// FFI trampoline for [`RecurringCallback::toggle`].
#[unsafe(no_mangle)]
pub extern "C" fn rust_recurring_callback_toggle(cb: &RecurringCallback) {
    cb.toggle();
}
