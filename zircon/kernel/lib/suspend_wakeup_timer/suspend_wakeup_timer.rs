// Copyright 2025 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

mod suspend_wakeup_timer_ffi;

use crate::arch_rs::curr_cpu_num;
use crate::kernel::relaxed_atomic::RelaxedAtomicI64;
use crate::kernel::timer::{Timer, ZX_CLOCK_BOOT};
use crate::kernel::types::BOOT_CPU_ID;
use crate::platform_rs::timer::InstantBoot;
use core::convert::Infallible;
use core::ffi::c_void;
use core::pin::Pin;
use debug::dprintf;
use ksync::{KMutex, RawSpinlock, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_types::{ZX_TIME_INFINITE, zx_instant_boot_t};

#[cfg(target_arch = "aarch64")]
use self::suspend_wakeup_timer_ffi::{Armv7MmioTimer, Armv7MmioTimerSubtimer, IrqHandlerResult};
#[cfg(target_arch = "aarch64")]
use crate::kernel::types::cpu_num_to_mask;
#[cfg(target_arch = "aarch64")]
use crate::platform_rs::timer::{DurationBoot, current_boot_time};
#[cfg(target_arch = "aarch64")]
use zx_status::Status;

/// Inline storage capacity in bytes for [`Callback`], sufficient to hold a single Rust
/// context pointer or a C++ `SuspendWakeupTimer::Callback` (`fit::inline_function`).
pub const CALLBACK_INLINE_SIZE: usize = 2 * core::mem::size_of::<usize>();

/// Callback invoked when a [`SuspendWakeupTimer`] fires.
///
/// The [`Callback`] has inline storage for a context pointer, allowing
/// it to easily target either a static function or an instanced method of a
/// type. It takes two parameters, `now` and `resume_at`, both timestamps on
/// the boot timeline which inform the callback owner of when the interrupt
/// fired, as well as what the originally requested resume time was.
pub type Callback =
    zr::SendSyncInlineFn<'static, fn(InstantBoot, InstantBoot), CALLBACK_INLINE_SIZE>;

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy)]
struct Armv7MmioSuspendWakeupTimer {
    timer: &'static Armv7MmioTimerSubtimer,
}

#[cfg(target_arch = "aarch64")]
impl Armv7MmioSuspendWakeupTimer {
    fn irq_handler(timer: &SuspendWakeupTimer) -> IrqHandlerResult {
        timer.do_callback(current_boot_time());
        IrqHandlerResult::RemainRegistered
    }

    fn find_supported() -> Option<Self> {
        // Search through the driver for a v7 MMIO which can use the VCT reference.
        // Failing that, try to fall back on which can use PCT.
        const TIMER_TYPES: [Armv7MmioTimerType; 2] =
            [Armv7MmioTimerType::Vct, Armv7MmioTimerType::Pct];

        for timer_type in TIMER_TYPES {
            for i in 0..Armv7MmioTimer::MAX_TIMERS {
                // SAFETY: `SuspendWakeupTimer::init` is only called at or after
                // `LK_INIT_LEVEL_PLATFORM`, after `Armv7MmioTimer` initialization has completed.
                if let Some(timer_block) = unsafe { Armv7MmioTimer::get(i) } {
                    let timer = if timer_type == Armv7MmioTimerType::Vct {
                        timer_block.vct_timer()
                    } else {
                        timer_block.pct_timer()
                    };

                    if timer.supported() {
                        dprintf!(
                            INFO,
                            "Using ARMv7 {} timer #{} for suspend-wakeup.\n",
                            if timer_type == Armv7MmioTimerType::Vct { "VCT" } else { "PCT" },
                            i
                        );
                        return Some(Self { timer });
                    }
                }
            }
        }
        None
    }
}

#[cfg(target_arch = "aarch64")]
#[derive(Clone, Copy, PartialEq, Eq)]
enum Armv7MmioTimerType {
    Vct,
    Pct,
}

/// Hardware timer backend selected by a [`SuspendWakeupTimer`].
///
/// The generic backend's [`Timer`] storage lives in [`SuspendWakeupTimer::generic_timer`] so
/// that it can be pin-initialized with `pin_init!`; this enum only records which backend drives
/// the wakeup.
#[derive(Clone, Copy)]
enum Backend {
    /// The generic kernel [`Timer`] on the boot timeline.
    Generic,
    /// A platform ARMv7 MMIO timer.
    #[cfg(target_arch = "aarch64")]
    Armv7Mmio(Armv7MmioSuspendWakeupTimer),
}

impl Backend {
    fn preferred() -> Self {
        #[cfg(target_arch = "aarch64")]
        if let Some(armv7_mmio) = Armv7MmioSuspendWakeupTimer::find_supported() {
            return Self::Armv7Mmio(armv7_mmio);
        }

        dprintf!(INFO, "Using Generic timer for suspend-wakeup.\n");
        Self::Generic
    }
}

/// # SuspendWakeupTimer
///
/// ## Summary
///
/// A type which helps to abstract the specific timer HW used to wake the system
/// from a suspended state. Currently meant to be used only by the
/// `IdlePowerThread`.
///
/// ## Creation
/// Users of the [`SuspendWakeupTimer`] start by creating a timer instance using the
/// [`SuspendWakeupTimer::init`] initializer. During initialization, a [`Callback`] instance
/// must be passed.
///
/// The [`Callback`] has enough storage for a single context pointer, allowing it to
/// easily target either a static or instanced method of a type. It takes two
/// parameters, `now` and `resume_at`, both timestamps on the boot timeline which
/// inform the callback owner of when the interrupt fired, as well as what the
/// originally requested resume time was.
///
/// [`SuspendWakeupTimer::init`] must not be called earlier than `LK_INIT_LEVEL_PLATFORM` to
/// allow platform specific wakeup timer hardware to be detected and initialized. Callbacks will
/// always take place on the `BOOT_CPU` and with interrupts off.
///
/// ## Usage
///
/// Usage of the [`SuspendWakeupTimer`] involves three methods called in accordance
/// with a protocol which must be followed to ensure correct behavior. The
/// methods are:
///
/// + [`SuspendWakeupTimer::set_resume_deadline`]
/// + [`SuspendWakeupTimer::cancel_timer`]
/// + [`SuspendWakeupTimer::ensure_started`]
///
/// At the start of a suspend operation, if the operation has a resume deadline,
/// [`SuspendWakeupTimer::set_resume_deadline`] must be called to configure the desired deadline
/// before instructing the `BOOT_CPU`'s `IdlePowerThread` to transition to the suspended
/// state. Once a resume deadline has been configured, it is illegal to call
/// [`SuspendWakeupTimer::set_resume_deadline`] again without first explicitly canceling the timer
/// via [`SuspendWakeupTimer::cancel_timer`].
///
/// After the resume deadline has been configured, and as the suspend coordinator
/// thread unwinds from the suspend operation for any reason (resume timer fired,
/// some other wake source fired, failure to enter suspend due to an unrelated
/// error), [`SuspendWakeupTimer::cancel_timer`] *must* be called in order to reset the timer, and
/// guarantee that there are no timer callbacks in flight. [`SuspendWakeupTimer::cancel_timer`] is
/// idempotent. Unlike [`SuspendWakeupTimer::set_resume_deadline`], it may be called any number of
/// times in a row. Once [`SuspendWakeupTimer::cancel_timer`] has been called, it is guaranteed
/// that there are no longer any callbacks in flight, however it does *not* guarantee that the
/// user won the race to cancel a timer which had been set up, but may not have
/// fired yet.
///
/// Finally, in the `BOOT_CPU`'s `IdlePowerThread` itself, when the system is
/// supposed to be in the suspend state, it *must* call [`SuspendWakeupTimer::ensure_started`] to
/// make certain that the hardware is properly set up to deliver an interrupt which
/// will wake the system at an appropriate time. This call *must* be made from
/// the `BOOT_CPU` and interrupts *must* be disabled when this happens. Similar to
/// [`SuspendWakeupTimer::cancel_timer`], [`SuspendWakeupTimer::ensure_started`] is idempotent and
/// may be called multiple times without a problem. If there is a configured deadline and the
/// timer is not yet started, it will be.
///
/// So, the summary of the usage protocol is as follows.
///
/// 1) At the start of a suspend operation call [`SuspendWakeupTimer::set_resume_deadline`] to
///    configure the time to wake up and resume the system.
/// 2) In the `BOOT_CPU`'s `IdlePowerThread` itself, just before entering a suspended
///    state and with interrupts still disabled, call [`SuspendWakeupTimer::ensure_started`] to
///    make sure that the timer is running if it needs to be.
/// 3) After control is returned to the coordinator thread, always call
///    [`SuspendWakeupTimer::cancel_timer`] as we unwind.
///
/// ## Restrictions
///
/// [`SuspendWakeupTimer`] callbacks will take place at hard IRQ time. Operations
/// which may need to block (such as holding a `Mutex`) are not allowed. That
/// said, no spinlocks are held during the callback itself. It is safe for the
/// callback to call methods on the [`SuspendWakeupTimer`] instance itself, however
/// it is anticipated that there is little to no reason to do so. Keep in mind,
/// however, that it is illegal to call [`SuspendWakeupTimer::set_resume_deadline`] a second time
/// without having canceled the timer first. If a callback _wanted_ to re-program its
/// deadline during the operation itself, [`SuspendWakeupTimer::cancel_timer`] must still be called
/// first to ensure that the timer is ready to be set up again.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(align(8))]
pub struct SuspendWakeupTimer {
    #[mutex]
    lock: KMutex<RawSpinlock>,
    #[guarded_by(lock)]
    started: bool,
    resume_at: RelaxedAtomicI64,
    callback: Callback,
    backend: Backend,
    /// Storage for the [`Backend::Generic`] timer. Left unscheduled by other backends.
    #[guarded_by(lock)]
    #[pin]
    generic_timer: Timer,
}

const EXPECTED_SPINLOCK_SIZE: usize = if cfg!(scheduler_lock_spin_tracing_enabled) {
    24
} else if cfg!(with_lock_dep) {
    16
} else {
    8
};
/// [`Backend`] is zero-sized unless the [`Backend::Armv7Mmio`] variant exists.
const EXPECTED_BACKEND_SIZE: usize = if cfg!(target_arch = "aarch64") { 8 } else { 0 };
const EXPECTED_SUSPEND_WAKEUP_TIMER_SIZE: usize =
    EXPECTED_SPINLOCK_SIZE + EXPECTED_BACKEND_SIZE + 128;

zr::static_assert!(core::mem::size_of::<Backend>() == EXPECTED_BACKEND_SIZE);
zr::static_assert_size_and_align!(SuspendWakeupTimer, EXPECTED_SUSPEND_WAKEUP_TIMER_SIZE, 8);

#[pinned_drop]
impl PinnedDrop for SuspendWakeupTimer {
    fn drop(self: Pin<&mut Self>) {
        match self.backend {
            // `generic_timer` is dropped in place, which asserts that it is no longer scheduled.
            Backend::Generic => {}
            #[cfg(target_arch = "aarch64")]
            Backend::Armv7Mmio(armv7_mmio) => {
                let _ = armv7_mmio.timer.reset_handler();
            }
        }
    }
}

impl SuspendWakeupTimer {
    /// Kernel timer callback trampoline for the generic [`Timer`] backend.
    ///
    /// # Safety
    ///
    /// `thiz` must be a valid pointer to a live [`SuspendWakeupTimer`].
    unsafe extern "C" fn generic_timer_handler(
        _timer: *mut Timer,
        now: zx_instant_boot_t,
        thiz: *mut c_void,
    ) {
        // SAFETY: `thiz` was passed as `self` in `ensure_started` and remains valid
        // until `cancel_timer` completes.
        unsafe { (&*thiz.cast::<Self>()).do_callback(InstantBoot(now)) };
    }

    fn init_with_backend(callback: Callback, backend: Backend) -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- KMutex::init(),
            started: false.into(),
            resume_at: RelaxedAtomicI64::new(ZX_TIME_INFINITE),
            callback,
            backend,
            generic_timer <- ksync::kcell_init(Timer::init(ZX_CLOCK_BOOT)),
        })
        .pin_chain(|this| {
            match this.backend {
                Backend::Generic => {}
                #[cfg(target_arch = "aarch64")]
                Backend::Armv7Mmio(armv7_mmio) => {
                    // Make certain that our timer is canceled, then register our handler.
                    let status: Result<(), Status> = armv7_mmio.timer.cancel_timer();
                    assert_eq!(status, Ok(()));

                    // SAFETY: `this` is pinned in place, and `SuspendWakeupTimer`'s `PinnedDrop`
                    // implementation calls `timer.reset_handler()` before the storage is
                    // invalidated.
                    let status = unsafe {
                        armv7_mmio.timer.set_handler(this.as_ref(), cpu_num_to_mask(BOOT_CPU_ID))
                    };
                    assert_eq!(status, Ok(()));
                }
            }
            Ok(())
        })
    }

    #[cfg(ktest)]
    fn init_generic(callback: Callback) -> impl PinInit<Self, Infallible> {
        Self::init_with_backend(callback, Backend::Generic)
    }

    /// Creates an in-place initializer for a [`SuspendWakeupTimer`] using the platform's
    /// preferred wakeup timer hardware.
    pub fn init(callback: Callback) -> impl PinInit<Self, Infallible> {
        Self::init_with_backend(callback, Backend::preferred())
    }

    /// Sets the time at which the [`SuspendWakeupTimer`] should call the user's
    /// callback in order to wake the system from suspend. Note that once this has
    /// been called and the timer has an assigned deadline, it is illegal to call
    /// this method again without first calling [`Self::cancel_timer`].
    pub fn set_resume_deadline(&self, resume_at: InstantBoot) {
        ksync::lock!(let timer_guard = self.lock_lock());
        debug_assert!(!*timer_guard.started());
        debug_assert_eq!(self.resume_at.load(), ZX_TIME_INFINITE);
        self.resume_at.store(resume_at.0);
    }

    /// Called by the `BOOT_CPU`'s `IdlePowerThread` every time it is just about to
    /// enter a suspended state to ensure that its [`SuspendWakeupTimer`] is started
    /// and will wake it up at the appropriate time.
    pub fn ensure_started(&self) {
        ksync::lock!(let mut timer_guard = self.lock_lock());
        if *timer_guard.started() || self.resume_at.load() == ZX_TIME_INFINITE {
            return;
        }

        match self.backend {
            Backend::Generic => {
                debug_assert_eq!(curr_cpu_num(), BOOT_CPU_ID);
                // SAFETY: `Self::generic_timer_handler` is a valid callback and `self` points to
                // this `SuspendWakeupTimer`, which remains alive while the timer is scheduled.
                unsafe {
                    timer_guard.as_mut().generic_timer_mut().as_ref().set_oneshot(
                        self.resume_at.load(),
                        Self::generic_timer_handler,
                        core::ptr::from_ref(self).cast_mut().cast::<c_void>(),
                    );
                }
            }
            #[cfg(target_arch = "aarch64")]
            Backend::Armv7Mmio(armv7_mmio) => {
                debug_assert!(!armv7_mmio.timer.enabled());
                let resume_delta = DurationBoot(
                    0.max(self.resume_at.load().saturating_sub(current_boot_time().0)),
                );
                let _ = armv7_mmio.timer.set_relative_timer(resume_delta);
            }
        }

        *timer_guard.as_mut().started_mut() = true;
    }

    /// Called to cancel any pending timer and its callback. Must be called at
    /// least once after every call to [`Self::set_resume_deadline`] before a second call to
    /// [`Self::set_resume_deadline`] may be made. After returning from [`Self::cancel_timer`], the
    /// user is guaranteed that the registered callback is no longer in flight.
    /// It has either completed, or was successfully canceled.
    pub fn cancel_timer(&self) {
        ksync::lock!(let mut timer_guard = self.lock_lock());

        match self.backend {
            Backend::Generic => {
                timer_guard.generic_timer().cancel();
            }
            #[cfg(target_arch = "aarch64")]
            Backend::Armv7Mmio(armv7_mmio) => {
                let _ = armv7_mmio.timer.cancel_timer();
            }
        }

        self.reset_locked(timer_guard.as_mut());
    }

    fn reset_locked(&self, mut guard: Pin<&mut SuspendWakeupTimerLockGuard<'_>>) {
        *guard.as_mut().started_mut() = false;
        self.resume_at.store(ZX_TIME_INFINITE);
    }

    fn do_callback(&self, now: InstantBoot) {
        // Don't hold any locks when calling our callback.  Our underlying timers
        // don't hold any locks, and if we don't hold any locks either, it becomes
        // safe for the callback to call its own timer instance's methods while
        // executing.
        //
        // But, why is it safe for us to read `resume_at` then?  What is to prevent
        // a callback from running on the boot CPU and attempting to read
        // `resume_at`, while at the same time someone is calling cancel which is
        // resetting `resume_at` back to `ZX_TIME_INFINITE`?  Isn't a formal data race?
        //
        // Turns out, the answer is "no", but the reason why is not all that
        // obvious.  There are 4 operations we need to consider, the 3 public
        // interface methods and the callback itself.
        //
        // The 3 public interface methods all hold the timer's spinlock as they read
        // and write.  The mutually exclusive nature of the lock guarantees that
        // there are no formal races, and that the value observed will always be
        // "correct" for the internal state of the timer.  So, this leaves the
        // callback itself, and its interaction with the 3 public methods, to
        // consider.
        //
        // + `cancel_timer`: Cancel operations will mutate the `resume_at` member of the
        //   timer, but before doing so, they will call "cancel" on their underlying
        //   timer.  All existing low level timer implementations guarantee that,
        //   "after cancel is called, any callback in flight has been successfully
        //   canceled and will not run --or--, the callback has already run to
        //   completion.  So, if the callback is either finished or never will be
        //   run, and then `resume_at` is mutated, there is no possibility that the
        //   value can be written (from the method) while concurrently being read by
        //   the callback.  So, no formal race.
        //
        // + `set_resume_deadline`: It is illegal for a user to set a new resume
        //   deadline without first having called `cancel_timer`.  `set_resume_deadline`
        //   will obtain `lock`, synchronizing it with any `cancel_timer` operations,
        //   before checking the value in `resume_at`, and will ASSERT if the timer
        //   has not been first canceled.  So, if there is a timer callback in
        //   flight, cancel has not been called, and the system will panic (because
        //   of the protocol violation).  If cancel _has_ been called, then there is
        //   no timer in flight, and a callback cannot become in-fight for the
        //   duration of the Set operation (because holding `lock` prevents
        //   `ensure_started` from being called concurrently).
        //
        // + `ensure_started`: Holds the lock, but never mutates `resume_at`, meaning
        //   that there is no possibility of a formal race.
        //
        // Please note: even though there is no chance of a formal race here, we
        // still declare `resume_at` as a `RelaxedAtomicI64`.  Why?  Because there is no
        // real penalty to be paid for relaxed atomic access to variables on the
        // architectures we support, and it makes it easier for a reader to know
        // that there is no chance of a formal data race by simply observing the
        // atomic nature of the member, without having to understand the more
        // complex chain of reasoning given above.
        //
        self.callback.call(now, InstantBoot(self.resume_at.load()));
    }
}

/// Kernel unit tests for [`SuspendWakeupTimer`].
#[cfg(ktest)]
#[unittest::suite(name = "suspend_wakeup_timer_rust")]
mod tests {
    use super::{Callback, SuspendWakeupTimer};
    use crate::kernel::mp::{MpIpiTarget, cpu_num_to_mask, sync_exec};
    use crate::kernel::thread::sleep_relative;
    use crate::kernel::types::BOOT_CPU_ID;
    use crate::platform_rs::timer::{DurationMono, InstantBoot, current_boot_time};
    use core::sync::atomic::{AtomicI64, AtomicU32, Ordering};
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_ge, expect_true};

    struct TestCallbackState {
        call_count: AtomicU32,
        observed_now: AtomicI64,
        observed_resume_at: AtomicI64,
    }

    impl TestCallbackState {
        const fn new() -> Self {
            Self {
                call_count: AtomicU32::new(0),
                observed_now: AtomicI64::new(0),
                observed_resume_at: AtomicI64::new(0),
            }
        }

        /// Resets `self` and returns a [`Callback`] referencing it.
        fn make_callback(&'static self) -> Callback {
            self.call_count.store(0, Ordering::Relaxed);
            self.observed_now.store(0, Ordering::Relaxed);
            self.observed_resume_at.store(0, Ordering::Relaxed);
            Callback::new(move |now: InstantBoot, resume_at: InstantBoot| {
                self.observed_now.store(now.0, Ordering::Release);
                self.observed_resume_at.store(resume_at.0, Ordering::Release);
                self.call_count.fetch_add(1, Ordering::Release);
            })
        }
    }

    /// Runs `timer.ensure_started()` on `BOOT_CPU_ID` with interrupts disabled, matching the
    /// protocol requirements of `SuspendWakeupTimer::ensure_started`.
    fn ensure_started_on_boot_cpu(timer: &SuspendWakeupTimer) {
        sync_exec(MpIpiTarget::Mask, cpu_num_to_mask(BOOT_CPU_ID), || {
            timer.ensure_started();
        });
    }

    /// Verifies that `cancel_timer` is idempotent and resets the deadline.
    #[test]
    fn test_cancel_is_idempotent() {
        static STATE: TestCallbackState = TestCallbackState::new();
        stack_pin_init!(let timer = SuspendWakeupTimer::init_generic(STATE.make_callback()));

        // Calling cancel_timer before any deadline is set is a valid no-op.
        timer.cancel_timer();
        timer.cancel_timer();

        // Set a deadline far in the future, start it on BOOT_CPU, then cancel it multiple times.
        let now = current_boot_time();
        let future_deadline = InstantBoot(now.0 + 10_000_000_000);
        timer.set_resume_deadline(future_deadline);
        ensure_started_on_boot_cpu(&timer);
        // Calling ensure_started a second time is idempotent.
        ensure_started_on_boot_cpu(&timer);

        timer.cancel_timer();
        timer.cancel_timer();

        // After canceling, setting a new deadline must succeed without asserting.
        let next_deadline = InstantBoot(now.0 + 20_000_000_000);
        timer.set_resume_deadline(next_deadline);
        timer.cancel_timer();

        expect_eq!(STATE.call_count.load(Ordering::Acquire), 0);
    }

    /// Verifies that `ensure_started` without a configured deadline does not start the timer.
    #[test]
    fn test_ensure_started_without_deadline() {
        static STATE: TestCallbackState = TestCallbackState::new();
        stack_pin_init!(let timer = SuspendWakeupTimer::init_generic(STATE.make_callback()));

        ensure_started_on_boot_cpu(&timer);
        let _ = sleep_relative(DurationMono::from_millis(1));
        timer.cancel_timer();

        expect_eq!(STATE.call_count.load(Ordering::Acquire), 0);
    }

    /// Verifies that a configured wakeup timer fires and can be re-armed after `cancel_timer`.
    #[test]
    fn test_timer_fires_and_rearms() {
        static STATE: TestCallbackState = TestCallbackState::new();
        stack_pin_init!(let timer = SuspendWakeupTimer::init_generic(STATE.make_callback()));

        for expected_count in 1..=2 {
            let now = current_boot_time();
            let deadline = InstantBoot(now.0 + 1_000_000);
            timer.set_resume_deadline(deadline);
            ensure_started_on_boot_cpu(&timer);

            let mut fired = false;
            for _ in 0..200 {
                if STATE.call_count.load(Ordering::Acquire) == expected_count {
                    fired = true;
                    break;
                }
                let _ = sleep_relative(DurationMono::from_millis(1));
            }

            expect_true!(fired);
            expect_eq!(STATE.observed_resume_at.load(Ordering::Acquire), deadline.0);
            expect_ge!(STATE.observed_now.load(Ordering::Acquire), deadline.0);

            // Must cancel before setting the next deadline.
            timer.cancel_timer();
        }
    }
}
