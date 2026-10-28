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
use ksync::{KMutex, KSpinlock, guarded, kcell_init};
use pin_init::{PinInit, pin_init};
use zx_types::zx_instant_mono_t;

const SLACK: TimerSlack =
    TimerSlack::new(DurationUnknown(DurationMono::from_millis(10).into_nanos()), SlackMode::Center);

/// Function pointer type for recurring callbacks.
pub type CallbackFunc = fn();

/// A recurring callback that fires once per second when toggled on.
#[guarded]
struct RecurringCallback {
    #[mutex]
    lock: KSpinlock,
    #[pin]
    #[guarded_by(lock)]
    timer: Timer,
    #[guarded_by(lock)]
    started: bool,
    func: CallbackFunc,
}

impl RecurringCallback {
    /// Creates an in-place initializer for a `RecurringCallback` with the given `callback`.
    fn init(callback: CallbackFunc) -> impl PinInit<Self, core::convert::Infallible> {
        pin_init!(Self {
            lock <- KSpinlock::init(),
            timer <- kcell_init(Timer::init(ZX_CLOCK_MONOTONIC)),
            started: false.into(),
            func: callback,
        })
    }

    /// # Safety
    ///
    /// `arg` must be a valid pointer to an initialized `'static` [`RecurringCallback`], and this
    /// callback must be invoked from interrupt context with interrupts and preemption disabled.
    unsafe extern "C" fn callback_wrapper(
        _t: *mut Timer,
        now: zx_instant_mono_t,
        arg: *mut c_void,
    ) {
        // SAFETY: `arg` was passed as a pointer to a `'static` `Self` in `toggle` or a previous
        // `callback_wrapper` invocation, so it remains valid for the lifetime of the kernel.
        let cb = unsafe { &*(arg as *const Self) };
        (cb.func)();

        {
            ksync::lock!(let mut guard = cb.lock_lock());
            let fields = guard.as_mut().fields_mut();

            if *fields.started {
                let deadline =
                    Deadline::new((InstantMono(now) + DurationMono::from_seconds(1)).into(), SLACK);
                // SAFETY: `Self::callback_wrapper` and `'static` `arg` remain valid for the timer
                // callback.
                unsafe {
                    fields.timer.as_ref().set_deadline(&deadline, Self::callback_wrapper, arg);
                }
            }
        }

        // Reschedule to give the debuglog a chance to run
        thread::preempt_set_pending();
    }

    /// Toggles the recurring callback on or off.
    fn toggle(&'static self) {
        ksync::lock!(let mut guard = self.lock_lock());
        let fields = guard.as_mut().fields_mut();

        if !*fields.started {
            let deadline = Deadline::after_mono(DurationMono::from_seconds(1), SLACK);
            // Start the timer
            // SAFETY: `Self::callback_wrapper` is a valid FFI timer callback and `self` is a
            // `'static` pinned `RecurringCallback` that remains valid for the lifetime of the
            // kernel.
            unsafe {
                fields.timer.as_ref().set_deadline(
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

ksync::declare_singleton_spinlock!(
    /// Spinlock protecting lazy initialization of `StaticRecurringCallback` instances.
    RecurringCallbackInitLock
);

/// A statically allocated [`RecurringCallback`] that lazily initializes its pinned [`Timer`] on
/// first [`toggle`](Self::toggle).
pub struct StaticRecurringCallback {
    cell: ksync::KOnceCell<RecurringCallback, RecurringCallbackInitLock>,
    func: CallbackFunc,
}

impl StaticRecurringCallback {
    /// Creates a new static recurring callback that invokes `func` once per second when toggled on.
    pub const fn new(func: CallbackFunc) -> Self {
        Self { cell: ksync::KOnceCell::new(), func }
    }

    /// Toggles the recurring callback on or off.
    pub fn toggle(&'static self) {
        let cb: &'static RecurringCallback = {
            ksync::lock!(let mut guard = RecurringCallbackInitLock::get().lock());
            // SAFETY: `guard.token_mut()` belongs to the singleton `RecurringCallbackInitLock`
            // that protects `self.cell`. Once initialized, `self.cell` is never mutated again, so
            // `get_unchecked()` soundly returns a `'static` shared reference without creating a
            // `&mut RecurringCallback` alias on subsequent calls.
            unsafe {
                if !self.cell.is_initialized(guard.token()) {
                    let _ = self
                        .cell
                        .get_or_pin_init(RecurringCallback::init(self.func), guard.token_mut());
                }
                self.cell.get_unchecked().unwrap()
            }
        };
        cb.toggle();
    }
}
